//! The one owner of Parquet and Arrow IPC reader construction (#779).
//!
//! Every Parquet file tylertoo opens — a GeoParquet input (local or remote),
//! an overview file handed to `export-pmtiles` or `validate`, the footer of
//! its own output — and every Arrow IPC file it reads back (a `--plan`) is
//! opened here and nowhere else. Before this module existed 15 production
//! sites (and some 40 more in tests) opened Parquet directly, through
//! `ParquetRecordBatchReaderBuilder`, `SerializedFileReader` or
//! `ArrowReaderMetadata::load`, and each hardening fix
//! reached only some of them: the arrow i32 offset cap landed in 2 of 4 read
//! paths (#563 → #577, then #606, #607, #657, #659), and the checks added for
//! crafted input ran after the decode while the panics happened inside it
//! (#683, #684, #687, #690).
//!
//! What the owner guarantees, in one place:
//!
//! - **A footer pre-check** before parquet-rs decodes a footer (#690):
//!   [`precheck_footer`] walks the thrift bytes and refuses any list, map or
//!   schema child count larger than the bytes or elements that could follow
//!   it, so no footer-declared count sizes an allocation.
//! - **Column chunk ranges inside the file** (#430), for local and remote
//!   inputs alike: parquet's own `byte_range()` asserts on a negative offset,
//!   and every later `start + len` trusts these numbers.
//! - **A panic boundary** around every upstream decode — the footer, the
//!   embedded `ARROW:schema` (#683), each data page (#687) and each IPC
//!   message (#684). A panic becomes [`InputError::DecoderPanic`], carrying
//!   the upstream message and location. See `guard` for why that is sound.
//! - **One batch-size ceiling** (#563): [`ParquetSource::build`] plans the
//!   initial decode size from footer statistics, decodes byte views, and
//!   splits before expanding payloads. Both total decoded bytes and i32
//!   offsets are bounded. [`byte_array_limit`] is fixed in production; the
//!   `testing` feature enables the isolated scaled-limit lane.
//! - **IPC blocks inside the file**: [`open_ipc`] refuses a footer whose
//!   dictionary or record-batch block ends past the file, before arrow-ipc
//!   zero-allocates the block's declared length.
//! - **One place to set the projection, row groups and batch size**:
//!   [`ParquetSource`].
//!
//! The workspace dependency policy keeps these builders out of core.
//! This private module is the reader construction owner.

use std::cell::{Cell, RefCell};
use std::io::{Read, Seek, SeekFrom};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_ipc::reader::FileReader;
use arrow_schema::{ArrowError, SchemaRef};
use bytes::Bytes;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReader,
    ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::ProjectionMask;
use parquet::errors::ParquetError;
use parquet::file::metadata::{FooterTail, ParquetMetaData, ParquetMetaDataReader};
use parquet::file::reader::{ChunkReader, Length};
use parquet::schema::types::SchemaDescriptor;

use crate::Error as InputError;

// ============================================================================
// Panic boundary
// ============================================================================

thread_local! {
    /// How many [`guard`]s this thread is inside. The panic hook stays quiet
    /// while it is non-zero: the panic is about to become an error.
    static BOUNDARY_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// The message and location of the last panic caught inside a boundary,
    /// recorded by the hook (which sees the location; `catch_unwind` does
    /// not).
    static CAUGHT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Install, once per process, a panic hook that records a panic raised
/// inside a [`guard`] instead of printing it, and hands every other panic to
/// the hook that was installed before it.
///
/// Recording rather than printing keeps a crafted input from spraying a
/// backtrace over stderr ahead of the clean error. It also matters under
/// libFuzzer, whose hook aborts on every panic, caught or not: the fuzz
/// targets install theirs before the first input, this one wraps it, and a
/// panic inside the boundary then unwinds to the boundary as it does in a
/// normal build. A panic anywhere else still reaches libFuzzer and aborts.
///
/// Our own reader code (the remote chunk cache, its spill, `InputReader`)
/// runs inside the boundary too, so a caught panic may be a tylertoo bug.
/// With `RUST_BACKTRACE` set, the hook still prints the panic and its
/// backtrace to stderr, as the default hook would.
fn install_quiet_hook() {
    #[expect(
        clippy::disallowed_types,
        reason = "#779: installs the panic boundary hook once; no warning counters"
    )]
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if BOUNDARY_DEPTH.with(Cell::get) > 0 {
                let at = info
                    .location()
                    .map(|l| format!(" (at {}:{})", short_path(l.file()), l.line()))
                    .unwrap_or_default();
                let message = format!("{}{at}", panic_message(info.payload()));
                // `capture` honours RUST_BACKTRACE / RUST_LIB_BACKTRACE.
                let backtrace = std::backtrace::Backtrace::capture();
                if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
                    eprintln!(
                        "tylertoo: caught a panic inside the input boundary: {message}\n{backtrace}"
                    );
                }
                CAUGHT.with(|c| *c.borrow_mut() = Some(message));
            } else {
                previous(info);
            }
        }));
    });
}

/// A panic payload's message, when it is a string.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_string())
}

/// `file` from the crate directory on (`arrow-ipc-59.3.1/src/convert.rs`),
/// without the cargo registry prefix.
fn short_path(file: &str) -> &str {
    let parts: Vec<&str> = file.split(['/', '\\']).collect();
    let crate_dir = parts.iter().rposition(|p| {
        p.rsplit_once('-')
            .is_some_and(|(_, v)| v.starts_with(|c: char| c.is_ascii_digit()))
    });
    match crate_dir {
        Some(i) => {
            let skip: usize = parts[..i].iter().map(|p| p.len() + 1).sum();
            &file[skip.min(file.len())..]
        }
        None => file,
    }
}

/// Run `f`, an upstream decode of untrusted bytes, turning a panic inside it
/// into [`InputError::DecoderPanic`] naming `what` was being decoded.
///
/// # Why `AssertUnwindSafe` is sound here
///
/// `f` borrows decoder state — a reader builder, a record batch reader, a
/// footer buffer — that a panic may leave half-updated. Every caller drops
/// that state, or marks it finished and never polls it again, as soon as
/// `f` has panicked ([`UntrustedBatchReader`] and [`UntrustedIpcReader`] set
/// `done`), so a broken invariant can never be observed afterwards. That is
/// the property `UnwindSafe` approximates; the assertion states it.
///
/// The one shared state a decode can reach is a remote input's chunk cache
/// and its spill, shared by every reader of that input. Their mutexes are
/// held only inside our own `get_bytes`, never across an upstream decode,
/// so an upstream panic cannot poison them. A panic from our own code while
/// one is held could, so every lock on them recovers a poisoned guard
/// (`PoisonError::into_inner`) instead of panicking. Each critical section
/// inserts, evicts or reads whole entries, so a panic in one leaves at worst
/// a stale byte total, never an entry that reads back wrong, and another
/// reader sharing the cache carries on.
///
/// # Errors
///
/// Returns [`InputError::DecoderPanic`] if `f` panicked.
pub(crate) fn guard<T>(what: &'static str, f: impl FnOnce() -> T) -> Result<T, InputError> {
    install_quiet_hook();
    BOUNDARY_DEPTH.with(|d| d.set(d.get() + 1));
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    BOUNDARY_DEPTH.with(|d| d.set(d.get() - 1));
    result.map_err(|payload| {
        let message = CAUGHT
            .with(|c| c.borrow_mut().take())
            .unwrap_or_else(|| panic_message(payload.as_ref()));
        log::debug!("caught a panic while decoding the {what}: {message}");
        InputError::DecoderPanic { what, message }
    })
}

/// [`guard`] around a fallible Parquet decode, flattened into its error
/// type: the panic travels as [`ParquetError::External`], so every caller's
/// existing footer and read error mapping carries it unchanged.
fn guard_parquet<T>(
    what: &'static str,
    f: impl FnOnce() -> Result<T, ParquetError>,
) -> Result<T, ParquetError> {
    guard(what, f).unwrap_or_else(|e| Err(ParquetError::External(Box::new(e))))
}

