# hostile-wagyu — the wagyu-rs runner for the #205 clipping evaluation

The out-of-process half of `crates/core/tests/hostile_geometry_eval.rs`.
It clips the polygon cases the in-tree harness dumps with
[wagyu-rs](https://crates.io/crates/wagyu-rs) 0.2.1 and writes the results
back for the harness to score with the same oracles as the in-tree engines.

## Why out of process

wagyu-rs 0.2.1 declares `geo = "0.32"` as a dependency that its sources never
use (only `geo-types`). geo 0.32 pins `i_overlay >=4.0, <4.1`; tylertoo's geo
0.33 pins `i_overlay >=4.5.1, <4.6`; cargo cannot hold two semver-compatible
4.x versions of one crate, so the workspace fails to resolve the moment
wagyu-rs is added — even as an optional dependency behind an off-by-default
feature, since cargo resolves optional dependencies regardless of activation.
Upstream: [nlebovits/wagyu-rs#113](https://github.com/nlebovits/wagyu-rs/issues/113).
This crate is excluded from the workspace (root `Cargo.toml`) and has its own
lockfile.

## Run

```bash
# 1. dump the cases (also scores the in-tree engines)
cargo test --release -p tylertoo-core \
  --test hostile_geometry_eval full_scorecard

# 2. clip them with wagyu-rs
cargo run --release \
  --manifest-path corpus/hostile_wagyu/Cargo.toml

# 3. fold the wagyu columns into the scorecard
cargo test --release -p tylertoo-core \
  --test hostile_geometry_eval full_scorecard
cat target/hostile_geometry_eval/SCORECARD.md
```

`tests/adapter_sanity.rs` pins that the runner drives wagyu-rs correctly
(integer squares clip to their overlap under both fill rules and every ring
orientation) and why there is no degrees-as-is `Wagyu<f64>` column (f64 input
is snap-rounded to integers).

The decision the scorecard led to is in `corpus/HOSTILE_GEOMETRY.md` and
`context/ARCHITECTURE.md` ("Clipping engine (#205)").
