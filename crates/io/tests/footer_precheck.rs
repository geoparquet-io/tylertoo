//! Property: a footer the pre-check passes decodes in parquet-rs without
//! reserving memory for elements it does not hold (#690, #789).
//!
//! The pre-check is a second reader of the thrift bytes, so it must see the
//! counts parquet-rs will see. Each case encodes a two-element schema whose
//! `num_children` and neighbouring fields vary in wire type, field-id form
//! and stop byte, among unknown fields of any type, then decodes the footer
//! with parquet-rs directly and measures its largest single allocation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use proptest::prelude::*;

/// Records the largest single allocation on this thread.
struct Largest;

thread_local! {
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards every call to `System` unchanged.
unsafe impl GlobalAlloc for Largest {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = LARGEST.try_with(|l| l.set(l.get().max(layout.size())));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Largest = Largest;

/// The largest allocation parquet-rs makes decoding `footer`, unchecked.
fn largest_decode_allocation(footer: &[u8]) -> usize {
    LARGEST.with(|l| l.set(0));
    let _ = std::panic::catch_unwind(|| {
        #[expect(
            clippy::disallowed_methods,
            reason = "#789: the property measures the unchecked decode"
        )]
        let _ = parquet::file::metadata::ParquetMetaDataReader::decode_metadata(footer);
    });
    LARGEST.with(Cell::get)
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// A field header. The long form carries `id + wrap * 65,536`, which
/// parquet-rs truncates back to `id`.
fn header(out: &mut Vec<u8>, last: &mut i16, id: i16, ty: u8, long: bool, wrap: i64) {
    match id.checked_sub(*last) {
        Some(delta @ 1..=15) if !long => out.push((delta as u8) << 4 | ty),
        _ => {
            out.push(ty);
            varint(out, zigzag(i64::from(id) + wrap * 65_536));
        }
    }
    *last = id;
}

/// A value of compact type `ty`, its content drawn from `seed`: enum-sized
/// varints and ASCII strings, so parquet-rs gets past them.
fn value(out: &mut Vec<u8>, ty: u8, seed: u64, depth: u32) {
    match ty {
        3 => out.push(seed as u8),
        4..=6 => varint(out, seed % 3),
        7 => out.extend_from_slice(&seed.to_le_bytes()),
        8 => {
            varint(out, seed % 5);
            out.extend((0..seed % 5).map(|i| b'a' + (seed >> (8 * i)) as u8 % 26));
        }
        9 | 10 if depth < 3 => {
            let (elem, n) = ((seed % 13 + 1) as u8, (seed >> 4) % 4);
            out.push((n as u8) << 4 | elem);
            (0..n).for_each(|i| collection_element(out, elem, seed >> (8 + i), depth));
        }
        11 if depth < 3 => {
            let n = seed % 3;
            varint(out, n);
            if n > 0 {
                let (k, v) = ((seed >> 2) % 13 + 1, (seed >> 6) % 13 + 1);
                out.push((k as u8) << 4 | v as u8);
                for i in 0..n {
                    collection_element(out, k as u8, seed >> (10 + i), depth);
                    collection_element(out, v as u8, seed >> (20 + i), depth);
                }
            }
        }
        12 => {
            if depth < 3 && seed % 2 == 1 {
                let mut last = 0;
                let ty = ((seed >> 1) % 13 + 1) as u8;
                header(out, &mut last, ((seed >> 5) % 20) as i16, ty, false, 0);
                value(out, ty, seed >> 10, depth + 1);
            }
            if seed >> 4 & 3 == 0 {
                // Id 32,767, then a short-form field: a delta past i16.
                let mut last = 0;
                header(out, &mut last, i16::MAX, 3, true, 0);
                out.extend_from_slice(&[0, 0x13, 0]);
            }
            out.push(0);
        }
        13 => out.extend_from_slice(&[seed as u8; 16]),
        // Bools carry no bytes in a field; past the depth cap, an empty
        // list, set or map.
        _ => out.push(0),
    }
}

/// A collection element. A bool is one byte on the wire, but parquet-rs
/// skips it as none, so half the time it is written as none.
fn collection_element(out: &mut Vec<u8>, ty: u8, seed: u64, depth: u32) {
    match ty {
        1 | 2 if seed.is_multiple_of(2) => {}
        1 | 2 => out.push(1),
        _ => value(out, ty, seed, depth + 1),
    }
}

/// A `LogicalType` as parquet-rs reads it: one variant, a struct. Each
/// header's wire type is sometimes another than the one parquet-rs reads.
fn logical_type(out: &mut Vec<u8>, seed: u64) {
    let wire = |k: u32, read_as: u8| match seed >> (32 + k) & 1 {
        0 => read_as,
        _ => ((seed >> (40 + 4 * k)) % 13 + 1) as u8,
    };
    let mut last = 0;
    let variant = (seed % 19) as i16;
    header(out, &mut last, variant, wire(0, 12), false, 0);
    let mut last = 0;
    match variant {
        5 => {
            for id in 1..=2 {
                header(out, &mut last, id, wire(id as u32, 5), false, 0);
                varint(out, seed >> 8 & 0xf);
            }
        }
        7 | 8 => {
            header(out, &mut last, 1, 1, false, 0);
            header(out, &mut last, 2, wire(3, 12), false, 0);
            let mut unit = 0;
            header(
                out,
                &mut unit,
                (seed >> 8) as i16 % 3 + 1,
                wire(4, 12),
                false,
                0,
            );
            out.extend_from_slice(&[0, 0]);
        }
        _ => {}
    }
    out.extend_from_slice(&[0, 0]);
}

/// A field parquet-rs may or may not know: `id` and wire type `ty`. When
/// `crafted`, its value is of the type parquet-rs reads `id` as in the
/// struct it is put in (so a reader that skips by wire type loses its place).
#[derive(Debug, Clone)]
struct Junk {
    id: i16,
    ty: u8,
    crafted: bool,
    seed: u64,
}

fn junk() -> impl Strategy<Value = Junk> {
    // Field 10 is the logical type, the one nested struct parquet-rs reads.
    (
        prop_oneof![2 => -2i16..=20, 1 => Just(10)],
        1u8..=13,
        any::<bool>(),
        any::<u64>(),
    )
        .prop_map(|(id, ty, crafted, seed)| Junk {
            id,
            ty,
            crafted,
            seed,
        })
}

/// How parquet-rs reads a field of a struct the footer is built from.
#[derive(Clone, Copy)]
enum Read {
    /// A varint (5) or binary (8).
    Wire(u8),
    LogicalType,
    /// A list or struct: a minimal value of it parquet-rs accepts, longer
    /// than one byte so that skipping it by another wire type loses the
    /// place.
    Bytes(&'static [u8]),
}

/// How parquet-rs reads each field of a struct, by id.
type Reads = fn(i16) -> Option<Read>;

const FILE: Reads = |id| match id {
    1 | 3 => Some(Read::Wire(5)),
    // key_value_metadata: one KeyValue { key: "k" }.
    5 => Some(Read::Bytes(&[0x1C, 0x18, 1, b'k', 0])),
    6 => Some(Read::Wire(8)),
    // column_orders: one TYPE_ORDER.
    7 => Some(Read::Bytes(&[0x1C, 0x1C, 0, 0])),
    _ => None,
};
const SCHEMA_ELEMENT: Reads = |id| match id {
    1..=3 | 5..=9 => Some(Read::Wire(5)),
    4 => Some(Read::Wire(8)),
    10 => Some(Read::LogicalType),
    _ => None,
};
const ROW_GROUP: Reads = |id| match id {
    2 | 3 | 5 | 7 => Some(Read::Wire(5)),
    // sorting_columns: one SortingColumn { 0, false, false }.
    4 => Some(Read::Bytes(&[0x1C, 0x15, 0, 0x12, 0x12, 0])),
    _ => None,
};
const COLUMN_CHUNK: Reads = |id| match id {
    1 => Some(Read::Wire(8)),
    2 | 4..=7 => Some(Read::Wire(5)),
    _ => None,
};
const COLUMN_METADATA: Reads = |id| match id {
    1 | 4..=7 | 9..=11 | 14 | 15 => Some(Read::Wire(5)),
    // encodings: [PLAIN].
    2 => Some(Read::Bytes(&[0x15, 0])),
    // statistics: { null_count: 0 }.
    12 => Some(Read::Bytes(&[0x36, 0, 0])),
    // encoding_stats: one PageEncodingStats { 0, 0, 0 }.
    13 => Some(Read::Bytes(&[0x1C, 0x15, 0, 0x15, 0, 0x15, 0, 0])),
    // size_statistics: { unencoded_byte_array_data_bytes: 0 }.
    16 => Some(Read::Bytes(&[0x16, 0, 0])),
    // geospatial_statistics: { geospatial_types: [] }.
    17 => Some(Read::Bytes(&[0x29, 0x05, 0])),
    _ => None,
};

fn put(out: &mut Vec<u8>, last: &mut i16, j: &Junk, reads: Reads) {
    header(out, last, j.id, j.ty, false, 0);
    match reads(j.id).filter(|_| j.crafted) {
        Some(Read::LogicalType) => logical_type(out, j.seed),
        Some(Read::Bytes(bytes)) => out.extend_from_slice(bytes),
        Some(Read::Wire(ty)) => value(out, ty, j.seed, 0),
        None => value(out, j.ty, j.seed, 0),
    }
}

/// One `SchemaElement`.
#[derive(Debug, Clone)]
struct Element {
    before: Vec<Junk>,
    /// `num_children`: value, wire type, long-form id, wrap.
    children: Option<(i64, u8, bool, i64)>,
    after: Vec<Junk>,
    /// The stop byte's high nibble (parquet-rs stops on the low one).
    stop: u8,
}

fn element() -> impl Strategy<Value = Element> {
    let children = (
        prop_oneof![1i64..3, any::<i64>()],
        prop_oneof![3 => Just(5u8), 1 => 1u8..=13],
        any::<bool>(),
        0i64..3,
    );
    (
        prop::collection::vec(junk(), 0..2),
        prop::option::weighted(0.9, children),
        prop::collection::vec(junk(), 0..2),
        prop_oneof![3 => Just(0u8), 1 => 1u8..16],
    )
        .prop_map(|(before, children, after, stop)| Element {
            before,
            children,
            after,
            stop,
        })
}

/// Fields after the schema: a row group's (at each of its levels, when
/// there is one), the footer's after it, and a second row-group list of
/// this many.
#[derive(Debug, Clone, Default)]
struct Tail {
    row_group: Option<[Vec<Junk>; 3]>,
    after: Vec<Junk>,
    second: Option<u32>,
}

fn tail() -> impl Strategy<Value = Tail> {
    let junk = || prop::collection::vec(junk(), 0..2);
    (
        prop::option::of([junk(), junk(), junk()]),
        junk(),
        prop::option::of(1u32 << 20..1 << 31),
    )
        .prop_map(|(row_group, after, second)| Tail {
            row_group,
            after,
            second,
        })
}

/// One row group of one column chunk, `junk` put in its row group, column
/// chunk and column metadata.
fn row_group(out: &mut Vec<u8>, junk: &[Vec<Junk>; 3]) {
    let field = |out: &mut Vec<u8>, last: &mut i16, id: i16, ty: u8| {
        header(out, last, id, ty, false, 0);
    };
    let (mut group, mut chunk, mut meta) = (0, 0, 0);
    field(out, &mut group, 1, 9);
    out.push(0x1C);
    field(out, &mut chunk, 2, 6);
    varint(out, 0); // file_offset
    field(out, &mut chunk, 3, 12);
    field(out, &mut meta, 1, 5);
    varint(out, 2); // INT32
    field(out, &mut meta, 2, 9);
    out.extend_from_slice(&[0x15, 0]); // PLAIN
    field(out, &mut meta, 3, 9);
    out.extend_from_slice(&[0x18, 1, b'b']);
    for id in [4, 5, 6, 7, 9] {
        field(out, &mut meta, id, if id == 4 { 5 } else { 6 });
        varint(out, 0);
    }
    for j in &junk[2] {
        put(out, &mut meta, j, COLUMN_METADATA);
    }
    out.push(0);
    for j in &junk[1] {
        put(out, &mut chunk, j, COLUMN_CHUNK);
    }
    out.push(0);
    for id in [2, 3] {
        field(out, &mut group, id, 6);
        varint(out, 0);
    }
    for j in &junk[0] {
        put(out, &mut group, j, ROW_GROUP);
    }
    out.push(0);
}

fn footer(top: &[Junk], schema_ty: u8, elements: &[Element], tail: &Tail) -> Vec<u8> {
    let mut out = Vec::new();
    let mut last = 0;
    header(&mut out, &mut last, 1, 5, false, 0);
    varint(&mut out, zigzag(2));
    for j in top {
        put(&mut out, &mut last, j, FILE);
    }
    header(&mut out, &mut last, 2, schema_ty, false, 0);
    out.push((elements.len() as u8) << 4 | 12);
    for (i, e) in elements.iter().enumerate() {
        let mut last = 0;
        if i > 0 {
            header(&mut out, &mut last, 1, 5, false, 0);
            varint(&mut out, zigzag(1)); // INT32
            header(&mut out, &mut last, 3, 5, false, 0);
            varint(&mut out, 0); // REQUIRED
        }
        header(&mut out, &mut last, 4, 8, false, 0);
        out.extend_from_slice(&[1, b'a' + i as u8]);
        for j in &e.before {
            put(&mut out, &mut last, j, SCHEMA_ELEMENT);
        }
        if let Some((n, ty, long, wrap)) = e.children {
            header(&mut out, &mut last, 5, ty, long, wrap);
            varint(&mut out, zigzag(n));
        }
        for j in &e.after {
            put(&mut out, &mut last, j, SCHEMA_ELEMENT);
        }
        out.push(e.stop << 4);
    }
    header(&mut out, &mut last, 3, 6, false, 0);
    varint(&mut out, 0);
    header(&mut out, &mut last, 4, 9, false, 0);
    match &tail.row_group {
        Some(junk) => {
            out.push(0x1C);
            row_group(&mut out, junk);
        }
        None => out.push(0x0C),
    }
    for j in &tail.after {
        put(&mut out, &mut last, j, FILE);
    }
    if let Some(n) = tail.second {
        header(&mut out, &mut last, 4, 9, false, 0);
        out.push(0xFC);
        varint(&mut out, u64::from(n));
    }
    out.push(0);
    out
}

/// A schema of a group root and one INT32 leaf: what a row group needs.
fn honest_schema() -> Vec<Element> {
    let root = Element {
        before: vec![],
        children: Some((1, 5, false, 0)),
        after: vec![],
        stop: 0,
    };
    let leaf = Element {
        children: None,
        ..root.clone()
    };
    vec![root, leaf]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn a_passed_footer_reserves_no_more_than_it_holds(
        top in prop::collection::vec(junk(), 0..2),
        schema_ty in prop_oneof![3 => Just(9u8), 1 => 1u8..=13],
        elements in prop_oneof![
            2 => prop::collection::vec(element(), 1..4),
            1 => Just(honest_schema()),
        ],
        tail in tail(),
    ) {
        let footer = footer(&top, schema_ty, &elements, &tail);
        if tylertoo_io::precheck_footer(&footer).is_ok() {
            // Decoded structs cost a few hundred bytes per footer byte at
            // most; the reservations this guards against are gigabytes.
            let largest = largest_decode_allocation(&footer);
            prop_assert!(
                largest <= 1024 * footer.len() + (64 << 10),
                "{largest} bytes for a {}-byte footer {footer:02x?}",
                footer.len()
            );
        }
    }
}

/// An honest footer passes, so the property is not vacuous.
#[test]
fn an_honest_footer_passes_and_decodes() {
    let tail = Tail {
        row_group: Some(Default::default()),
        ..Tail::default()
    };
    let footer = footer(&[], 9, &honest_schema(), &tail);
    tylertoo_io::precheck_footer(&footer).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "#789: the honest footer must decode unchecked too"
    )]
    let metadata = parquet::file::metadata::ParquetMetaDataReader::decode_metadata(&footer);
    assert_eq!(metadata.unwrap().num_row_groups(), 1);
}