/// [`guard_parquet`] for an Arrow decode.
pub(super) fn guard_arrow<T>(
    what: &'static str,
    f: impl FnOnce() -> Result<T, ArrowError>,
) -> Result<T, ArrowError> {
    guard(what, f).unwrap_or_else(|e| Err(ArrowError::ExternalError(Box::new(e))))
}

/// The [`InputError`] inside `error`, when the owner put one there (a
/// decoder panic or a refused footer).
pub fn input_error<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a InputError> {
    let mut current = Some(error);
    while let Some(e) = current {
        if let Some(found) = e.downcast_ref::<InputError>() {
            return Some(found);
        }
        if let Some(ParquetError::External(inner)) = e.downcast_ref::<ParquetError>() {
            return input_error(inner.as_ref());
        }
        if let Some(ArrowError::ExternalError(inner)) = e.downcast_ref::<ArrowError>() {
            return input_error(inner.as_ref());
        }
        current = e.source();
    }
    None
}

// ============================================================================
// Footer pre-check (#690)
// ============================================================================

/// Deepest struct/list nesting a footer may use. Real footers nest under
/// ten levels (file → row group → column chunk → metadata → statistics);
/// past this the bytes are an attack on a recursive decoder's stack.
const MAX_FOOTER_DEPTH: u32 = 64;

/// Why [`Skim`] stopped.
enum SkimStop {
    /// A declared count no honest footer could hold: refuse the footer.
    Hostile(String),
    /// The bytes stopped making sense (truncated, unknown type). Not ours to
    /// judge: parquet-rs decodes the footer and reports its own error.
    Confused,
}

/// The fewest bytes one collection element of compact type `ty` takes on
/// the wire: a double is 8, everything else (a varint, a length-prefixed
/// binary, a nested collection's header, a struct's stop byte) at least 1.
fn min_wire_bytes(ty: u8) -> u64 {
    if ty == 7 {
        8
    } else {
        1
    }
}

/// A forward-only reader of thrift compact-protocol bytes that skips values
/// without allocating, checking every declared count against what remains.
struct Skim<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Skim<'_> {
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn byte(&mut self) -> Result<u8, SkimStop> {
        let b = *self.buf.get(self.pos).ok_or(SkimStop::Confused)?;
        self.pos += 1;
        Ok(b)
    }

    fn advance(&mut self, n: usize) -> Result<(), SkimStop> {
        if n > self.remaining() {
            return Err(SkimStop::Confused);
        }
        self.pos += n;
        Ok(())
    }

    fn varint(&mut self) -> Result<u64, SkimStop> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.byte()?;
            if shift == 63 && b > 1 {
                return Err(SkimStop::Confused);
            }
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(SkimStop::Confused)
    }

    fn zigzag(&mut self) -> Result<i64, SkimStop> {
        let n = self.varint()?;
        Ok((n >> 1) as i64 ^ -((n & 1) as i64))
    }

    /// A list or set header: `(element type, declared size)`. Every element
    /// takes at least one byte, so a size past the remaining bytes is a lie
    /// told to size an allocation.
    fn list_header(&mut self) -> Result<(u8, usize), SkimStop> {
        let b = self.byte()?;
        let ty = b & 0x0f;
        let size = match b >> 4 {
            15 => self.varint()?,
            n => u64::from(n),
        };
        self.check_count(size, min_wire_bytes(ty), "list")?;
        Ok((ty, size as usize))
    }

    /// Refuse `size` elements of at least `each` bytes apiece when fewer
    /// bytes than that remain.
    fn check_count(&self, size: u64, each: u64, what: &str) -> Result<(), SkimStop> {
        if size.saturating_mul(each) > self.remaining() as u64 {
            return Err(SkimStop::Hostile(format!(
                "a {what} in the footer declares {size} elements of at least {each} byte(s), \
                 but only {} bytes follow it",
                self.remaining()
            )));
        }
        Ok(())
    }

    /// Refuse nesting deeper than [`MAX_FOOTER_DEPTH`].
    fn check_depth(depth: u32) -> Result<(), SkimStop> {
        if depth > MAX_FOOTER_DEPTH {
            return Err(SkimStop::Hostile(format!(
                "the footer nests values more than {MAX_FOOTER_DEPTH} levels deep"
            )));
        }
        Ok(())
    }

    /// Skip `size` collection elements of compact type `elem`. A boolean
    /// element is one byte in a collection, not a type nibble.
    fn skip_elements(&mut self, elem: u8, size: usize, depth: u32) -> Result<(), SkimStop> {
        let elem = if matches!(elem, 1 | 2) { 3 } else { elem };
        (0..size).try_for_each(|_| self.skip(elem, depth))
    }

    /// Skip one value of compact type `ty`.
    fn skip(&mut self, ty: u8, depth: u32) -> Result<(), SkimStop> {
        Self::check_depth(depth)?;
        match ty {
            // Booleans: the value is the type nibble of a field; in a
            // collection it is one byte (handled by the caller passing 3).
            1 | 2 => Ok(()),
            3 => self.advance(1),
            4..=6 => self.varint().map(drop),
            7 => self.advance(8),
            8 => {
                let len = self.varint()?;
                self.advance(usize::try_from(len).map_err(|_| SkimStop::Confused)?)
            }
            9 | 10 => {
                let (elem, size) = self.list_header()?;
                self.skip_elements(elem, size, depth + 1)
            }
            11 => self.skip_map(depth + 1),
            12 => self.fields(depth + 1, |s, _, ty| s.skip(ty, depth + 1)),
            _ => Err(SkimStop::Confused),
        }
    }

    /// Skip a map: its size, then (when non-empty) one key/value type byte
    /// and that many pairs.
    fn skip_map(&mut self, depth: u32) -> Result<(), SkimStop> {
        let size = self.varint()?;
        self.check_count(size, 2, "map")?;
        let kv = if size == 0 { 0 } else { self.byte()? };
        (0..size).try_for_each(|_| {
            self.skip_elements(kv >> 4, 1, depth)?;
            self.skip_elements(kv & 0x0f, 1, depth)
        })
    }

    /// Walk one struct's fields, handing each `(field id, type)` to `field`,
    /// which must consume the value.
    fn fields(
        &mut self,
        depth: u32,
        mut field: impl FnMut(&mut Self, i16, u8) -> Result<(), SkimStop>,
    ) -> Result<(), SkimStop> {
        Self::check_depth(depth)?;
        let mut last: i16 = 0;
        loop {
            let b = self.byte()?;
            if b == 0 {
                return Ok(());
            }
            let ty = b & 0x0f;
            let id = match b >> 4 {
                0 => i16::try_from(self.zigzag()?).map_err(|_| SkimStop::Confused)?,
                delta => last
                    .checked_add(i16::from(delta))
                    .ok_or(SkimStop::Confused)?,
            };
            last = id;
            field(self, id, ty)?;
        }
    }

    /// `FileMetaData.schema` (field 2): a list of `SchemaElement`s whose
    /// `num_children` (field 5) each must fit in the elements after it.
    fn schema(&mut self, depth: u32) -> Result<(), SkimStop> {
        let (elem, size) = self.list_header()?;
        if elem != 12 {
            return self.skip_elements(elem, size, depth + 1);
        }
        for index in 0..size {
            let after = size - index - 1;
            self.fields(depth + 1, |s, id, ty| {
                if id == 5 && ty == 5 {
                    let children = s.zigzag()?;
                    if children > after as i64 {
                        return Err(SkimStop::Hostile(format!(
                            "schema element {index} declares {children} children, but only \
                             {after} schema elements follow it"
                        )));
                    }
                    Ok(())
                } else {
                    s.skip(ty, depth + 2)
                }
            })?;
        }
        Ok(())
    }
}

