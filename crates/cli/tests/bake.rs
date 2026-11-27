//! Integration test for `tylertoo bake` and `tylertoo tiles --bake`: a
//! PMTiles archive baked into a copy of its GeoParquet input
//! (`geo:pmtiles`), read back by `stats` and `decode`, and stripped back
//! to the original bytes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tylertoo(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tylertoo"))
        .args(args)
        .output()
        .expect("run tylertoo")
}

fn ok(args: &[&str]) -> String {
    let out = tylertoo(args);
    assert!(
        out.status.success(),
        "{args:?} exited with {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn input() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/streaming/multi-rowgroup-small.parquet")
}

fn s(p: &Path) -> &str {
    p.to_str().expect("UTF-8 path")
}

#[test]
fn tiles_bake_writes_a_copy_that_serves_the_archive_and_strips_back() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, baked) = (dir.path().join("t.pmtiles"), dir.path().join("t.parquet"));
    let stdout = ok(&[
        "tiles",
        s(&input()),
        s(&archive),
        "--max-zoom",
        "8",
        "--bake",
        s(&baked),
    ]);
    assert!(stdout.contains("Baked"), "{stdout}");

    // The baked copy holds the archive: `stats` reads it the same.
    assert_eq!(
        ok(&["stats", s(&baked), "--json"]),
        ok(&["stats", s(&archive), "--json"])
    );

    // `bake --strip` gives back the input, byte for byte.
    let plain = dir.path().join("plain.parquet");
    ok(&["bake", "--strip", s(&baked), "-o", s(&plain)]);
    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(input()).unwrap()
    );

    // And `bake` puts the same archive back.
    let again = dir.path().join("again.parquet");
    ok(&["bake", s(&plain), s(&archive), "-o", s(&again)]);
    assert_eq!(
        std::fs::read(&again).unwrap(),
        std::fs::read(&baked).unwrap()
    );
}

#[test]
fn decode_reads_a_baked_file_like_its_archive() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, baked) = (dir.path().join("t.pmtiles"), dir.path().join("t.parquet"));
    ok(&["tiles", s(&input()), s(&archive), "--max-zoom", "6"]);
    ok(&["bake", s(&input()), s(&archive), "-o", s(&baked)]);
    let (a, b) = (dir.path().join("a.parquet"), dir.path().join("b.parquet"));
    let from_archive = ok(&["decode", s(&archive), s(&a), "--zoom", "6"]);
    let from_baked = ok(&["decode", s(&baked), s(&b), "--zoom", "6"]);
    let counts = |out: &str| {
        out.lines()
            .last()
            .unwrap_or_default()
            .split(" in ")
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(counts(&from_archive), counts(&from_baked));
}

#[test]
fn bake_refuses_what_it_cannot_do_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.pmtiles");
    ok(&["tiles", s(&input()), s(&archive), "--max-zoom", "4"]);
    let out = dir.path().join("out.parquet");
    let input = input();
    let cases: [(&[&str], &str); 4] = [
        // Not a GeoParquet file to bake into.
        (
            &["bake", s(&archive), s(&archive), "-o", s(&out)],
            "not a Parquet file",
        ),
        // Nothing to strip.
        (
            &["bake", "--strip", s(&input), "-o", s(&out)],
            "holds no baked",
        ),
        // The archive is required without --strip.
        (&["bake", s(&input), "-o", s(&out)], "ARCHIVE"),
        // --bake onto the input itself.
        (
            &[
                "tiles",
                s(&input),
                s(&archive),
                "--force",
                "--bake",
                s(&input),
            ],
            "would overwrite",
        ),
    ];
    for (args, want) in cases {
        let run = tylertoo(args);
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(!run.status.success(), "{args:?} succeeded");
        assert!(stderr.contains(want), "{args:?}: {stderr}");
    }
    assert!(!out.exists());

    // A plain GeoParquet file is not an archive, and `stats` says why.
    let run = tylertoo(&["stats", s(&input)]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("geo:pmtiles"));
}