/// A footer whose schema is `groups` groups, each holding the next, around
/// one INT32 leaf, and no row groups.
fn nested_schema(groups: usize) -> Vec<u8> {
    let mut out = vec![0x15, 4, 0x19, 0xFC];
    varint(&mut out, groups as u64 + 1);
    // The root: name (field 4) and one child (field 5).
    out.extend_from_slice(&[0x48, 1, b'r', 0x15, 2, 0]);
    for _ in 1..groups {
        // repetition REQUIRED (3), name (4), one child (5).
        out.extend_from_slice(&[0x35, 0, 0x18, 1, b'g', 0x15, 2, 0]);
    }
    // The leaf: type INT32 (1), repetition REQUIRED (3), name (4).
    out.extend_from_slice(&[0x15, 2, 0x25, 0, 0x18, 1, b'x', 0]);
    // num_rows (3), an empty row-group list (4).
    out.extend_from_slice(&[0x16, 0, 0x19, 0x0C, 0]);
    out
}

/// parquet-rs builds the schema tree by recursion, one frame per level, so
/// a few kilobytes of nested groups overflow a thread's stack (#789): the
/// pre-check refuses a schema nested past 64 levels, and one nested to the
/// cap decodes on a 2 MiB stack.
#[test]
fn a_schema_nested_past_the_cap_is_refused() {
    let err = tylertoo_io::precheck_footer(&nested_schema(65)).unwrap_err();
    assert!(
        matches!(err, tylertoo_io::Error::HostileFooter(_)),
        "{err:?}"
    );

    let footer = nested_schema(64);
    tylertoo_io::precheck_footer(&footer).unwrap();
    let columns = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            #[expect(
                clippy::disallowed_methods,
                reason = "#789: a schema at the cap must decode unchecked"
            )]
            let metadata = parquet::file::metadata::ParquetMetaDataReader::decode_metadata(&footer);
            metadata
                .unwrap()
                .file_metadata()
                .schema_descr()
                .num_columns()
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(columns, 1);
}