/// Refuse a thrift `FileMetaData` footer whose declared counts could make
/// parquet-rs allocate for elements it does not hold (#690).
///
/// parquet-rs reserves `Vec::with_capacity(num_children)` for each schema
/// element before reading a child, so a 162-byte footer declaring 1.5
/// billion children asked for 12 GB (an abort under a memory limit or
/// libFuzzer's). This walks the footer once, allocating nothing, and checks
/// every list, set and map size against the bytes that remain, every schema
/// element's child count against the elements that follow, and the nesting
/// depth. Bytes it cannot make sense of are left to parquet-rs, which
/// reports its own error: the pre-check only ever refuses a count.
///
/// Each declared count is floored at its element type's smallest wire size
/// (`min_wire_bytes`), not its decoded size. A struct element can be one
/// byte on the wire (its stop byte) yet decode into a struct of a few
/// hundred bytes, so a footer of F bytes can still make parquet-rs reserve
/// about F × `size_of::<T>()`. That is linear in a footer the file actually
/// holds, so on the order of 100 MB for a 1 MB footer. It is not the
/// unbounded, footer-independent reservation of #690.
///
/// # Errors
///
/// Returns [`InputError::HostileFooter`] naming the offending count.
pub fn precheck_footer(thrift: &[u8]) -> Result<(), InputError> {
    let mut skim = Skim {
        buf: thrift,
        pos: 0,
    };
    let walked = skim.fields(0, |s, id, ty| {
        if id == 2 && ty == 9 {
            s.schema(1)
        } else {
            s.skip(ty, 1)
        }
    });
    match walked {
        Err(SkimStop::Hostile(reason)) => Err(InputError::HostileFooter(reason)),
        Ok(()) | Err(SkimStop::Confused) => Ok(()),
    }
}

/// Decode a thrift footer (the bytes before the length and `PAR1`) after
/// [`precheck_footer`], inside the panic boundary.
///
/// # Errors
///
/// Returns a [`ParquetError`] if the pre-check refuses the footer, the
/// decode fails, or the decoder panics.
#[expect(
    clippy::disallowed_methods,
    reason = "#779: footer decoder owner runs the precheck and panic boundary"
)]
pub fn decode_footer(thrift: &[u8]) -> Result<ParquetMetaData, ParquetError> {
    precheck_footer(thrift).map_err(|e| ParquetError::External(Box::new(e)))?;
    guard_parquet("parquet footer", || {
        ParquetMetaDataReader::decode_metadata(thrift)
    })
}

/// A [`ChunkReader`] that serves the file's tail (the footer and its
/// trailer) from bytes already fetched, and everything else from `inner`.
///
/// The pre-check has to read the footer before parquet-rs does; this hands
/// parquet-rs the same bytes, so a remote input pays the footer fetch once.
struct PrefetchedTail<'a, R> {
    inner: &'a R,
    /// File offset of `tail[0]`.
    at: u64,
    tail: Bytes,
}

impl<R: ChunkReader> Length for PrefetchedTail<'_, R> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
}

/// [`PrefetchedTail::get_read`]'s reader.
enum TailRead<T> {
    Inner(T),
    Tail(bytes::buf::Reader<Bytes>),
}

impl<T: Read> Read for TailRead<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            TailRead::Inner(r) => r.read(buf),
            TailRead::Tail(r) => r.read(buf),
        }
    }
}

impl<R: ChunkReader> ChunkReader for PrefetchedTail<'_, R> {
    type T = TailRead<R::T>;

    fn get_read(&self, start: u64) -> Result<Self::T, ParquetError> {
        if start >= self.at {
            let from = usize::try_from(start - self.at)
                .unwrap_or(usize::MAX)
                .min(self.tail.len());
            Ok(TailRead::Tail(bytes::Buf::reader(self.tail.slice(from..))))
        } else {
            self.inner.get_read(start).map(TailRead::Inner)
        }
    }

    fn get_bytes(&self, start: u64, length: usize) -> Result<Bytes, ParquetError> {
        let from = start
            .checked_sub(self.at)
            .and_then(|f| usize::try_from(f).ok());
        match from.and_then(|f| f.checked_add(length).map(|end| (f, end))) {
            Some((f, end)) if end <= self.tail.len() => Ok(self.tail.slice(f..end)),
            _ => self.inner.get_bytes(start, length),
        }
    }
}

/// Fetch `reader`'s footer and trailer, run [`precheck_footer`] on the
/// footer, and wrap the reader so parquet-rs reads the same bytes.
///
/// A file too short for a footer, with a bad magic, an encrypted footer or
/// a footer longer than the file is passed through unchecked: parquet-rs
/// rejects those itself, with its own messages.
fn prefetch_footer<R: ChunkReader>(reader: &R) -> Result<PrefetchedTail<'_, R>, ParquetError> {
    const TRAILER: usize = 8;
    let unchecked = || PrefetchedTail {
        inner: reader,
        at: u64::MAX,
        tail: Bytes::new(),
    };
    let size = reader.len();
    let Some(trailer_at) = size.checked_sub(TRAILER as u64) else {
        return Ok(unchecked());
    };
    let trailer = reader.get_bytes(trailer_at, TRAILER)?;
    let Ok(tail) = FooterTail::try_from(trailer.as_ref()) else {
        return Ok(unchecked());
    };
    let footer_len = tail.metadata_length() as u64;
    let Some(footer_at) = trailer_at.checked_sub(footer_len) else {
        return Ok(unchecked());
    };
    if tail.is_encrypted_footer() {
        return Ok(unchecked());
    }
    let footer = reader.get_bytes(footer_at, tail.metadata_length())?;
    precheck_footer(&footer).map_err(|e| ParquetError::External(Box::new(e)))?;
    let mut both = Vec::with_capacity(footer.len() + TRAILER);
    both.extend_from_slice(&footer);
    both.extend_from_slice(&trailer);
    Ok(PrefetchedTail {
        inner: reader,
        at: footer_at,
        tail: Bytes::from(both),
    })
}

/// Reject a footer whose column chunk byte ranges are negative, overflow or
/// run past the file (#430), before anything is built on them.
///
/// parquet's `byte_range()` asserts on a negative offset or length, and the
/// thrift decoder does not reject them, so the raw fields are checked
/// first. Every later `start + len` over these ranges (a remote input's
/// chunk map and staging spans, the page reader's slices) would otherwise
/// wrap in release or panic in debug.
///
/// # Errors
///
/// Returns [`ParquetError::EOF`] naming the first bad chunk.
pub(crate) fn validate_chunk_ranges(
    metadata: &ParquetMetaData,
    file_size: u64,
) -> Result<(), ParquetError> {
    for (rg_idx, rg) in metadata.row_groups().iter().enumerate() {
        for (col_idx, col) in rg.columns().iter().enumerate() {
            let raw_start = col
                .dictionary_page_offset()
                .unwrap_or_else(|| col.data_page_offset());
            let raw_len = col.compressed_size();
            if raw_start < 0 || raw_len < 0 {
                return Err(ParquetError::EOF(format!(
                    "column chunk (row group {rg_idx}, column {col_idx}) has a negative offset \
                     or length ({raw_start}, {raw_len})"
                )));
            }
            let (start, len) = col.byte_range();
            let end = start.checked_add(len).filter(|end| *end <= file_size);
            if end.is_none() || usize::try_from(len).is_err() {
                return Err(ParquetError::EOF(format!(
                    "column chunk (row group {rg_idx}, column {col_idx}) range {start}..{} \
                     beyond object size {file_size}",
                    u128::from(start) + u128::from(len),
                )));
            }
        }
    }
    Ok(())
}

/// Read and check `reader`'s Parquet footer, without deriving an Arrow
/// schema: for callers that need row-group sizes, statistics or key-value
/// metadata only.
///
/// # Errors
///
/// Returns a [`ParquetError`] if the footer cannot be read, the pre-check
/// refuses it, its chunk ranges are out of bounds, or the decoder panics.
#[expect(
    clippy::disallowed_methods,
    reason = "#779: metadata owner prechecks and validates chunk ranges"
)]
pub fn read_parquet_metadata<R: ChunkReader>(
    reader: &R,
) -> Result<Arc<ParquetMetaData>, ParquetError> {
    let prefetched = prefetch_footer(reader)?;
    let metadata = guard_parquet("parquet footer", || {
        ParquetMetaDataReader::new().parse_and_finish(&prefetched)
    })?;
    validate_chunk_ranges(&metadata, reader.len())?;
    Ok(Arc::new(metadata))
}

