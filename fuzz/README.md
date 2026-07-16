# tylertoo fuzz targets

`cargo-fuzz` targets for the untrusted-bytes surfaces of `tylertoo-core`
(issue #187). This crate is a **standalone workspace** — it is not a member
of the root workspace and never affects `cargo build/check/test --workspace`
or per-PR CI time.

## Targets

| Target | Surface |
|--------|---------|
| `footer_json` | `overview::level::OverviewsMeta::from_json` — the `overviews` footer JSON read verbatim from any input file |
| `wkb` | `wkb::wkb_to_geometry` — WKB geometry blobs read verbatim from any input file |

## Running locally

Requires nightly and `cargo-fuzz`:

```bash
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run footer_json -- -max_total_time=60
cargo +nightly fuzz run wkb -- -max_total_time=60
```

Crashing inputs land in `fuzz/artifacts/<target>/`; minimize with
`cargo +nightly fuzz tmin <target> <artifact>` and add the minimized case
as a deterministic regression test in core.

## CI wiring (scheduled, not per-PR)

To be wired into scheduled CI alongside #182 (workflow files are owned by
that effort). Suggested job:

```yaml
# .github/workflows/fuzz.yml (scheduled)
# schedule: cron weekly
# - dtolnay/rust-toolchain@nightly
# - cargo install cargo-fuzz
# - cd fuzz && for t in footer_json wkb; do
#     cargo +nightly fuzz run "$t" -- -max_total_time=300; done
# - upload fuzz/artifacts/ on failure
```
