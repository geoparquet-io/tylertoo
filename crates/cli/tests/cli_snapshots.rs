//! Snapshot tests of the CLI's user-visible surface (#613): each
//! `tests/cmd/*.md` case runs `tylertoo` as a black box and compares its
//! stdout, stderr, and exit code against the console blocks in the file.
//!
//! Update with `TRYCMD=overwrite cargo test -p tylertoo --test cli_snapshots`,
//! then review the diff by hand: a wrong capture pins a bug as correct
//! behaviour. See DEVELOPMENT.md, "Updating CLI snapshots".

#[test]
fn cli_snapshots() {
    trycmd::TestCases::new()
        .register_bin(
            "tylertoo",
            std::path::Path::new(env!("CARGO_BIN_EXE_tylertoo")),
        )
        // Log lines carry timestamps, core counts, RSS, and elapsed times;
        // the cases pin what the CLI prints, not what core logs.
        .env("RUST_LOG", "off")
        .case("tests/cmd/*.md");
}
