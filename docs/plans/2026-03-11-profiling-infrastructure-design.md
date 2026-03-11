# Profiling Infrastructure Design

**Date:** 2026-03-11
**Status:** Approved
**Related Issues:** #32 (memory), #41 (GeoParquet reading), #90 (parallel encoding)

## Overview

Add profiling infrastructure to gpq-tiles to help evaluate performance issues. Two independent PRs will be implemented in parallel:

| PR | Branch | Focus |
|----|--------|-------|
| Time Profiling | `profiling/time` | Phase-level timing with tracing |
| Memory Profiling | `profiling/memory` | Heap allocation tracking with dhat |

## Time Profiling

### Dependencies

```toml
# crates/core/Cargo.toml
[dependencies]
tracing = "0.1"

[dev-dependencies]
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
tracing-chrome = "0.7"
```

### Migration

Replace `log` crate with `tracing` (API-compatible). Add `tracing-log` bridge for dependencies still using `log`.

### Instrumentation

Add `#[instrument]` spans to key functions:

| Function | Span name | Fields |
|----------|-----------|--------|
| `generate_tiles_to_writer()` | `pipeline` | `min_zoom`, `max_zoom`, `feature_count` |
| `process_row_group()` | `row_group` | `index`, `row_count` |
| `extract_geometries()` | `read_parquet` | `path`, `geometry_count` |
| `simplify_geometry()` | `simplify` | `tolerance` |
| `clip_to_tile()` | `clip` | `tile_coord` |
| `encode_tile()` | `encode` | `tile_coord`, `feature_count` |

### CLI Flags

```
--profile          Enable console timing summary
--trace-output F   Write Chrome trace JSON to file F
```

### Console Output

```
Profiling summary:
  pipeline          12.4s  100%
  ├─ read_parquet    2.8s   23%
  ├─ simplify        4.9s   40%
  ├─ clip            1.9s   15%
  └─ encode          2.7s   22%
```

### Architecture

- Tracing spans always compiled in (zero-cost when no subscriber)
- Runtime opt-in via `--profile` or `--trace-output`
- Chrome trace JSON viewable in `chrome://tracing` or Perfetto

## Memory Profiling

### Dependencies

```toml
# crates/core/Cargo.toml
[features]
dhat-heap = ["dhat"]

[dependencies]
dhat = { version = "0.3", optional = true }
```

### Implementation

**Global allocator (lib.rs):**
```rust
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;
```

**Profiler initialization (main.rs):**
```rust
fn main() {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    // ... rest of main
}
```

### Usage

```bash
# Build with profiling
cargo build --release --features dhat-heap

# Run (outputs dhat-heap.json on exit)
./target/release/gpq-tiles input.parquet output.pmtiles

# Analyze in web viewer
open https://nnethercote.github.io/dh_view/dh_view.html
```

### What dhat Shows

- Total bytes allocated
- Peak heap usage
- Allocation sites ranked by bytes
- Call stacks for largest allocators

## Testing

### Time Profiling

- Unit test that spans are emitted using `tracing-test`
- Integration test that `--trace-output` produces valid JSON

### Memory Profiling

- CI job builds with `--features dhat-heap` (compile check only)
- Manual validation produces `dhat-heap.json`

## Documentation

Add `docs/PROFILING.md` covering:
- How to use `--profile` and `--trace-output`
- How to build and use dhat heap profiling
- How to interpret outputs

## Success Criteria

- [ ] `--profile` shows phase timing breakdown
- [ ] `--trace-output` produces valid Chrome trace JSON
- [ ] `--features dhat-heap` builds and produces heap profile
- [ ] Can identify top allocators for issue #32
