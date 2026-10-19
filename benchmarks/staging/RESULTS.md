# Remote staging sweep (#290)

Measured 2026-10-19 at commit `7758606`. Pass 0 stages a remote input's
selected row groups to local disk before the passes read them. This sweep
varies three things: the staging concurrency, the coalescing window, and the
HTTP client. It runs them on two real hosts serving the same object and on
two emulated hosts.

## Headline findings

1. **Keep the defaults: concurrency 8, no coalescing window.** On both real
   hosts the default cell is at or within 0.1 s of the best: 0.3 s on
   source.coop and 0.2 s on S3 for a 123 MB object. Every window is
   neutral or slower there.
2. **The best window depends on the host.** It wins only where time to first
   byte (TTFB) dominates and throughput per connection doesn't. On the
   emulated 1 s-TTFB host, a 64 MiB window cut staging from 3.0 s to 1.1 s.
   On the emulated throttled host (4 MB/s per connection) it doubled
   staging at the default concurrency, 7.9 s → 16.7 s, because fewer,
   bigger spans mean fewer connections. The knob stays env-only:
   `TYLERTOO_STAGING_COALESCE_BYTES=67108864` for a high-TTFB host that does
   not throttle each connection.
3. **A large window can turn staging into a failure.** A 128 MiB span at
   4 MB/s takes about 30 s, the `object_store` default request timeout. The
   request is cut off, staging gives up on the part, and the passes fall
   back to per-chunk reads: 218 requests and 292 s instead of 17 s. Raise
   `TYLERTOO_HTTP_TIMEOUT` with a large window on a slow host. A single row
   group larger than 30 s of transfer has the same exposure without any
   window.
4. **The HTTP/2 hypothesis does not hold for our client.** `object_store`
   defaults to HTTP/1.1, so concurrency 8 opens 8 connections (the emulated
   server counted 9 including the footer read). Forcing HTTP/2 on
   source.coop was no faster (0.6 s vs 0.4 s at concurrency 8), and S3 does
   not negotiate HTTP/2 at all.
5. **Parallelism does not hurt on source.coop from this host.** The issue
   measured 3.75 MB/s staging vs 11.1 MB/s single-stream `curl`. Here
   staging at concurrency 8 moved 123 MB in 0.3 s, about 4× faster than a
   single-stream `curl` of the whole object (1.4 s). The issue's numbers came
   from a client outside AWS. See Caveats.
6. **Partitions are pruned across the whole prefix.** A regional `--bbox`
   over all 27 Brazilian state files of FTW `global-data` (11.06 GiB) read
   5 of 861 row groups and fetched 1.1 % of the bytes in 59 requests: two
   footer reads per part plus 5 row groups. Every non-intersecting state was
   pruned from its footer alone.

## Environment

| | |
|---|---|
| Host | AWS c7g.16xlarge (Graviton3, 64 vCPU, 128 GiB), us-west-2 |
| tylertoo | 0.8.0 + this branch, `cargo build --release -p tylertoo` (no `dhat-heap`) |
| HTTP stack | `object_store` 0.14.2, reqwest 0.13, HTTP/1.1 by default |
| Object | `ftw/global-data-2e/vector/2017/zone=25/utm25.parquet`: 123 MB, 18 row groups of about 6.8 MB, 146,042 polygons |
| source.coop | `https://data.source.coop/<key>`, through Cloudflare (PDX) |
| S3 | `s3://us-west-2.opendata.source.coop/<key>`, anonymous, the bucket behind the proxy |
| Emulated hosts | `benchmarks/overview/logging_server.py` on localhost with `TTFB_MS` / `CONN_BW_MBPS` |

The S3 bucket is the storage source.coop proxies for this key, so both real
hosts serve identical bytes.

## Method

- Each cell is a fresh `tylertoo overview <url> out.parquet --min-zoom 0
  --max-zoom 6 --report` process. The low max zoom keeps the passes short.
- The metric is the `[convert] staged N selected row group(s) to local disk
  in X.Xs` line, which times pass 0 alone (0.1 s resolution). Requests and
  bytes come from `--report`'s `remote_fetch`. The table counts every
  request, so add 2 for the footer to the staging requests.
- Medians of 3 runs for the real hosts and the TTFB-only host's
  concurrency 8 row. The throttled host has 1 to 2 runs per cell, and its
  repeated cells matched to 0.1 s. The localhost emulator is deterministic.
- A `×N` suffix is the live concurrency when the memory budget (256 MiB ÷
  largest span) caps the ceiling.
- "off" is window 0, one request per row group (the default).

## Results: median staging seconds (total requests)

### source.coop

