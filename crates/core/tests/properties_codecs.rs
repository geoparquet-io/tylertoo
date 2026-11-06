//! Properties at the owners' invariants: tile math, the varint / zigzag /
//! directory codecs, MVT geometry commands, and the readers of untrusted
//! bytes (#677; replaces mutation testing). Case counts are capped so the
//! file stays in the quick tier.
use geo::{Coord, LineString, MultiLineString};
use proptest::prelude::*;
use tylertoo_core::mvt::{
    command_decode, command_encode, encode_linestring, encode_multi_linestring, geo_to_tile_coords,
    zigzag_decode, zigzag_encode,
};
use tylertoo_core::pmtiles_writer::{
    decode_directory, decode_varint, encode_directory, encode_varint, tile_id, tile_id_to_zxy,
    DirEntry, Header,
};
use tylertoo_core::tile::{tile_bounds, tiles_per_axis};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn zigzag_round_trips(n in any::<i32>()) {
        prop_assert_eq!(zigzag_decode(zigzag_encode(n)), n);
    }

    #[test]
    fn command_round_trips(id in 0u32..8, count in 0u32..(1 << 29)) {
        prop_assert_eq!(command_decode(command_encode(id, count)), (id, count));
    }

    #[test]
    fn varint_round_trips_and_is_minimal(v in any::<u64>()) {
        let mut buf = Vec::new();
        encode_varint(v, &mut buf);
        prop_assert_eq!(decode_varint(&buf), Some((v, buf.len())));
        // Trailing bytes are not consumed.
        buf.push(0xAB);
        prop_assert_eq!(decode_varint(&buf).map(|(x, _)| x), Some(v));
    }

    #[test]
    fn varint_decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..16)) {
        if let Some((_, used)) = decode_varint(&bytes) {
            prop_assert!(used >= 1 && used <= bytes.len().min(10));
        }
    }

    #[test]
    fn directory_round_trips(
        raw in prop::collection::vec(
            (1u64..1_000_000, 0u64..1 << 40, any::<u32>(), any::<u32>()), 0..40),
    ) {
        let mut id = 0u64;
        let entries: Vec<DirEntry> = raw
            .into_iter()
            .map(|(d, offset, length, run_length)| {
                id += d;
                DirEntry { tile_id: id, offset, length, run_length }
            })
            .collect();
        let back = decode_directory(&encode_directory(&entries)).expect("own output decodes");
        prop_assert_eq!(back.len(), entries.len());
        for (a, b) in entries.iter().zip(&back) {
            prop_assert_eq!((a.tile_id, a.offset, a.length, a.run_length),
                            (b.tile_id, b.offset, b.length, b.run_length));
        }
    }

    #[test]
    fn contiguous_directory_entries_round_trip(
        lens in prop::collection::vec(1u32..5000, 1..40),
    ) {
        // The spec's offset-0 shorthand: back-to-back entries.
        let mut offset = 0u64;
        let entries: Vec<DirEntry> = lens
            .iter()
            .enumerate()
            .map(|(i, &length)| {
                let e = DirEntry { tile_id: i as u64, offset, length, run_length: 1 };
                offset += u64::from(length);
                e
            })
            .collect();
        let back = decode_directory(&encode_directory(&entries)).unwrap();
        prop_assert_eq!(back.iter().map(|e| e.offset).collect::<Vec<_>>(),
                        entries.iter().map(|e| e.offset).collect::<Vec<_>>());
    }

    #[test]
    fn untrusted_pmtiles_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode_directory(&bytes);
        let _ = Header::from_bytes(&bytes);
    }

    #[test]
    fn tiles_per_axis_is_a_power_of_two_clamped_at_2_pow_32(z in any::<u8>()) {
        prop_assert_eq!(tiles_per_axis(z), 1u64 << z.min(32));
    }

    #[test]
    fn tile_id_round_trips(z in 0u8..=31, fx in 0.0f64..1.0, fy in 0.0f64..1.0) {
        let n = tiles_per_axis(z);
        let (x, y) = (((n as f64 * fx) as u64).min(n - 1) as u32, ((n as f64 * fy) as u64).min(n - 1) as u32);
        prop_assert_eq!(tile_id_to_zxy(tile_id(z, x, y)).unwrap(), (z, x, y));
    }

    #[test]
    fn tile_bounds_are_ordered_and_tile_the_world(z in 0u8..=30, fx in 0.0f64..1.0, fy in 0.0f64..1.0) {
        let n = tiles_per_axis(z);
        prop_assert_eq!(n, 1u64 << z);
        let x = ((n as f64 * fx) as u64).min(n - 1) as u32;
        let y = ((n as f64 * fy) as u64).min(n - 1) as u32;
        let b = tile_bounds(x, y, z);
        prop_assert!(b.lng_min < b.lng_max && b.lat_min < b.lat_max);
        prop_assert!(b.lng_min >= -180.0 && b.lng_max <= 180.0);
        prop_assert!(b.lat_min >= -85.06 && b.lat_max <= 85.06);
        // The grid covers the world: first column starts at -180, last ends
        // at 180; the top row reaches the Web Mercator limit.
        if x == 0 {
            prop_assert_eq!(b.lng_min, -180.0);
        }
        if u64::from(x) + 1 == n {
            prop_assert_eq!(b.lng_max, 180.0);
        }
        if y == 0 {
            prop_assert!((b.lat_max - 85.0511287798).abs() < 1e-6, "{}", b.lat_max);
        }
        // Neighbours share an edge exactly.
        if u64::from(x) + 1 < n {
            prop_assert_eq!(tile_bounds(x + 1, y, z).lng_min, b.lng_max);
        }
        if u64::from(y) + 1 < n {
            prop_assert_eq!(tile_bounds(x, y + 1, z).lat_max, b.lat_min);
        }
    }
}