/// A checked footer and its decoded Arrow schema, reusable across read passes.
#[derive(Debug, Clone)]
pub struct ReaderMetadata(ArrowReaderMetadata);

impl ReaderMetadata {
    /// The checked footer, for row-group statistics and key-value metadata.
    pub fn metadata(&self) -> &Arc<ParquetMetaData> {
        self.0.metadata()
    }

    /// The decoded Arrow schema.
    pub fn schema(&self) -> &SchemaRef {
        self.0.schema()
    }

    /// The physical schema, for nested-column projection.
    pub fn parquet_schema(&self) -> &SchemaDescriptor {
        self.0.parquet_schema()
    }
}

/// Derive an Arrow schema from physical columns, ignoring embedded Arrow metadata.
///
/// # Errors
///
/// Returns an error if physical schema conversion fails or panics.
pub fn schema_without_metadata(
    metadata: &ParquetMetaData,
) -> Result<arrow_schema::Schema, ParquetError> {
    guard_parquet("physical Arrow schema", || {
        parquet::arrow::parquet_to_arrow_schema(metadata.file_metadata().schema_descr(), None)
    })
}

/// [`read_parquet_metadata`], plus the Arrow schema (decoded from the
/// footer's `ARROW:schema` key when present) inside the panic boundary.
///
/// # Errors
///
/// As [`read_parquet_metadata`], plus a schema the decoder rejects or
/// panics on (#683).
pub fn load_reader_metadata<R: ChunkReader>(reader: &R) -> Result<ReaderMetadata, ParquetError> {
    reader_metadata(read_parquet_metadata(reader)?)
}

/// The Arrow reader metadata for an already-checked footer: the schema
/// conversion that panics on a crafted `ARROW:schema` (#683), guarded.
///
/// # Errors
///
/// Returns a [`ParquetError`] if the schema cannot be derived or the
/// decoder panics.
pub fn reader_metadata(metadata: Arc<ParquetMetaData>) -> Result<ReaderMetadata, ParquetError> {
    guard_parquet("Arrow schema", || {
        ArrowReaderMetadata::try_new(metadata, ArrowReaderOptions::new()).map(ReaderMetadata)
    })
}

// ============================================================================
// The byte-array ceiling (#563)
// ============================================================================

/// Arrow's fixed i32 byte-offset ceiling. Production never reads an override.
pub const BYTE_ARRAY_BATCH_LIMIT: u64 = i32::MAX as u64;

/// The byte-array ceiling used by every read and concatenation boundary.
pub fn byte_array_limit() -> u64 {
    #[cfg(feature = "testing")]
    if let Some(limit) = std::env::var("TYLERTOO_BYTE_ARRAY_LIMIT_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
    {
        return limit.min(BYTE_ARRAY_BATCH_LIMIT);
    }
    BYTE_ARRAY_BATCH_LIMIT
}

/// Maximum expanded Arrow bytes in one read batch. Leave room for decode,
/// casts and queued batches; i32 offset safety alone is not a memory budget.
pub fn decoded_batch_limit() -> u64 {
    decoded_limit(byte_array_limit())
}

fn decoded_limit(offset_limit: u64) -> u64 {
    (64 << 20).min(offset_limit.saturating_mul(2))
}

/// Rows that can be joined without undoing a reader's memory/offset split.
///
/// Buffer sizes deliberately overestimate slices and dictionaries. Returning
/// the first batch unchanged always preserves the reader's existing bound.
pub fn concat_prefix_rows<'a>(
    batches: impl IntoIterator<Item = &'a RecordBatch>,
    limit: u64,
) -> usize {
    let mut bytes = 0u64;
    let mut rows = 0usize;
    for batch in batches {
        let next = bytes.saturating_add(batch.get_array_memory_size() as u64);
        if rows > 0 && next > limit.min(decoded_limit(limit)) {
            break;
        }
        bytes = next;
        rows = rows.saturating_add(batch.num_rows());
    }
    rows
}

fn scaled_ceiling() -> Option<u64> {
    let limit = byte_array_limit();
    (limit < BYTE_ARRAY_BATCH_LIMIT).then_some(limit)
}

/// Check a concatenated batch against the scaled byte-offset ceiling.
///
/// Fails `batch` when one of its `Binary`/`Utf8` columns holds more than a
/// scaled [`byte_array_limit`] (#563, #779). A no-op at the real ceiling,
/// which arrow enforces itself.
///
/// Every batch an [`UntrustedBatchReader`] yields is already checked. Call
/// this only on a batch built after the read, such as the in-memory
/// engine's concatenated table (#606): batches each under the ceiling can
/// pass it together.
///
/// # Errors
///
/// An [`ArrowError`] naming the column, its bytes and the ceiling.
pub fn check_byte_array_ceiling(batch: &RecordBatch) -> Result<(), ArrowError> {
    match scaled_ceiling() {
        Some(limit) => check_batch_under(batch, limit),
        None => Ok(()),
    }
}

/// Decode byte arrays as views, so repeated dictionary entries remain shared
/// until a bounded prefix is converted back to the input schema.
fn wide_field(field: &arrow_schema::FieldRef) -> arrow_schema::FieldRef {
    Arc::new(
        field
            .as_ref()
            .clone()
            .with_data_type(wide_type(field.data_type())),
    )
}

fn projected_field(
    original: &arrow_schema::Field,
    decoded: &arrow_schema::Field,
) -> arrow_schema::Field {
    use arrow_schema::DataType;
    let target = match (original.data_type(), decoded.data_type()) {
        (DataType::Struct(fields), DataType::Struct(projected)) => DataType::Struct(
            projected
                .iter()
                .map(|field| {
                    Arc::new(projected_field(
                        fields.iter().find(|f| f.name() == field.name()).unwrap(),
                        field,
                    ))
                })
                .collect(),
        ),
        (DataType::List(field), DataType::List(projected)) => {
            DataType::List(Arc::new(projected_field(field, projected)))
        }
        (DataType::LargeList(field), DataType::LargeList(projected)) => {
            DataType::LargeList(Arc::new(projected_field(field, projected)))
        }
        (DataType::FixedSizeList(field, n), DataType::FixedSizeList(projected, _)) => {
            DataType::FixedSizeList(Arc::new(projected_field(field, projected)), *n)
        }
        (DataType::Map(field, sorted), DataType::Map(projected, _)) => {
            DataType::Map(Arc::new(projected_field(field, projected)), *sorted)
        }
        _ => original.data_type().clone(),
    };
    original.clone().with_data_type(target)
}

fn wide_type(data_type: &arrow_schema::DataType) -> arrow_schema::DataType {
    use arrow_schema::DataType;
    match data_type {
        DataType::Utf8 | DataType::LargeUtf8 => DataType::Utf8View,
        DataType::Binary | DataType::LargeBinary => DataType::BinaryView,
        DataType::Dictionary(_, value) => wide_type(value),
        DataType::List(f) => DataType::List(wide_field(f)),
        DataType::LargeList(f) => DataType::LargeList(wide_field(f)),
        DataType::FixedSizeList(f, n) => DataType::FixedSizeList(wide_field(f), *n),
        DataType::Map(f, sorted) => DataType::Map(wide_field(f), *sorted),
        DataType::Struct(fields) => DataType::Struct(fields.iter().map(wide_field).collect()),
        _ => data_type.clone(),
    }
}

fn batch_overflow(batch: &RecordBatch, schema: &SchemaRef, limit: u64) -> Option<(String, u64)> {
    batch
        .columns()
        .iter()
        .zip(schema.fields())
        .find_map(|(array, field)| {
            array_overflow(array.as_ref(), field.data_type(), limit)
                .map(|bytes| (field.name().clone(), bytes))
        })
}

