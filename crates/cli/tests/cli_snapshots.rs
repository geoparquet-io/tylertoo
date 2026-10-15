//! Snapshot tests of the CLI's user-visible surface (#613): each
//! `tests/cmd/*.md` case runs `tylertoo` as a black box and compares its
//! stdout, stderr, and exit code against the console blocks in the file.
//!
//! Update with `TRYCMD=overwrite cargo test -p tylertoo --test cli_snapshots`,
//! then review the diff by hand: a wrong capture pins a bug as correct
//! behavior. See DEVELOPMENT.md, "Updating CLI snapshots".

use std::path::Path;

#[test]
fn cli_snapshots() {
    trycmd::TestCases::new()
        .register_bin("tylertoo", Path::new(env!("CARGO_BIN_EXE_tylertoo")))
        // Log lines carry timestamps, core counts, RSS, and elapsed times;
        // the cases pin what the CLI prints, not what core logs.
        .env("RUST_LOG", "off")
        // A CLICOLOR_FORCE=1 inherited from the caller's shell would put ANSI
        // escapes in clap's help and errors; NO_COLOR takes precedence.
        .env("NO_COLOR", "1")
        // CI exports RUST_BACKTRACE=1, under which anyhow appends a stack
        // backtrace to every `Error: ...`. This turns off error backtraces
        // only: a panic in a case still prints one.
        .env("RUST_LIB_BACKTRACE", "0")
        // CI runs this under `--all-features`, which compiles in the dhat
        // profiler: on exit it prints a heap summary to stderr and writes
        // dhat-heap.json into the working directory (crates/cli, for the
        // unsandboxed cases).
        .env("TYLERTOO_DHAT", "off")
        .case("tests/cmd/*.md");
}

/// The sandboxed cases run on copies of a committed fixture, since a
/// sandbox can only see its `<case>.in/` directory. Keep the copies equal to
/// the source so the cases test the fixture their prose names.
#[test]
fn sandbox_inputs_match_their_source_fixture() {
    let source = Path::new("../../tests/fixtures/properties/overture-names.parquet");
    let expected = std::fs::read(source).expect("read the source fixture");
    for copy in [
        "tests/cmd/errors.in/overture-names.parquet",
        "tests/cmd/pipeline.in/overture-names.parquet",
    ] {
        let actual = std::fs::read(copy).expect("read the sandbox copy");
        assert!(
            actual == expected,
            "{copy} differs from {}; copy it again",
            source.display()
        );
    }
}
