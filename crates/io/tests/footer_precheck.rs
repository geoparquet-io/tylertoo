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
/// `crafted`, its value is of the type parquet-rs reads `id` as in a
/// `SchemaElement` (so a reader that skips by wire type loses its place).
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

fn put(out: &mut Vec<u8>, last: &mut i16, j: &Junk) {
    header(out, last, j.id, j.ty, false, 0);
    match (j.crafted, j.id) {
        (true, 10) => logical_type(out, j.seed),
        (true, 4) => value(out, 8, j.seed, 0),
        (true, 1..=9) => value(out, 5, j.seed, 0),
        _ => value(out, j.ty, j.seed, 0),
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

fn footer(top: &[Junk], schema_ty: u8, elements: &[Element]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut last = 0;
    header(&mut out, &mut last, 1, 5, false, 0);
    varint(&mut out, zigzag(2));
    for j in top {
        put(&mut out, &mut last, j);
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
            put(&mut out, &mut last, j);
        }
        if let Some((n, ty, long, wrap)) = e.children {
            header(&mut out, &mut last, 5, ty, long, wrap);
            varint(&mut out, zigzag(n));
        }
        for j in &e.after {
            put(&mut out, &mut last, j);
        }
        out.push(e.stop << 4);
    }
    header(&mut out, &mut last, 3, 6, false, 0);
    varint(&mut out, 0);
    header(&mut out, &mut last, 4, 9, false, 0);
    out.push(0x0C); // no row groups
    out.push(0);
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn a_passed_footer_reserves_no_more_than_it_holds(
        top in prop::collection::vec(junk(), 0..2),
        schema_ty in prop_oneof![3 => Just(9u8), 1 => 1u8..=13],
        elements in prop::collection::vec(element(), 1..4),
    ) {
        let footer = footer(&top, schema_ty, &elements);
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
    let element = Element {
        before: vec![],
        children: Some((1, 5, false, 0)),
        after: vec![],
        stop: 0,
    };
    let leaf = Element {
        children: None,
        ..element.clone()
    };
    let footer = footer(&[], 9, &[element, leaf]);
    tylertoo_io::precheck_footer(&footer).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "#789: the honest footer must decode unchecked too"
    )]
    parquet::file::metadata::ParquetMetaDataReader::decode_metadata(&footer).unwrap();
}