/// Only the children addressed by a slice count. Large byte arrays are exempt
/// unless they are the temporary wide representation of an i32-offset input.
fn array_overflow(
    array: &dyn arrow_array::Array,
    target: &arrow_schema::DataType,
    limit: u64,
) -> Option<u64> {
    use arrow_array::cast::AsArray;
    use arrow_schema::DataType;
    let over = |bytes: u64| (bytes > limit).then_some(bytes);
    match target {
        DataType::Utf8 | DataType::Binary => {
            let bytes = match array.data_type() {
                DataType::Utf8 => {
                    let o = array.as_string::<i32>().value_offsets();
                    (i64::from(*o.last().unwrap()) - i64::from(o[0])) as u64
                }
                DataType::Binary => {
                    let o = array.as_binary::<i32>().value_offsets();
                    (i64::from(*o.last().unwrap()) - i64::from(o[0])) as u64
                }
                DataType::LargeUtf8 => {
                    let o = array.as_string::<i64>().value_offsets();
                    (*o.last().unwrap() - o[0]) as u64
                }
                DataType::LargeBinary => {
                    let o = array.as_binary::<i64>().value_offsets();
                    (*o.last().unwrap() - o[0]) as u64
                }
                DataType::Utf8View => array
                    .as_string_view()
                    .views()
                    .iter()
                    .map(|v| u64::from(*v as u32))
                    .sum(),
                DataType::BinaryView => array
                    .as_binary_view()
                    .views()
                    .iter()
                    .map(|v| u64::from(*v as u32))
                    .sum(),
                _ => return None,
            };
            over(bytes)
        }
        DataType::List(field) => {
            let list = array.as_list::<i32>();
            let offsets = list.value_offsets();
            let start = offsets[0] as usize;
            let len = *offsets.last().unwrap() as usize - start;
            array_overflow(
                list.values().slice(start, len).as_ref(),
                field.data_type(),
                limit,
            )
        }
        DataType::LargeList(field) => {
            let list = array.as_list::<i64>();
            let offsets = list.value_offsets();
            let start = offsets[0] as usize;
            let len = *offsets.last().unwrap() as usize - start;
            array_overflow(
                list.values().slice(start, len).as_ref(),
                field.data_type(),
                limit,
            )
        }
        DataType::FixedSizeList(field, _) => array_overflow(
            array.as_fixed_size_list().values().as_ref(),
            field.data_type(),
            limit,
        ),
        DataType::Struct(fields) => array
            .as_struct()
            .columns()
            .iter()
            .zip(fields)
            .find_map(|(a, f)| array_overflow(a.as_ref(), f.data_type(), limit)),
        DataType::Map(field, _) => {
            let map = array.as_map();
            let offsets = map.value_offsets();
            let start = offsets[0] as usize;
            let len = *offsets.last().unwrap() as usize - start;
            array_overflow(&map.entries().slice(start, len), field.data_type(), limit)
        }
        DataType::Dictionary(_, value) => {
            if let Some(dictionary) = array.as_any_dictionary_opt() {
                array_overflow(dictionary.values().as_ref(), value, limit)
            } else {
                array_overflow(array, value, limit)
            }
        }
        _ => array.to_data().child_data().iter().find_map(|child| {
            let child = arrow_array::make_array(child.clone());
            array_overflow(child.as_ref(), child.data_type(), limit)
        }),
    }
}

// Charge the expanded values, not just the views' shared backing buffers.
// Slicing child data first also avoids charging unused list/dictionary data.
fn expanded_bytes(array: &dyn arrow_array::Array) -> u64 {
    use arrow_array::cast::AsArray;
    use arrow_schema::DataType;
    let own = (array.len() as u64).saturating_add(1);
    match array.data_type() {
        DataType::Utf8View => array
            .as_string_view()
            .views()
            .iter()
            .fold(own.saturating_mul(17), |n, v| {
                n.saturating_add(u64::from(*v as u32))
            }),
        DataType::BinaryView => array
            .as_binary_view()
            .views()
            .iter()
            .fold(own.saturating_mul(17), |n, v| {
                n.saturating_add(u64::from(*v as u32))
            }),
        DataType::List(_) => {
            let a = array.as_list::<i32>();
            let o = a.value_offsets();
            own.saturating_mul(5).saturating_add(expanded_bytes(
                a.values()
                    .slice(o[0] as usize, (*o.last().unwrap() - o[0]) as usize)
                    .as_ref(),
            ))
        }
        DataType::LargeList(_) => {
            let a = array.as_list::<i64>();
            let o = a.value_offsets();
            own.saturating_mul(9).saturating_add(expanded_bytes(
                a.values()
                    .slice(o[0] as usize, (*o.last().unwrap() - o[0]) as usize)
                    .as_ref(),
            ))
        }
        DataType::Struct(_) => array
            .as_struct()
            .columns()
            .iter()
            .fold(own, |n, a| n.saturating_add(expanded_bytes(a.as_ref()))),
        DataType::Map(_, _) => {
            let a = array.as_map();
            let o = a.value_offsets();
            own.saturating_mul(5).saturating_add(expanded_bytes(
                &a.entries()
                    .slice(o[0] as usize, (*o.last().unwrap() - o[0]) as usize),
            ))
        }
        DataType::FixedSizeList(_, _) => {
            own.saturating_add(expanded_bytes(array.as_fixed_size_list().values().as_ref()))
        }
        _ => (array
            .to_data()
            .get_slice_memory_size()
            .unwrap_or(usize::MAX) as u64)
            // A dictionary cast may additionally allocate 64-bit keys.
            .saturating_add((array.len() as u64).saturating_mul(9)),
    }
}

fn expanded_batch_bytes(batch: &RecordBatch) -> u64 {
    batch
        .columns()
        .iter()
        .fold(0u64, |n, a| n.saturating_add(expanded_bytes(a.as_ref())))
}

fn check_batch_under(batch: &RecordBatch, limit: u64) -> Result<(), ArrowError> {
    match batch_overflow(batch, &batch.schema(), limit) {
        Some((column, bytes)) => Err(ArrowError::ParquetError(format!(
            "index overflow decoding byte array: column '{column}' holds {bytes} bytes in one \
             batch, past the {limit}-byte byte-array ceiling (i32 offsets)"
        ))),
        None => Ok(()),
    }
}

/// Choose the initial decode size from selected row-group statistics.
///
/// Fixed-width values and byte views are bounded before decode. Statistics
/// refine the size; [`UntrustedBatchReader`] checks actual expanded bytes
/// before casting views, so dictionary expansion and skew cannot defeat it.
/// Both engines and every reader use this plan. An oversized row is refused.
pub fn read_batch_plan(
    metadata: &ParquetMetaData,
    row_groups: Option<&[usize]>,
    requested: usize,
    limit: u64,
) -> usize {
    let avg = metadata
        .row_groups()
        .iter()
        .enumerate()
        .filter(|(i, _)| row_groups.is_none_or(|picked| picked.contains(i)))
        .filter(|(_, group)| group.num_rows() > 0)
        .flat_map(|(_, group)| {
            group
                .columns()
                .iter()
                .filter(|column| {
                    column.column_descr().physical_type() == parquet::basic::Type::BYTE_ARRAY
                })
                .map(move |column| {
                    column
                        .uncompressed_size()
                        .max(column.unencoded_byte_array_data_bytes().unwrap_or(0))
                        .max(0) as u64
                        / group.num_rows() as u64
                })
        })
        .max()
        .unwrap_or(0);
    let fixed = metadata
        .row_groups()
        .iter()
        .enumerate()
        .filter(|(i, _)| row_groups.is_none_or(|picked| picked.contains(i)))
        .map(|(_, group)| {
            group
                .columns()
                .iter()
                .map(|column| {
                    use parquet::basic::Type;
                    let width = match column.column_descr().physical_type() {
                        Type::BOOLEAN => 1,
                        Type::INT32 | Type::FLOAT => 4,
                        Type::INT64 | Type::DOUBLE => 8,
                        Type::INT96 => 16,
                        Type::BYTE_ARRAY => 16, // one view, before bounded expansion
                        Type::FIXED_LEN_BYTE_ARRAY => {
                            column.column_descr().type_length().max(0) as u64
                        }
                    };
                    let values = if column.column_descr().max_rep_level() > 0 {
                        // A repeated leaf can put all its values in a single row.
                        column.num_values().max(0) as u64
                    } else {
                        1
                    };
                    width.saturating_add(1).saturating_mul(values)
                })
                .fold(0u64, u64::saturating_add)
        })
        .max()
        .unwrap_or(1)
        .max(1);
    requested
        .max(1)
        .min((decoded_limit(limit) / fixed).max(1) as usize)
        .min(
            usize::try_from((limit / 2) / avg.max(1))
                .unwrap_or(usize::MAX)
                .max(1),
        )
}

