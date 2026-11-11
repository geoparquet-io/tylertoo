use super::*;
use arrow_array::{BinaryArray, FixedSizeListArray, StructArray};
use arrow_schema::{Field, Fields};

fn crs(id: &str) -> CrsInfo {
    CrsInfo::from_identifier(id)
}

fn t(id: &str) -> ToLonLat {
    ToLonLat::for_crs(&crs(id)).expect(id)
}

/// Within ~1 m of pyproj (`Transformer.from_crs(epsg, 4326, always_xy=True)`).
fn near(got: (f64, f64), want: (f64, f64)) {
    assert!(
        (got.0 - want.0).abs() < 1e-5 && (got.1 - want.1).abs() < 1e-5,
        "got {got:?}, pyproj {want:?}"
    );
}

#[test]
fn epsg_codes_from_every_spelling() {
    for id in [
        "EPSG:28992",
        "epsg:28992",
        "urn:ogc:def:crs:EPSG::28992",
        "http://www.opengis.net/def/crs/EPSG/0/28992",
        "srid:28992",
    ] {
        assert_eq!(epsg_code(id), Some(28992), "{id}");
    }
    for id in ["ESRI:54009", "Amersfoort / RD New", "EPSG:", "EPSG:999999"] {
        assert_eq!(epsg_code(id), None, "{id}");
    }
}

#[test]
fn points_match_pyproj() {
    near(
        t("EPSG:28992").point(113_682.264_567, 384_710.303_691),
        (4.792_807_404, 51.449_963_593),
    );
    near(
        t("EPSG:31370").point(179_480.369_423, 236_905.310_413),
        (4.792_725_869, 51.441_038_549),
    );
    near(t("EPSG:3035").point(4_321_000.0, 3_210_000.0), (10.0, 52.0));
    near(
        t("EPSG:32633").point(500_000.0, 5_500_000.0),
        (15.0, 49.652_542_922),
    );
    // A geographic source other than WGS 84: degrees in, degrees out.
    near(t("EPSG:4258").point(10.0, 50.0), (10.0, 50.0));
}

#[test]
fn unresolvable_crs_has_no_transform() {
    // No id, an unknown code, a geocentric CRS, and no declaration at all.
    for id in ["Amersfoort / RD New", "EPSG:1", "EPSG:4978"] {
        assert!(ToLonLat::for_crs(&crs(id)).is_none(), "{id}");
    }
    assert!(ToLonLat::for_crs(&CrsInfo::unknown()).is_none());
}

fn wkb_polygon(le: bool, ring: &[(f64, f64)]) -> Vec<u8> {
    let mut v = vec![u8::from(le)];
    let u = |n: u32| if le { n.to_le_bytes() } else { n.to_be_bytes() };
    let f = |n: f64| if le { n.to_le_bytes() } else { n.to_be_bytes() };
    v.extend(u(3));
    v.extend(u(1));
    v.extend(u(ring.len() as u32));
    for &(x, y) in ring {
        v.extend(f(x));
        v.extend(f(y));
    }
    v
}

fn wkb_xy(b: &[u8], at: usize, le: bool) -> (f64, f64) {
    let f = |at: usize| {
        let a: [u8; 8] = b[at..at + 8].try_into().unwrap();
        if le {
            f64::from_le_bytes(a)
        } else {
            f64::from_be_bytes(a)
        }
    };
    (f(at), f(at + 8))
}

#[test]
fn wkb_batches_are_rewritten_in_place() {
    let rd = t("EPSG:28992");
    let corner = (113_682.264_567_f64, 384_710.303_691_f64);
    let ring = [
        corner,
        (114_682.0, 384_710.0),
        (114_682.0, 385_710.0),
        corner,
    ];
    // An EWKB ZM point with an SRID is still one x/y pair.
    let mut ewkb = vec![1u8];
    ewkb.extend((0x2000_0000u32 | 0xC000_0000 | 1).to_le_bytes());
    ewkb.extend(28992u32.to_le_bytes());
    for v in [corner.0, corner.1, 7.0, 9.0] {
        ewkb.extend(v.to_le_bytes());
    }
    let mut collection = vec![0u8];
    collection.extend(7u32.to_be_bytes());
    collection.extend(1u32.to_be_bytes());
    collection.extend(wkb_polygon(false, &ring));
    let array = BinaryArray::from_opt_vec(vec![
        Some(&wkb_polygon(true, &ring)[..]),
        None,
        Some(&ewkb[..]),
        Some(&collection[..]),
        Some(&[1u8, 3, 0][..]),
    ]);
    let schema = Arc::new(arrow_schema::Schema::new(vec![Field::new(
        "geometry",
        DataType::Binary,
        true,
    )]));
    let batch = RecordBatch::try_new(schema, vec![Arc::new(array)]).unwrap();
    let out = rd.batch(batch, "geometry").unwrap();
    let out = out.column(0).as_binary::<i32>();
    let want = (4.792_807_404, 51.449_963_593);
    near(wkb_xy(out.value(0), 13, true), want);
    assert!(out.is_null(1));
    near(wkb_xy(out.value(2), 9, true), want);
    assert_eq!(wkb_xy(out.value(2), 25, true), (7.0, 9.0), "z and m stay");
    near(wkb_xy(out.value(3), 9 + 13, false), want);
    assert_eq!(out.value(4)[0], 0xFF, "malformed WKB is marked, not read");
}

#[test]
fn native_coordinates_are_rewritten() {
    let rd = t("EPSG:28992");
    let (x, y) = (113_682.264_567, 384_710.303_691);
    let want = (4.792_807_404, 51.449_963_593);

    let item = Arc::new(Field::new("xy", DataType::Float64, false));
    let interleaved = FixedSizeListArray::new(
        item,
        2,
        Arc::new(Float64Array::from(vec![x, y, x, y])),
        None,
    );
    let out = rd.array(&(Arc::new(interleaved) as ArrayRef)).unwrap();
    let values = out
        .as_fixed_size_list()
        .values()
        .as_primitive::<arrow_array::types::Float64Type>()
        .clone();
    near((values.value(2), values.value(3)), want);

    let fields = Fields::from(vec![
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]);
    let separated = StructArray::new(
        fields,
        vec![
            Arc::new(Float64Array::from(vec![x])),
            Arc::new(Float64Array::from(vec![y])),
        ],
        None,
    );
    let out = rd.array(&(Arc::new(separated) as ArrayRef)).unwrap();
    let s = out.as_struct();
    let f = |i: usize| {
        s.column(i)
            .as_primitive::<arrow_array::types::Float64Type>()
            .value(0)
    };
    near((f(0), f(1)), want);
}

#[test]
fn bbox_holds_the_image_and_refuses_a_pole() {
    let rd = t("EPSG:28992");
    let b = rd
        .bbox([
            113_682.264_567,
            384_710.303_691,
            114_682.264_567,
            385_710.303_691,
        ])
        .unwrap();
    let corner = (4.792_807_404, 51.449_963_593);
    assert!(b[0] <= corner.0 && corner.0 <= b[2] && b[1] <= corner.1 && corner.1 <= b[3]);
    assert!(b[2] - b[0] < 0.02 && b[3] - b[1] < 0.02, "{b:?}");
    // Antarctic polar stereographic around the pole.
    assert!(t("EPSG:3031").bbox([-1e5, -1e5, 1e5, 1e5]).is_none());
}