| concurrency | off | 16 MiB | 64 MiB | 128 MiB |
|---|---|---|---|---|
| 1 | 2.2 (20) | 1.7 (11) | 1.3 (4) | 1.4 (3) |
| 2 | 1.1 (20) | 0.9 (11) | 0.7 (4) | 1.3 (3) |
| 4 | 0.6 (20) | 0.5 (11) | 0.7 (4) | 1.3 (3) ×2 |
| **8 (default)** | **0.3 (20)** | 0.3 (11) | 0.7 (4) ×4 | 1.3 (3) ×2 |
| 16 | 0.4 (20) | 0.5 (11) | 0.7 (4) ×4 | 1.3 (3) ×2 |

### S3

| concurrency | off | 16 MiB | 64 MiB | 128 MiB |
|---|---|---|---|---|
| 1 | 1.4 (20) | 1.3 (11) | 1.3 (4) | 1.3 (3) |
| 2 | 0.7 (20) | 0.7 (11) | 0.7 (4) | 1.3 (3) |
| 4 | 0.4 (20) | 0.4 (11) | 0.7 (4) | 1.4 (3) ×2 |
| **8 (default)** | **0.2 (20)** | 0.3 (11) | 0.7 (4) ×4 | 1.3 (3) ×2 |
| 16 | 0.3 (20) | 0.3 (11) | 0.8 (4) ×4 | 1.4 (3) ×2 |

### Emulated: 1 s TTFB, 4 MB/s per connection

| concurrency | off | 16 MiB | 64 MiB | 128 MiB |
|---|---|---|---|---|
| 1 | 49.5 (20) | 40.1 (11) | 32.9 (4) | failed (218) |
| 2 | 24.9 (20) | 21.8 (11) | 16.7 (4) | failed (218) |
| 4 | 13.6 (20) | 12.7 (11) | 16.7 (4) | failed (218) ×2 |
| **8 (default)** | **7.9 (20)** | 7.9 (11) | 16.7 (4) ×4 | failed (218) ×2 |
| 16 | 5.0 (20) | 4.9 (11) | 16.7 (4) ×4 | failed (218) ×2 |

"failed" is finding 3: the span hit the 30 s request timeout and the run
took 292 s on the fallback path.

### Emulated: 1 s TTFB, no throughput cap

| concurrency | off | 16 MiB | 64 MiB |
|---|---|---|---|
| 1 | 18.1 (20) | 9.1 (11) | 2.2 (4) |
| **8 (default)** | **3.0 (20)** | 2.1 (11) | **1.1 (4) ×4** |

Staging time here is the number of TTFB waves: 18 spans at 8 in flight is
3 waves, and 2 merged spans fit in one. The concurrency 1 row is a single
run.

### HTTP/1.1 vs HTTP/2 (source.coop)

| concurrency | window | HTTP/1.1 | HTTP/2 |
|---|---|---|---|
| 1 | off | 1.8 | 2.1 |
| 4 | 64 MiB | 0.8 | 0.8 |
| 8 | off | 0.4 | 0.6 |

### Whole-object baselines

| host | tool | seconds |
|---|---|---|
| source.coop | `curl`, one stream | 1.37 |
| S3 | `aws s3 cp --no-sign-request` | 0.96 |

## Caveats

- **Measured inside the bucket's region.** The client sits in us-west-2 next
  to the bucket, so real-host TTFB is tens of milliseconds and bandwidth is
  plentiful. The issue reported source.coop TTFB of 0.7–23 s per range from
  a client outside AWS. The emulated hosts stand in for that regime, but a
  real far-away client is still unmeasured, and it decides whether
  `TYLERTOO_STAGING_COALESCE_BYTES` should get a non-zero default.
- **One object.** Its row groups are about 6.8 MB. Larger row groups shift
  every window column left (fewer merges fit), and the memory budget caps
  concurrency sooner.
- **Staging time only.** Wall time includes the passes, which this sweep
  keeps short on purpose; it does not measure end-to-end conversion.
- The partial emulated grids were cut short once their pattern was clear;
  the cells they omit follow from the TTFB-wave arithmetic above.

## Reproduce

```bash
cargo build --release -p tylertoo        # not --all-features: dhat skews timings
cd benchmarks/staging
GRID=baseline ./run_sweep.sh
GRID=main HOSTS="coop s3" ./run_sweep.sh
GRID=http HOSTS=coop CELLS="8:0 1:0 4:64" ./run_sweep.sh
GRID=main HOSTS=emu TTFB_MS=1000 CONN_BW_MBPS=4 WINDOWS="0 16 64 128" ./run_sweep.sh
GRID=main HOSTS=emu TTFB_MS=1000 CONN_BW_MBPS=0 WINDOWS="0 16 64" CONCS="1 8" ./run_sweep.sh
python3 summarize.py                     # Markdown tables from sweep_out/sweep.csv
```