/// The byte-array column that holds the most decoded bytes in total over some
/// row groups (#606): what decides whether one batch can hold all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargestByteArrayColumn {
    /// The column's dotted path (`geometry`, or `a.b` for a nested leaf).
    pub column: String,
    /// Decoded bytes summed over the row groups.
    pub total_bytes: u64,
}

/// Find the byte-array leaf with the most decoded bytes in selected groups.
///
/// Sums over `row_groups` (`None` = all), counting only leaves under a root
/// for which `root_counts` returns true. Returns `None` when no counted leaf
/// holds any bytes.
///
/// Footer-only. A chunk's size is its `unencoded_byte_array_data_bytes` size
/// statistic when the writer recorded it, the exact length of the values the
/// decoded array holds. Without it the chunk's `total_uncompressed_size`
/// stands in. That figure counts a dictionary-encoded chunk's distinct values
/// once, so the total can then fall short of the decoded size.
pub fn largest_byte_array_column(
    meta: &ParquetMetaData,
    row_groups: Option<&[usize]>,
    root_counts: impl Fn(usize) -> bool,
) -> Option<LargestByteArrayColumn> {
    let descr = meta.file_metadata().schema_descr();
    let groups = meta.row_groups();
    let all: Vec<usize>;
    let picked: &[usize] = match row_groups {
        Some(p) => p,
        None => {
            all = (0..groups.len()).collect();
            &all
        }
    };
    let mut totals = vec![0u64; descr.num_columns()];
    for &gi in picked {
        let Some(rg) = groups.get(gi) else {
            continue;
        };
        for (leaf, col) in rg.columns().iter().enumerate() {
            if col.column_descr().physical_type() != parquet::basic::Type::BYTE_ARRAY {
                continue;
            }
            let bytes = col
                .unencoded_byte_array_data_bytes()
                .unwrap_or_else(|| col.uncompressed_size())
                .max(0) as u64;
            totals[leaf] = totals[leaf].saturating_add(bytes);
        }
    }
    totals
        .iter()
        .enumerate()
        .filter(|&(leaf, &bytes)| bytes > 0 && root_counts(descr.get_column_root_idx(leaf)))
        .max_by_key(|&(_, &bytes)| bytes)
        .map(|(leaf, &total_bytes)| LargestByteArrayColumn {
            column: descr.column(leaf).path().string(),
            total_bytes,
        })
}

/// Conservative expanded input bytes, before reading any pages.
///
/// Missing dictionary size statistics are bounded by repeating the entire chunk for
/// each value rather than treating dictionary storage as decoded storage.
pub fn decoded_input_bytes(
    metadata: &ParquetMetaData,
    row_groups: Option<&[usize]>,
    root_counts: impl Fn(usize) -> bool,
) -> u64 {
    let schema = metadata.file_metadata().schema_descr();
    metadata
        .row_groups()
        .iter()
        .enumerate()
        .filter(|(i, _)| row_groups.is_none_or(|picked| picked.contains(i)))
        .flat_map(|(_, group)| group.columns().iter().enumerate())
        .filter(|(leaf, _)| root_counts(schema.get_column_root_idx(*leaf)))
        .map(|(_, column)| {
            use parquet::basic::Type;
            let values = column.num_values().max(0) as u64;
            let payload = match column.column_descr().physical_type() {
                Type::BYTE_ARRAY => column.unencoded_byte_array_data_bytes().map_or_else(
                    || (column.uncompressed_size().max(0) as u64).saturating_mul(values),
                    |bytes| bytes.max(0) as u64,
                ),
                Type::FIXED_LEN_BYTE_ARRAY => {
                    values.saturating_mul(column.column_descr().type_length().max(0) as u64)
                }
                _ => values.saturating_mul(16),
            };
            // Offsets, validity and nested containers, including empty lists.
            payload.saturating_add(values.saturating_add(1).saturating_mul(24))
        })
        .fold(0u64, u64::saturating_add)
}

// ============================================================================
// Parquet
// ============================================================================

/// The parquet reader's default batch size, used when a caller sets none.
pub const DEFAULT_BATCH_ROWS: usize = 1024;

/// A checked Parquet file, ready to read: the footer has passed the
/// pre-check and the chunk-range check, and the Arrow schema decoded inside
/// the panic boundary.
///
/// The one place a read's projection, row groups and batch size are set;
/// [`Self::build`] caps the batch size and returns a reader whose every
/// batch is decoded inside the boundary.
pub struct ParquetSource<R: ChunkReader + 'static> {
    reader: R,
    projection: Option<ProjectionMask>,
    metadata: ReaderMetadata,
    row_groups: Option<Vec<usize>>,
    batch_rows: usize,
    limit: Option<usize>,
}

impl<R: ChunkReader + 'static> std::fmt::Debug for ParquetSource<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParquetSource")
            .field("row_groups", &self.row_groups)
            .field("batch_rows", &self.batch_rows)
            .finish_non_exhaustive()
    }
}

/// Open the Parquet file `reader` reads: footer pre-check, chunk-range
/// check, and the footer and schema decode inside the panic boundary.
///
/// # Errors
///
/// Returns a [`ParquetError`] if any of those fail; a decoder panic is a
/// [`ParquetError::External`] holding [`InputError::DecoderPanic`].
pub fn open_parquet<R: ChunkReader + 'static>(reader: R) -> Result<ParquetSource<R>, ParquetError> {
    let metadata = load_reader_metadata(&reader)?;
    Ok(open_parquet_with_metadata(reader, metadata))
}

/// [`open_parquet`] over a footer already checked and decoded by
/// [`load_reader_metadata`] (or [`reader_metadata`]): a caller opening the
/// same file many times parses its footer once.
pub fn open_parquet_with_metadata<R: ChunkReader + 'static>(
    reader: R,
    metadata: ReaderMetadata,
) -> ParquetSource<R> {
    ParquetSource {
        reader,
        projection: None,
        metadata,
        row_groups: None,
        batch_rows: DEFAULT_BATCH_ROWS,
        limit: None,
    }
}