/// Decode a LineString / MultiLineString command stream back to per-line
/// vertices (the inverse of the encoder, for the round trip below).
fn decode_lines(cmds: &[u32]) -> Vec<Vec<(i32, i32)>> {
    let (mut lines, mut cur, mut i) = (Vec::new(), (0i32, 0i32), 0);
    while i < cmds.len() {
        let (id, count) = command_decode(cmds[i]);
        i += 1;
        if id == 1 {
            lines.push(Vec::new());
        }
        for _ in 0..count {
            cur.0 += zigzag_decode(cmds[i]);
            cur.1 += zigzag_decode(cmds[i + 1]);
            i += 2;
            lines.last_mut().unwrap().push(cur);
        }
    }
    lines
}

fn line() -> impl Strategy<Value = Vec<(f64, f64)>> {
    prop::collection::vec((0.0f64..1.0, 0.0f64..1.0), 2..12)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mvt_line_commands_round_trip(
        z in 0u8..=14, extent in prop::sample::select(vec![256u32, 4096, 8192]),
        lines in prop::collection::vec(line(), 1..4),
    ) {
        let b = tile_bounds(0, 0, z);
        let to_ls = |pts: &[(f64, f64)]| LineString::from(
            pts.iter()
                .map(|&(fx, fy)| Coord {
                    x: b.lng_min + fx * (b.lng_max - b.lng_min),
                    y: b.lat_min + fy * (b.lat_max - b.lat_min),
                })
                .collect::<Vec<_>>());
        let ml = MultiLineString::new(lines.iter().map(|l| to_ls(l)).collect());
        let expected: Vec<Vec<(i32, i32)>> = ml.0.iter()
            .map(|l| l.0.iter().map(|c| geo_to_tile_coords(c.x, c.y, &b, extent)).collect())
            .collect();
        prop_assert_eq!(decode_lines(&encode_multi_linestring(&ml, &b, extent)), expected.clone());
        prop_assert_eq!(decode_lines(&encode_linestring(&ml.0[0], &b, extent)), vec![expected[0].clone()]);
    }
}

/// The corners of the grid at every zoom, so the whole-world checks do not
/// depend on the random draw landing on an edge tile.
#[test]
fn grid_corners_cover_the_world_at_every_zoom() {
    for z in 0u8..=30 {
        let last = (tiles_per_axis(z) - 1) as u32;
        assert_eq!(tile_bounds(0, 0, z).lng_min, -180.0, "z{z}");
        assert_eq!(tile_bounds(last, last, z).lng_max, 180.0, "z{z}");
        assert!(
            (tile_bounds(0, 0, z).lat_max - 85.0511287798).abs() < 1e-6,
            "z{z}"
        );
        assert!(
            (tile_bounds(last, last, z).lat_min + 85.0511287798).abs() < 1e-6,
            "z{z}"
        );
    }
}
