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
//!   the upstream message and location. See [`guard`] for why that is sound.
//! - **One batch-size ceiling** (#563): [`ParquetSource::build`] lowers the
//!   requested batch so no decoded byte-array column can reach arrow's i32
//!   offset limit, from footer statistics alone. [`byte_array_limit`] is the
//!   ceiling every path uses, so a test that scales it scales every path.
//! - **IPC blocks inside the file**: [`open_ipc`] refuses a footer whose
//!   dictionary or record-batch block ends past the file, before arrow-ipc
//!   zero-allocates the block's declared length.
//! - **One place to set the projection, row groups and batch size**:
//!   [`ParquetSource`].
//!
//! clippy's `disallowed-methods` (clippy.toml) forbids the raw builders
//! everywhere else; this module is their one permitted caller.
#![allow(clippy::disallowed_methods)] // #779: the owner of the raw builders

use std::cell::{Cell, RefCell};
use std::io::{Read, Seek, SeekFrom};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Once};

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

use crate::input::InputError;

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
    static HOOK: Once = Once::new();
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
fn guard_arrow<T>(
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
/// ([`min_wire_bytes`]), not its decoded size. A struct element can be one
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

/// [`read_parquet_metadata`], plus the Arrow schema (decoded from the
/// footer's `ARROW:schema` key when present) inside the panic boundary.
///
/// # Errors
///
/// As [`read_parquet_metadata`], plus a schema the decoder rejects or
/// panics on (#683).
pub fn load_reader_metadata<R: ChunkReader>(
    reader: &R,
) -> Result<ArrowReaderMetadata, ParquetError> {
    reader_metadata(read_parquet_metadata(reader)?)
}

/// The Arrow reader metadata for an already-checked footer: the schema
/// conversion that panics on a crafted `ARROW:schema` (#683), guarded.
///
/// # Errors
///
/// Returns a [`ParquetError`] if the schema cannot be derived or the
/// decoder panics.
pub fn reader_metadata(
    metadata: Arc<ParquetMetaData>,
) -> Result<ArrowReaderMetadata, ParquetError> {
    guard_parquet("Arrow schema", || {
        ArrowReaderMetadata::try_new(metadata, ArrowReaderOptions::new())
    })
}

// ============================================================================
// The byte-array ceiling (#563)
// ============================================================================

/// The environment variable that scales the byte-array ceiling down (#779):
/// `TYLERTOO_BYTE_ARRAY_LIMIT_BYTES=16384` makes every read path size its
/// batches, and fail, as if arrow's i32 offset ceiling were 16 KiB. The
/// scaled-limit lane (`crates/core/tests/scaled_limits.rs`) sets it: those
/// tests run the CLI-facing engines from outside the crate, where the
/// in-crate [`scale_byte_array_limit`] hook cannot reach.
pub(crate) const BYTE_ARRAY_LIMIT_ENV: &str = "TYLERTOO_BYTE_ARRAY_LIMIT_BYTES";

/// The most bytes of one byte-array column a decoded batch may hold:
/// [`crate::input_set::BYTE_ARRAY_BATCH_LIMIT`], unless it is scaled down.
///
/// The one source of the ceiling. Every read path sizes its batches from
/// it, and [`check_byte_array_ceiling`] fails a batch past it, so scaling
/// it exercises every path's cap at a size a fixture can reach. Two things
/// scale it, and either can only lower it:
///
/// - [`BYTE_ARRAY_LIMIT_ENV`], a positive integer (anything else is
///   ignored);
/// - in unit tests, `scale_byte_array_limit`.
///
/// Reads the environment on every call, so callers ask once per read plan
/// (a [`ParquetSource::build`], a preflight, an in-memory load), never per
/// batch.
pub fn byte_array_limit() -> u64 {
    let limit = std::env::var(BYTE_ARRAY_LIMIT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map_or(crate::input_set::BYTE_ARRAY_BATCH_LIMIT, |n| {
            n.min(crate::input_set::BYTE_ARRAY_BATCH_LIMIT)
        });
    #[cfg(test)]
    if let Some(scaled) = scaled_limit() {
        return limit.min(scaled);
    }
    limit
}

/// [`byte_array_limit`] when it is scaled below arrow's real ceiling, the
/// one a reader must then enforce itself; `None` at the real ceiling, which
/// arrow enforces.
fn scaled_ceiling() -> Option<u64> {
    let limit = byte_array_limit();
    (limit < crate::input_set::BYTE_ARRAY_BATCH_LIMIT).then_some(limit)
}

#[cfg(test)]
static SCALED_LIMIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
fn scaled_limit() -> Option<u64> {
    match SCALED_LIMIT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        n => Some(n),
    }
}

/// While alive, [`byte_array_limit`] is the scaled ceiling and every batch
/// an [`UntrustedBatchReader`] yields is checked against it the way arrow
/// checks the real one: a byte-array column past it fails the read with
/// arrow's own overflow message. Process-wide, so the engines' worker
/// threads see it; serialized, so two scaling tests cannot interleave.
#[cfg(test)]
pub(crate) struct ScaledByteArrayLimit {
    _serial: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
pub(crate) fn scale_byte_array_limit(bytes: u64) -> ScaledByteArrayLimit {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    SCALED_LIMIT.store(bytes.max(1), std::sync::atomic::Ordering::Relaxed);
    ScaledByteArrayLimit { _serial: serial }
}

#[cfg(test)]
impl Drop for ScaledByteArrayLimit {
    fn drop(&mut self) {
        SCALED_LIMIT.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Fails `batch` the way arrow fails a batch past its i32 offset ceiling
/// when one of its `Binary`/`Utf8` columns holds more than a scaled
/// [`byte_array_limit`] (#563, #779). A no-op at the real ceiling, which
/// arrow enforces itself.
///
/// Every batch an [`UntrustedBatchReader`] yields is already checked. Call
/// this only on a batch built after the read, such as the in-memory
/// engine's concatenated table (#606): batches each under the ceiling can
/// pass it together.
///
/// # Errors
///
/// An [`ArrowError`] naming the column, its bytes and the ceiling.
pub(crate) fn check_byte_array_ceiling(batch: &RecordBatch) -> Result<(), ArrowError> {
    match scaled_ceiling() {
        Some(limit) => check_batch_under(batch, limit),
        None => Ok(()),
    }
}

/// The scaled ceiling's stand-in for arrow's i32 offset check: an error
/// carrying arrow's own "index overflow decoding byte array" wording when a
/// `Binary`/`Utf8` column of `batch` holds more than `limit` bytes.
fn check_batch_under(batch: &RecordBatch, limit: u64) -> Result<(), ArrowError> {
    use arrow_array::cast::AsArray;
    use arrow_schema::DataType;
    for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
        let offsets = match column.data_type() {
            DataType::Binary => column.as_binary::<i32>().value_offsets(),
            DataType::Utf8 => column.as_string::<i32>().value_offsets(),
            _ => continue,
        };
        // The bytes this (possibly sliced) column addresses.
        let bytes = match (offsets.first(), offsets.last()) {
            (Some(&first), Some(&last)) => u64::try_from(last - first).unwrap_or(0),
            _ => 0,
        };
        if bytes > limit {
            return Err(ArrowError::ParquetError(format!(
                "index overflow decoding byte array: column '{}' holds {bytes} bytes in one \
                 batch, past the {limit}-byte byte-array ceiling (scaled down from arrow's \
                 i32 offset limit, #563; {BYTE_ARRAY_LIMIT_ENV})",
                field.name()
            )));
        }
    }
    Ok(())
}

/// `requested` rows per batch, lowered when the widest byte-array column of
/// `metadata`'s row groups `row_groups` (`None` = all) needs fewer to keep a
/// batch under [`byte_array_limit`] (#563). Never below 1.
pub fn batch_rows_for(
    metadata: &ParquetMetaData,
    row_groups: Option<&[usize]>,
    requested: usize,
) -> usize {
    crate::input_set::capped_batch_rows(
        requested.max(1),
        crate::input_set::widest_byte_array_column(metadata, row_groups, 0).as_ref(),
    )
}

// ============================================================================
// Parquet
// ============================================================================

/// The parquet reader's default batch size, used when a caller sets none.
pub(crate) const DEFAULT_BATCH_ROWS: usize = 1024;

/// A checked Parquet file, ready to read: the footer has passed the
/// pre-check and the chunk-range check, and the Arrow schema decoded inside
/// the panic boundary.
///
/// The one place a read's projection, row groups and batch size are set;
/// [`Self::build`] caps the batch size and returns a reader whose every
/// batch is decoded inside the boundary.
pub struct ParquetSource<R: ChunkReader + 'static> {
    builder: ParquetRecordBatchReaderBuilder<R>,
    metadata: ArrowReaderMetadata,
    row_groups: Option<Vec<usize>>,
    batch_rows: usize,
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
    metadata: ArrowReaderMetadata,
) -> ParquetSource<R> {
    ParquetSource {
        builder: ParquetRecordBatchReaderBuilder::new_with_metadata(reader, metadata.clone()),
        metadata,
        row_groups: None,
        batch_rows: DEFAULT_BATCH_ROWS,
    }
}

impl<R: ChunkReader + 'static> ParquetSource<R> {
    /// The parsed footer.
    pub fn metadata(&self) -> &Arc<ParquetMetaData> {
        self.builder.metadata()
    }

    /// The Arrow schema of the whole file.
    pub fn schema(&self) -> &SchemaRef {
        self.builder.schema()
    }

    /// The Parquet schema, for building a [`ProjectionMask`].
    pub fn parquet_schema(&self) -> &SchemaDescriptor {
        self.builder.parquet_schema()
    }

    /// The checked footer and schema, to open this file again with
    /// [`open_parquet_with_metadata`] without re-parsing.
    pub fn reader_metadata(&self) -> &ArrowReaderMetadata {
        &self.metadata
    }

    /// Read only the columns in `mask`.
    #[must_use]
    pub fn with_projection(mut self, mask: ProjectionMask) -> Self {
        self.builder = self.builder.with_projection(mask);
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

    /// Ask for batches of `rows` rows. [`Self::build`] lowers this when the
    /// selected row groups' widest byte-array column needs fewer rows to
    /// stay under [`byte_array_limit`].
    #[must_use]
    pub fn with_batch_rows(mut self, rows: usize) -> Self {
        self.batch_rows = rows;
        self
    }

    /// The batch size [`Self::build`] will use: the requested one, capped
    /// for the selected row groups (#563).
    pub fn capped_batch_rows(&self) -> usize {
        batch_rows_for(self.metadata(), self.row_groups.as_deref(), self.batch_rows)
    }

    /// Build the reader.
    ///
    /// # Errors
    ///
    /// Returns a [`ParquetError`] if the reader cannot be built or its
    /// construction panics.
    pub fn build(self) -> Result<UntrustedBatchReader, ParquetError> {
        let rows = self.capped_batch_rows();
        let mut builder = self.builder.with_batch_size(rows);
        if let Some(row_groups) = self.row_groups {
            builder = builder.with_row_groups(row_groups);
        }
        let inner = guard_parquet("parquet reader", || builder.build())?;
        Ok(UntrustedBatchReader::new(inner))
    }
}

/// A Parquet record batch reader whose every batch is decoded inside the
/// panic boundary (#687).
///
/// When the byte-array ceiling is scaled, every batch is also checked
/// against it (#779). After an error it yields nothing more.
#[derive(Debug)]
pub struct UntrustedBatchReader {
    inner: ParquetRecordBatchReader,
    done: bool,
    /// The scaled [`byte_array_limit`] every batch is held to, read once
    /// when the reader is built; `None` at the real ceiling.
    scaled_ceiling: Option<u64>,
}

impl UntrustedBatchReader {
    fn new(inner: ParquetRecordBatchReader) -> Self {
        Self {
            inner,
            done: false,
            scaled_ceiling: scaled_ceiling(),
        }
    }
}

/// One batch from `next`, decoded inside the panic boundary. After an error
/// or the end, `done` is set and every later call yields `None`.
fn guarded_next(
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
        let inner = &mut self.inner;
        let ceiling = self.scaled_ceiling;
        guarded_next(&mut self.done, "parquet page", || {
            let item = inner.next();
            match ceiling {
                Some(limit) => {
                    item.map(|r| r.and_then(|b| check_batch_under(&b, limit).map(|()| b)))
                }
                None => item,
            }
        })
    }
}

impl RecordBatchReader for UntrustedBatchReader {
    fn schema(&self) -> SchemaRef {
        self.inner.schema()
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
    use std::path::{Path, PathBuf};

    fn corpus(target: &str, name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus")
            .join(target)
            .join(name)
    }

    /// Every row of every column of the input at `path`, read the way a
    /// convert reads it.
    fn read_all(path: &Path) -> Result<usize, String> {
        let source = crate::input::InputSource::Local(path.to_path_buf());
        let reader = source
            .open()
            .map_err(|e| e.to_string())?
            .build()
            .map_err(|e| e.to_string())?;
        let mut rows = 0;
        for batch in reader {
            rows += batch.map_err(|e| e.to_string())?.num_rows();
        }
        Ok(rows)
    }

    /// A point GeoParquet of `n` rows whose `name` column holds `name_bytes`
    /// per row: the byte-array column that sets every read's batch cap.
    fn write_wide_points(path: &Path, n: usize, name_bytes: usize) {
        use arrow_array::{Array, Int64Array, RecordBatch, StringArray};
        use arrow_schema::{DataType, Field, Schema};
        use geoarrow::array::GeometryBuilder;
        use geoarrow::datatypes::GeometryType;
        use geoarrow_array::GeoArrowArray;
        use geoparquet::writer::{
            GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
        };
        use std::sync::Arc;

        let geoms: Vec<Option<geo::Geometry<f64>>> = (0..n)
            .map(|i| {
                let (x, y) = ((i % 40) as f64 * 4.0 - 80.0, (i / 40) as f64 * 2.0 - 25.0);
                Some(geo::Geometry::Point(geo::Point::new(x, y)))
            })
            .collect();
        let mut b =
            GeometryBuilder::new(GeometryType::new(Default::default())).with_prefer_multi(false);
        b.extend_from_iter(geoms.iter().map(|g| g.as_ref()));
        let geom_arr = b.finish();
        let schema = Arc::new(Schema::new(vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(Field::new("name", DataType::Utf8, false)),
            Arc::new(geom_arr.data_type().to_field("geometry", true)),
        ]));
        let names: Vec<String> = (0..n)
            .map(|i| format!("{i:0>width$}", width = name_bytes))
            .collect();
        let columns: Vec<Arc<dyn Array>> = vec![
            Arc::new(Int64Array::from((0..n as i64).collect::<Vec<_>>())),
            Arc::new(StringArray::from(names)),
            geom_arr.to_array_ref(),
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        let options = GeoParquetWriterOptionsBuilder::default()
            .set_encoding(GeoParquetWriterEncoding::WKB)
            .set_generate_covering(true)
            .build();
        let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &options).unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut writer =
            parquet::arrow::ArrowWriter::try_new(file, encoder.target_schema(), None).unwrap();
        writer
            .write(&encoder.encode_record_batch(&batch).unwrap())
            .unwrap();
        writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
        writer.close().unwrap();
    }

    /// One owner means one ceiling (#563, #577, #606, #607, #657, #659): with
    /// the byte-array ceiling scaled to 8 KiB, every former reader path reads
    /// the same file under it. The owner's readers then fail any batch whose
    /// byte-array column passes the scaled ceiling, exactly as arrow fails
    /// one past the real 2 GiB, so a path that bypassed the cap would fail
    /// here with "index overflow decoding byte array".
    #[test]
    fn one_scaled_ceiling_reaches_every_read_path() {
        use crate::input_set::{ConvertSource, ReadPlan};
        use crate::overview::convert::{convert_to_overviews, ConvertError, ConvertOptions};

        const LIMIT: u64 = 8 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        let overview = dir.path().join("overview.parquet");
        // 1000 rows of 200-byte names: 200 KB in one column, ~25 batches'
        // worth at the scaled ceiling, and 1024-row default batches would
        // pass it ~25 times over.
        write_wide_points(&input, 1000, 200);
        let _scaled = super::scale_byte_array_limit(LIMIT);
        assert_eq!(super::byte_array_limit(), LIMIT);

        // input_set staging: the stream every engine pass reads.
        let source = ConvertSource::resolve_path(&input).unwrap();
        let plan = ReadPlan {
            batch_size: 8192,
            projection: None,
            row_groups: None,
        };
        let rows: usize = source
            .open_stream(&plan)
            .unwrap()
            .map(|b| b.unwrap().num_rows())
            .sum();
        assert_eq!(rows, 1000);

        // The streaming engine: assign, pass 1, pass 2, all under the cap.
        let options = ConvertOptions {
            cluster: true,
            ..ConvertOptions::default()
        };
        convert_to_overviews(&input, &overview, &options).unwrap();

        // The in-memory engine concatenates the whole table, so no batch size
        // can save it: it refuses from footer statistics, against the same
        // (scaled) ceiling.
        let in_memory = ConvertOptions {
            streaming: false,
            ..options.clone()
        };
        match convert_to_overviews(&input, dir.path().join("mem.parquet"), &in_memory) {
            Err(ConvertError::InMemoryByteArrayTooLarge { limit_bytes, .. }) => {
                assert_eq!(limit_bytes, LIMIT);
            }
            other => panic!("expected the #606 refusal at the scaled ceiling, got {other:?}"),
        }

        // The overview reader, at the export's batch size.
        let reader = crate::overview::reader::OverviewReader::open(&overview).unwrap();
        for k in 0..reader.num_levels() {
            for batch in reader.read_band_batched(k, 8192).unwrap() {
                batch.unwrap();
            }
        }

        // validate / check: the §12.1 cluster sum read (#607).
        let report = crate::overview::check::validate_file(&overview).unwrap();
        assert_eq!(
            report.check_passed("cluster_sum_invariant"),
            Some(true),
            "{report:?}"
        );

        // export-pmtiles reads every level through the overview reader.
        crate::overview::export::export_pmtiles(
            &overview,
            dir.path().join("out.pmtiles"),
            &crate::overview::export::ExportOptions::default(),
        )
        .unwrap();
    }

    /// The scaled ceiling bites: a reader asked for 1024-row batches over the
    /// wide file without the cap would fail, so the test above is not
    /// passing vacuously.
    #[test]
    fn the_scaled_ceiling_fails_an_uncapped_batch() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        write_wide_points(&input, 1000, 200);
        let _scaled = super::scale_byte_array_limit(8 * 1024);
        let file = std::fs::File::open(&input).unwrap();
        let source = super::open_parquet(file).unwrap().with_batch_rows(1024);
        assert!(source.capped_batch_rows() < 1024);
        // Bypass the cap the way a raw builder would.
        let mut reader = super::UntrustedBatchReader::new(
            parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::new_with_metadata(
                std::fs::File::open(&input).unwrap(),
                source.reader_metadata().clone(),
            )
            .with_batch_size(1024)
            .build()
            .unwrap(),
        );
        let err = reader.next().unwrap().unwrap_err().to_string();
        assert!(err.contains("index overflow decoding byte array"), "{err}");
    }

    /// Batches each under the scaled ceiling can pass it together: the
    /// in-memory engine's concatenated table goes through the owner's check
    /// too (#606), and only past the ceiling.
    #[test]
    fn a_concatenated_table_is_held_to_the_scaled_ceiling() {
        use arrow_array::{RecordBatch, StringArray};
        use std::sync::Arc;
        let schema = Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new(
            "name",
            arrow_schema::DataType::Utf8,
            false,
        )]));
        let batch = |rows: usize| {
            let values: Vec<String> = (0..rows).map(|_| "x".repeat(100)).collect();
            RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(values))]).unwrap()
        };
        let _scaled = super::scale_byte_array_limit(1000);
        let part = batch(8);
        super::check_byte_array_ceiling(&part).unwrap();
        // A slice counts only the bytes it addresses.
        super::check_byte_array_ceiling(&batch(20).slice(5, 10)).unwrap();
        #[allow(clippy::disallowed_methods)] // the in-memory concat under test
        let full = arrow_select::concat::concat_batches(&schema, &[part.clone(), part]).unwrap();
        let err = super::check_byte_array_ceiling(&full)
            .unwrap_err()
            .to_string();
        assert!(err.contains("index overflow decoding byte array"), "{err}");
        assert!(err.contains("1600 bytes"), "{err}");
    }

    /// #683: a crafted `ARROW:schema` footer key panicked arrow-ipc's schema
    /// conversion on every open.
    #[test]
    fn issue_683_crafted_arrow_schema_is_an_error_on_every_open() {
        let path = corpus("overview_file", "seed-issue-683-arrow-schema.parquet");
        let err = read_all(&path).unwrap_err();
        assert!(err.contains("panicked"), "input open: {err}");
        let err = crate::overview::reader::OverviewReader::open(&path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("panicked"), "overview reader: {err}");
        // `validate` must not pass a file export refuses: the schema decode
        // is one of its checks.
        let report = crate::overview::check::validate_file(&path).unwrap();
        let failed = report
            .checks
            .iter()
            .find(|c| c.name == "arrow_schema_decodes" && !c.passed)
            .unwrap_or_else(|| panic!("validate passed #683's file: {:?}", report.checks));
        assert!(failed.message.contains("panicked"), "{}", failed.message);
    }

    /// #687: a crafted data page panicked parquet-rs's definition-level
    /// decoder mid-read.
    #[test]
    fn issue_687_crafted_def_levels_are_an_error_not_a_panic() {
        let path = corpus("remote_footer", "seed-issue-687-def-levels.parquet");
        let err = read_all(&path).unwrap_err();
        assert!(err.contains("panicked"), "{err}");
    }

    /// #690: a footer whose schema element declares billions of children
    /// made parquet-rs reserve ~12 GB before it read any of them. The
    /// pre-check refuses it, naming the count, before parquet sees it.
    #[test]
    fn issue_690_huge_num_children_is_refused_before_the_decode() {
        let thrift = std::fs::read(corpus(
            "parquet_footer",
            "seed-issue-690-num-children.thrift",
        ))
        .unwrap();
        let mut file = b"PAR1".to_vec();
        file.extend_from_slice(&thrift);
        file.extend_from_slice(&(thrift.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oom.parquet");
        std::fs::write(&path, &file).unwrap();
        let err = crate::overview::check::validate_file(&path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("children"), "{err}");
        let err = read_all(&path).unwrap_err();
        assert!(err.contains("children"), "{err}");
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