/// The depth cap counts open groups, not groups: 100 sibling groups of one
/// leaf each, two levels deep, pass and decode.
#[test]
fn a_wide_shallow_schema_passes() {
    let mut out = vec![0x15, 4, 0x19, 0xFC];
    varint(&mut out, 201);
    // The root: name (4), 100 children (5).
    out.extend_from_slice(&[0x48, 1, b'r', 0x15]);
    varint(&mut out, zigzag(100));
    out.push(0);
    for _ in 0..100 {
        // A group: repetition REQUIRED (3), name (4), one child (5).
        out.extend_from_slice(&[0x35, 0, 0x18, 1, b'g', 0x15, 2, 0]);
        // Its leaf: type INT32 (1), repetition REQUIRED (3), name (4).
        out.extend_from_slice(&[0x15, 2, 0x25, 0, 0x18, 1, b'x', 0]);
    }
    out.extend_from_slice(&[0x16, 0, 0x19, 0x0C, 0]);
    tylertoo_io::precheck_footer(&out).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "#789: a wide schema must decode unchecked too"
    )]
    let metadata = parquet::file::metadata::ParquetMetaDataReader::decode_metadata(&out);
    assert_eq!(
        metadata
            .unwrap()
            .file_metadata()
            .schema_descr()
            .num_columns(),
        100
    );
}