impl<R: ChunkReader + 'static> ParquetSource<R> {
    /// The parsed footer.
    pub fn metadata(&self) -> &Arc<ParquetMetaData> {
        self.metadata.metadata()
    }

    /// The Arrow schema of the whole file.
    pub fn schema(&self) -> &SchemaRef {
        self.metadata.schema()
    }

    /// The Parquet schema, for building a [`ProjectionMask`].
    pub fn parquet_schema(&self) -> &SchemaDescriptor {
        self.metadata.parquet_schema()
    }

    /// The checked footer and schema, to open this file again with
    /// [`open_parquet_with_metadata`] without re-parsing.
    pub fn reader_metadata(&self) -> &ReaderMetadata {
        &self.metadata
    }

    /// Read only the columns in `mask`.
    #[must_use]
    pub fn with_projection(mut self, mask: ProjectionMask) -> Self {
        self.projection = Some(mask);
        self
    }

    /// Read only the top-level columns `roots` (indexes into the file
    /// schema).
    #[must_use]
    pub fn with_projection_roots(self, roots: impl IntoIterator<Item = usize>) -> Self {
        let mask = ProjectionMask::roots(self.parquet_schema(), roots);
        self.with_projection(mask)
    }

    /// Read only row groups `row_groups`, in that order.
    #[must_use]
    pub fn with_row_groups(mut self, row_groups: Vec<usize>) -> Self {
        self.row_groups = Some(row_groups);
        self
    }

    /// Ask for batches of at most `rows` rows. The read plan may lower the
    /// decode size, and the reader splits decoded batches when needed to
    /// stay under [`byte_array_limit`].
    #[must_use]
    pub fn with_batch_rows(mut self, rows: usize) -> Self {
        self.batch_rows = rows;
        self
    }

    /// Read at most `rows` rows, without decoding the remaining data pages.
    #[must_use]
    pub fn with_limit(mut self, rows: usize) -> Self {
        self.limit = Some(rows);
        self
    }

    /// Build a reader that decodes shared byte views, splits against both
    /// memory and offset ceilings, then restores the projected input schema.
    ///
    /// # Errors
    ///
    /// Returns a [`ParquetError`] if planning or construction fails.
    pub fn build(self) -> Result<UntrustedBatchReader, ParquetError> {
        let limit = byte_array_limit();
        let rows = read_batch_plan(
            self.metadata(),
            self.row_groups.as_deref(),
            self.batch_rows,
            limit,
        );
        let original = self.metadata.schema().clone();
        let wide = Arc::new(arrow_schema::Schema::new_with_metadata(
            original.fields().iter().map(wide_field).collect::<Vec<_>>(),
            original.metadata().clone(),
        ));
        let metadata = guard_parquet("wide-offset schema", || {
            ArrowReaderMetadata::try_new(
                self.metadata.metadata().clone(),
                ArrowReaderOptions::new().with_schema(wide),
            )
        })?;
        #[expect(
            clippy::disallowed_methods,
            reason = "#779: reader owner reuses checked metadata and applies batch limits"
        )]
        let mut builder = ParquetRecordBatchReaderBuilder::new_with_metadata(self.reader, metadata)
            .with_batch_size(rows);
        if let Some(projection) = self.projection {
            builder = builder.with_projection(projection);
        }
        if let Some(row_groups) = self.row_groups {
            builder = builder.with_row_groups(row_groups);
        }
        if let Some(limit) = self.limit {
            builder = builder.with_limit(limit);
        }
        let inner = guard_parquet("parquet reader", || builder.build())?;
        let schema = Arc::new(arrow_schema::Schema::new_with_metadata(
            inner
                .schema()
                .fields()
                .iter()
                .map(|f| {
                    original
                        .field_with_name(f.name())
                        .map(|original| projected_field(original, f))
                })
                .collect::<Result<Vec<_>, _>>()?,
            original.metadata().clone(),
        ));
        Ok(UntrustedBatchReader {
            inner,
            schema,
            done: false,
            pending: None,
            limit,
        })
    }
}

/// A Parquet record batch reader whose every batch is decoded inside the
/// panic boundary (#687).
///
/// Batches are split before narrowing byte-array offsets, recursively
/// through nested columns. After an error it yields nothing more.
#[derive(Debug)]
pub struct UntrustedBatchReader {
    inner: ParquetRecordBatchReader,
    done: bool,
    schema: SchemaRef,
    pending: Option<RecordBatch>,
    limit: u64,
}

/// One batch from `next`, decoded inside the panic boundary. After an error
/// or the end, `done` is set and every later call yields `None`.
pub(super) fn guarded_next(
    done: &mut bool,
    what: &'static str,
    next: impl FnOnce() -> Option<Result<RecordBatch, ArrowError>>,
) -> Option<Result<RecordBatch, ArrowError>> {
    if *done {
        return None;
    }
    let item = guard(what, next)
        .unwrap_or_else(|panic| Some(Err(ArrowError::ExternalError(Box::new(panic)))));
    *done = !matches!(item, Some(Ok(_)));
    item
}

impl Iterator for UntrustedBatchReader {
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let schema = self.schema.clone();
        let limit = self.limit;
        let item = guard("parquet page", || {
            let from_pending = self.pending.is_some();
            let batch = match self.pending.take() {
                Some(batch) => batch,
                None => match self.inner.next()? {
                    Ok(batch) => batch,
                    Err(e) => return Some(Err(e)),
                },
            };
            let fits = |rows| {
                let prefix = batch.slice(0, rows);
                batch_overflow(&prefix, &schema, limit).is_none()
                    && expanded_batch_bytes(&prefix) <= decoded_limit(limit)
            };
            let rows = if fits(batch.num_rows()) {
                batch.num_rows()
            } else {
                if let Some((column, bytes)) = batch_overflow(&batch.slice(0, 1), &schema, limit) {
                    return Some(Err(ArrowError::ExternalError(Box::new(
                        InputError::ByteArrayRowTooLarge {
                            column,
                            bytes,
                            limit_bytes: limit,
                        },
                    ))));
                }
                let bytes = expanded_batch_bytes(&batch.slice(0, 1));
                if bytes > decoded_limit(limit) {
                    return Some(Err(ArrowError::ExternalError(Box::new(
                        InputError::DecodedRowTooLarge {
                            bytes,
                            limit_bytes: decoded_limit(limit),
                        },
                    ))));
                }
                // Prefix byte counts are monotone, including addressed nested children.
                let (mut lo, mut hi) = (1, batch.num_rows());
                while lo + 1 < hi {
                    let mid = lo + (hi - lo) / 2;
                    if fits(mid) {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                lo
            };
            let compact = from_pending || rows < batch.num_rows();
            if rows < batch.num_rows() {
                self.pending = Some(batch.slice(rows, batch.num_rows() - rows));
            }
            let batch = batch.slice(0, rows);
            // Compact addressed nested children before the bounded cast.
            // Views still share dictionary/page payloads at this point.
            let indices =
                compact.then(|| arrow_array::UInt64Array::from_iter_values(0..rows as u64));
            Some(
                batch
                    .columns()
                    .iter()
                    .zip(schema.fields())
                    .map(|(a, f)| {
                        let compact = match &indices {
                            Some(indices) => arrow_select::take::take(a.as_ref(), indices, None)?,
                            None => a.clone(),
                        };
                        arrow_cast::cast(&compact, f.data_type())
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .and_then(|columns| RecordBatch::try_new(schema, columns)),
            )
        })
        .unwrap_or_else(|e| Some(Err(ArrowError::ExternalError(Box::new(e)))));
        self.done = !matches!(item, Some(Ok(_)));
        item
    }
}

impl RecordBatchReader for UntrustedBatchReader {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

// ============================================================================
// Arrow IPC
// ============================================================================

/// An Arrow IPC file reader whose footer, schema and every record batch are
/// decoded inside the panic boundary (#684). After an error it yields
/// nothing more.
pub struct UntrustedIpcReader<R: Read + Seek> {
    inner: FileReader<R>,
    done: bool,
}

/// Open the Arrow IPC file `reader` reads.
///
/// # Errors
///
/// Returns an [`ArrowError`] if the footer or schema cannot be decoded; a
/// decoder panic is an [`ArrowError::ExternalError`] holding
/// [`InputError::DecoderPanic`].
pub fn open_ipc<R: Read + Seek>(mut reader: R) -> Result<UntrustedIpcReader<R>, ArrowError> {
    guard_arrow("Arrow IPC footer", || precheck_ipc_blocks(&mut reader))?;
    #[expect(
        clippy::disallowed_methods,
        reason = "#779: IPC owner validates block bounds before construction"
    )]
    let inner = guard_arrow("Arrow IPC footer", || FileReader::try_new(reader, None))?;
    Ok(UntrustedIpcReader { inner, done: false })
}

/// Refuse an IPC file whose footer, or any dictionary or record-batch block
/// it lists, lies outside the file.
///
/// arrow-ipc zero-allocates the footer's declared length and each block's
/// `metaDataLength + bodyLength` before reading them, so a crafted length
/// aborts the process on the allocation: there is no panic for [`guard`] to
/// catch. A footer that does not verify is left for [`FileReader`] to name.
fn precheck_ipc_blocks<R: Read + Seek>(reader: &mut R) -> Result<(), ArrowError> {
    const TRAILER: u64 = 10; // footer length (4) + `ARROW1`
    let size = reader.seek(SeekFrom::End(0))?;
    if size < TRAILER {
        return Ok(());
    }
    let mut trailer = [0u8; TRAILER as usize];
    reader.seek(SeekFrom::End(-(TRAILER as i64)))?;
    reader.read_exact(&mut trailer)?;
    let Ok(footer_len) = arrow_ipc::reader::read_footer_length(trailer) else {
        return Ok(());
    };
    let refuse =
        |msg: String| ArrowError::ExternalError(Box::new(InputError::HostileIpcFooter(msg)));
    let body_end = size - TRAILER;
    if footer_len as u64 > body_end {
        return Err(refuse(format!(
            "the footer declares {footer_len} bytes but the file holds {size}"
        )));
    }
    let mut footer = vec![0u8; footer_len];
    reader.seek(SeekFrom::Start(body_end - footer_len as u64))?;
    reader.read_exact(&mut footer)?;
    let Ok(footer) = arrow_ipc::root_as_footer(&footer) else {
        return Ok(());
    };
    // A block must end before the footer starts.
    let blocks_end = body_end - footer_len as u64;
    let lists = [
        ("dictionary", footer.dictionaries()),
        ("record batch", footer.recordBatches()),
    ];
    for (what, blocks) in lists {
        for (i, block) in blocks.iter().flatten().enumerate() {
            let (offset, meta, body) = (block.offset(), block.metaDataLength(), block.bodyLength());
            let end = u64::try_from(offset)
                .ok()
                .zip(u64::try_from(meta).ok())
                .zip(u64::try_from(body).ok())
                .and_then(|((o, m), b)| o.checked_add(m)?.checked_add(b));
            if !end.is_some_and(|end| end <= blocks_end) {
                return Err(refuse(format!(
                    "{what} block {i} (offset {offset}, metadata {meta} bytes, body {body} \
                     bytes) lies outside the file's {blocks_end} data bytes"
                )));
            }
        }
    }
    Ok(())
}

impl<R: Read + Seek> UntrustedIpcReader<R> {
    /// The file's schema.
    pub fn schema(&self) -> SchemaRef {
        self.inner.schema()
    }
}

impl<R: Read + Seek> Iterator for UntrustedIpcReader<R> {
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        let inner = &mut self.inner;
        guarded_next(&mut self.done, "Arrow IPC record batch", || inner.next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn dictionary_decode_does_not_expand_repeated_payload_before_splitting() {
        use arrow_array::{Array, StringArray};
        use parquet::arrow::ArrowWriter;
        let batch = RecordBatch::try_from_iter(vec![(
            "name",
            Arc::new(StringArray::from(vec!["x".repeat(600); 100])) as arrow_array::ArrayRef,
        )])
        .unwrap();
        let mut file = tempfile::tempfile().unwrap();
        let mut writer = ArrowWriter::try_new(&mut file, batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let source = open_parquet(file.try_clone().unwrap()).unwrap();
        let group = source.metadata().row_group(0).clone();
        let columns = group
            .columns()
            .iter()
            .cloned()
            .map(|c| {
                c.into_builder()
                    .set_unencoded_byte_array_data_bytes(None)
                    .build()
                    .unwrap()
            })
            .collect();
        let group = group
            .into_builder()
            .set_column_metadata(columns)
            .build()
            .unwrap();
        let metadata = source
            .metadata()
            .as_ref()
            .clone()
            .into_builder()
            .set_row_groups(vec![group])
            .build();
        assert!(metadata
            .row_group(0)
            .column(0)
            .unencoded_byte_array_data_bytes()
            .is_none());
        let mut reader =
            open_parquet_with_metadata(file, reader_metadata(Arc::new(metadata)).unwrap())
                .with_batch_rows(100)
                .build()
                .unwrap();
        // Inspect the allocation BEFORE the owner's splitter/casts can run.
        let decoded = reader.inner.next().unwrap().unwrap();
        assert!(
            decoded.column(0).get_buffer_memory_size() < 10_000,
            "dictionary payload expanded before splitting: {} bytes",
            decoded.column(0).get_buffer_memory_size()
        );
    }

    #[test]
    fn read_plan_bounds_fixed_width_batches() {
        use arrow_array::{ArrayRef, Float64Array};
        use parquet::arrow::ArrowWriter;
        let batch = RecordBatch::try_from_iter((0..100).map(|i| {
            (
                format!("property_{i}"),
                Arc::new(Float64Array::from(vec![0.; 100])) as ArrayRef,
            )
        }))
        .unwrap();
        let mut file = tempfile::tempfile().unwrap();
        let mut writer = ArrowWriter::try_new(&mut file, batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let source = open_parquet(file).unwrap();
        let rows = read_batch_plan(source.metadata(), None, 100, 1000);
        assert!(rows * 100 * 8 <= 2000, "planned {rows} wide numeric rows");
    }

    #[test]
    fn decoded_bound_counts_all_columns_before_expanding_views() {
        use arrow_array::{ArrayRef, StringArray};
        use parquet::arrow::ArrowWriter;
        let batch = RecordBatch::try_from_iter((0..3).map(|i| {
            (
                format!("p{i}"),
                Arc::new(StringArray::from(vec!["x".repeat(400); 2])) as ArrayRef,
            )
        }))
        .unwrap();
        let mut file = tempfile::tempfile().unwrap();
        let mut writer = ArrowWriter::try_new(&mut file, batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let mut reader = open_parquet(file)
            .unwrap()
            .with_batch_rows(2)
            .build()
            .unwrap();
        reader.limit = 1000;
        let batches = reader.collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(
            batches
                .iter()
                .map(RecordBatch::num_rows)
                .collect::<Vec<_>>(),
            [1, 1]
        );
        for batch in batches {
            assert!(expanded_batch_bytes(&batch) <= 2000);
        }
    }

    fn corpus(target: &str, name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus")
            .join(target)
            .join(name)
    }

    /// A caught panic names where it happened and does not claim the input
    /// was malformed: our own reader code runs inside the boundary too.
    #[test]
    fn a_caught_panic_names_its_location_not_a_cause() {
        let err = super::guard("test value", || panic!("boom"))
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("panicked while decoding the test value: boom (at "),
            "{err}"
        );
        assert!(err.contains("untrusted.rs:"), "{err}");
        assert!(!err.contains("malformed"), "{err}");
    }

    /// A list's declared size is floored at its element type's wire size:
    /// ten doubles need 80 bytes, so 16 remaining bytes refuse it.
    #[test]
    fn a_list_of_doubles_needs_eight_bytes_each() {
        // Field 1, type list; list of 10 doubles; 15 bytes; struct stop.
        let mut footer = vec![0x19, 0xA7];
        footer.extend_from_slice(&[0u8; 15]);
        footer.push(0);
        let err = super::precheck_footer(&footer).unwrap_err().to_string();
        assert!(err.contains("10 elements of at least 8 byte"), "{err}");
    }

    /// The `plan_file` fuzz target's first find once re-enabled: an IPC
    /// footer whose dictionary block declares a ~1 PB length. arrow-ipc
    /// zero-allocates a block's length before it reads it, so the process
    /// aborted on the allocation (no panic for the boundary to catch).
    #[test]
    fn an_ipc_block_past_the_file_end_is_refused_before_the_read() {
        let bytes = std::fs::read(corpus("plan_file", "seed-ipc-block-length.arrow")).unwrap();
        let err = super::open_ipc(std::io::Cursor::new(bytes))
            .err()
            .expect("a block past the file end is refused")
            .to_string();
        assert!(err.contains("block"), "{err}");
    }
}
