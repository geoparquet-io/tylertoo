# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""
run_manifest.py — wall time and peak RSS for the workloads in
benchmarks/manifest.toml, base against head, interleaved (#734).

    uv run python -I benchmarks/run_manifest.py fetch --data ~/bench-data/788
    uv run python -I benchmarks/run_manifest.py run --name window1 \\
        --data ~/bench-data/788 \\
        --bin main=.788/bin/tylertoo-main-glibc --bin head=target/release/tylertoo
    uv run python -I benchmarks/run_manifest.py report --name window1 \\
        --data ~/bench-data/788

Standard library only. What one `run` does, cell by cell:

  inputs    each input resolved under <data>/inputs (or an --input-dir) and
            checked against its sha256; a `<file>.sha256` beside it caches
            the check. `fetch` downloads the ones that are missing.
  setup     a cell's untimed `setup` command (an overview to export, a plan
            to replay) runs once per cell with the FIRST binary, in the
            cell's scratch under .bench-out/, deleted when the cell ends.
  rounds    `pairs` rounds (manifest, at least 3 unless --dry-run); each
            round runs every binary once, and the order flips every round
            (A B, B A, A B...) so drift on the host cancels. One --bin is a
            baseline: the same rounds, one binary. Before them, `warmup`
            untimed runs of the first binary (manifest default 1) bring the
            inputs into the page cache; they are kept under <cell>/warmup/
            and never summarized.
  each run  waits for a quiet host (load gate below), runs the command under
            `/usr/bin/time -v` with a fresh TYLERTOO_PROFILE_JSON, records
            wall, CPU, peak RSS, the profile lines, output size and sha256,
            the CPU other processes used while it ran and the peak size of
            its scratch, then deletes its outputs (.bench-out/), whatever
            the outcome, and runs `sync`. Only the result row, the profile
            JSONL and the log (stdout + `time -v`) are kept. A run stops at
            `wall_s`, `rss_gib` or `scratch_gib` (status TIMEOUT, RSS_CAP,
            SCRATCH_CAP), and the whole run stops before any command when
            the disk has less than `min_free_gb` free.
  resume    every run writes <results>/<cell>/<binary>-r<i>.json; a rerun
            skips runs whose file exists, and cells whose summary.json
            exists, unless --force. A resume whose binaries differ from
            the saved binaries.json (hash, allocator, --bin-env, CPU count)
            is refused, so one median never mixes two builds.
  report    results.jsonl (one line per run) and report.md (median and
            spread per cell and binary) in <results>.

The load gate: the 1-minute load average must be under --load-max (3). Our
own previous run holds that average up for minutes after it exits, so within
LOAD_OWN_S of a run of ours the gate instead samples /proc/stat for
GATE_SAMPLE_S: fewer than --load-max busy cores while nothing of ours runs
means the host is quiet. Every wait is logged. A run that starts after the
gate gave up (load_wait_s) is recorded with gate TIMED_OUT and reported as
status GATE_TIMED_OUT, outside the medians. On a host where the other
work is pinned away from a CPU set (`taskset -c 0-31 ... --gate-cpus 0-31`),
the gate is instead busy cores on that set only, and each run records the
CPU others used on that set (`foreign_cores`) and on the rest (`context_cores`).

A cell with `memory_max` runs in a transient systemd scope
(`systemd-run --user --scope -p MemoryMax=...`, or `sudo -n systemd-run` when
the user manager is unavailable); an OOM kill there is the result, not an
error. A cell with `serve` reads its inputs over HTTP from
benchmarks/overview/logging_server.py with the given TTFB, and records the
bytes and requests the server saw.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
import tomllib
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
MANIFEST = HERE / "manifest.toml"
LOGGING_SERVER = HERE / "overview" / "logging_server.py"

LOAD_OWN_S = 240.0  # how long our own run keeps the 1-min load average up
GATE_SAMPLE_S = 3.0
GATE_POLL_S = 15.0
MIN_PAIRS = 3
CLK_TCK = os.sysconf("SC_CLK_TCK")


# --- manifest ---------------------------------------------------------------


class ManifestError(ValueError):
    """The manifest is malformed: a missing field or a dangling reference."""


CELL_KEYS = {
    "label", "tickets", "input", "inputs", "command", "args", "zoom", "flags",
    "output", "setup", "pairs", "allocator", "wall_s", "rss_gib", "memory_max",
    "serve", "env", "heavy", "notes", "scratch_gib", "watch", "warmup",
}
INPUT_KEYS = {"file", "path", "url", "sha256", "bytes", "rows", "geometry", "notes"}


def load_manifest(path: Path = MANIFEST) -> dict:
    """Parse and validate the manifest; every cell gets its defaults filled."""
    with path.open("rb") as f:
        m = tomllib.load(f)
    defaults = m.get("defaults", {})
    inputs = m.get("inputs", {})
    cells = m.get("cells", {})
    for name, inp in inputs.items():
        extra = set(inp) - INPUT_KEYS
        if extra:
            raise ManifestError(f"input {name}: unknown keys {sorted(extra)}")
        if ("file" in inp) == ("path" in inp):
            raise ManifestError(f"input {name}: exactly one of file/path")
        if not re.fullmatch(r"[0-9a-f]{64}", inp.get("sha256", "")):
            raise ManifestError(f"input {name}: sha256 missing or malformed")
    for name, cell in cells.items():
        extra = set(cell) - CELL_KEYS
        if extra:
            raise ManifestError(f"cell {name}: unknown keys {sorted(extra)}")
        for key in ("command", "args", "output"):
            if key not in cell:
                raise ManifestError(f"cell {name}: missing {key}")
        names = cell_input_names(cell)
        if not names:
            raise ManifestError(f"cell {name}: no input")
        for i in names:
            if i not in inputs:
                raise ManifestError(f"cell {name}: unknown input {i}")
        for key in ("pairs", "allocator", "wall_s", "rss_gib", "scratch_gib", "warmup"):
            cell.setdefault(key, defaults.get(key))
        cell.setdefault("flags", [])
        cell.setdefault("env", {})
        cell.setdefault("heavy", False)
        if "zoom" in cell and len(cell["zoom"]) != 2:  # noqa: PLR2004
            raise ManifestError(f"cell {name}: zoom is [min, max]")
    for name, members in m.get("sets", {}).items():
        for c in members:
            if c not in cells:
                raise ManifestError(f"set {name}: unknown cell {c}")
    return m


def cell_input_names(cell: dict) -> list[str]:
    """A cell names one `input` or a list of `inputs` (a multi-part read)."""
    if "inputs" in cell:
        return list(cell["inputs"])
    return [cell["input"]] if "input" in cell else []


def zoom_flags(spec: dict) -> list[str]:
    """`zoom = [a, b]` as the CLI's --min-zoom/--max-zoom."""
    if "zoom" not in spec:
        return []
    lo, hi = spec["zoom"]
    return ["--min-zoom", str(lo), "--max-zoom", str(hi)]


def expand(template: list[str], subs: dict[str, str]) -> list[str]:
    """Fill `{input}`, `{files_from}`, `{out}` and `{setup}` in each argument."""
    out = []
    for arg in template:
        for key, val in subs.items():
            arg = arg.replace("{" + key + "}", val)
        if re.search(r"\{[a-z_]+\}", arg):
            raise ManifestError(f"unfilled placeholder in {arg!r}")
        out.append(arg)
    return out


def build_argv(spec: dict, subs: dict[str, str]) -> list[str]:
    """The subcommand, its positional arguments, the zoom range, the flags."""
    return [spec["command"], *expand(spec["args"], subs), *zoom_flags(spec),
            *expand(spec.get("flags", []), subs)]


def round_order(labels: list[str], round_index: int) -> list[str]:
    """Every binary once per round; even rounds forward, odd rounds reversed."""
    return list(labels) if round_index % 2 == 0 else list(reversed(labels))


# --- inputs -----------------------------------------------------------------


def sha256_file(path: Path) -> str:
    """Hex sha256 of a file, read in 1 MiB chunks."""
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def candidates(inp: dict, data: Path, input_dirs: list[Path]) -> list[Path]:
    """Where an input may live: a repo path, or <data>/inputs, then --input-dir."""
    if "path" in inp:
        return [ROOT / inp["path"]]
    return [d / inp["file"] for d in [data / "inputs", *input_dirs]]


def verified(path: Path, want: str) -> bool:
    """True when path hashes to `want`; a matching `.sha256` sidecar newer
    than the file stands in for rehashing it."""
    if not path.is_file():
        return False
    side = path.with_name(path.name + ".sha256")
    if side.is_file() and side.stat().st_mtime >= path.stat().st_mtime:
        return side.read_text().split()[0] == want
    got = sha256_file(path)
    if got == want and os.access(path.parent, os.W_OK):
        side.write_text(f"{got}  {path.name}\n")
    return got == want


def resolve_input(name: str, inp: dict, data: Path, input_dirs: list[Path]) -> Path:
    """The verified local copy of an input, or ManifestError naming the fix."""
    for p in candidates(inp, data, input_dirs):
        if p.is_file():
            if verified(p, inp["sha256"]):
                return p
            raise ManifestError(f"input {name}: {p} does not match its sha256")
    raise ManifestError(f"input {name}: not found; run `fetch` (looked in "
                        f"{[str(p) for p in candidates(inp, data, input_dirs)]})")


def fetch(manifest: dict, data: Path, input_dirs: list[Path], names: list[str]) -> None:
    """Download each missing input with a URL into <data>/inputs and verify it."""
    for name in names:
        inp = manifest["inputs"][name]
        try:
            print(f"ok    {name}: {resolve_input(name, inp, data, input_dirs)}")
            continue
        except ManifestError as e:
            if "url" not in inp or "path" in inp:
                print(f"MISS  {name}: {e}")
                continue
        dst = data / "inputs" / inp["file"]
        dst.parent.mkdir(parents=True, exist_ok=True)
        tmp = dst.with_name(dst.name + ".part")
        print(f"get   {name}: {inp['url']}")
        # source.coop answers Python-urllib's default User-Agent with 403.
        req = urllib.request.Request(inp["url"], headers={"User-Agent": "tylertoo-bench/1"})
        with urllib.request.urlopen(req) as r, tmp.open("wb") as f:  # noqa: S310
            shutil.copyfileobj(r, f, 1 << 20)
        tmp.replace(dst)
        if not verified(dst, inp["sha256"]):
            raise ManifestError(f"input {name}: download does not match its sha256")
        print(f"ok    {name}: {dst}")


# --- host -------------------------------------------------------------------


def loadavg1() -> float:
    """The 1-minute load average."""
    return float(Path("/proc/loadavg").read_text().split()[0])


def parse_cpus(spec: str) -> frozenset[int]:
    """A taskset-style CPU list (`0-31`, `0,2,4-7`) as a set of CPU ids."""
    cpus: set[int] = set()
    for part in spec.split(","):
        lo, _, hi = part.partition("-")
        cpus.update(range(int(lo), int(hi or lo) + 1))
    return frozenset(cpus)


def cpu_busy_jiffies(cpus: frozenset[int] | None = None) -> tuple[int, int]:
    """(busy, total) jiffies from /proc/stat: the aggregate `cpu` line, or
    the sum of the `cpuN` lines for N in `cpus`."""
    busy = total = 0
    for line in Path("/proc/stat").read_text().splitlines():
        name, *vals = line.split()
        if not name.startswith("cpu"):
            break
        if (cpus is None) != (name == "cpu"):
            continue
        if cpus is not None and int(name[3:]) not in cpus:
            continue
        fields = [int(x) for x in vals]
        idle = fields[3] + (fields[4] if len(fields) > 4 else 0)  # noqa: PLR2004
        busy += sum(fields[:8]) - idle
        total += sum(fields[:8])
    return busy, total


def busy_cores(seconds: float, cpus: frozenset[int] | None = None) -> float:
    """Cores busy over a short window (of `cpus` only, if given), by /proc/stat."""
    b0, _ = cpu_busy_jiffies(cpus)
    t0 = time.monotonic()
    time.sleep(seconds)
    b1, _ = cpu_busy_jiffies(cpus)
    return (b1 - b0) / CLK_TCK / (time.monotonic() - t0)


def all_cpus() -> frozenset[int]:
    """Every CPU id /proc/stat lists."""
    return frozenset(int(ln.split()[0][3:]) for ln in Path("/proc/stat").read_text().splitlines()
                     if ln.startswith("cpu") and ln[3].isdigit())


def block_device(path: Path) -> str:
    """The /proc/diskstats name of the block device holding `path`."""
    dev = os.stat(path).st_dev
    want = (os.major(dev), os.minor(dev))
    for line in Path("/proc/diskstats").read_text().splitlines():
        f = line.split()
        if (int(f[0]), int(f[1])) == want:
            return f[2]
    raise ManifestError(f"no /proc/diskstats entry for {path} ({want[0]}:{want[1]})")


def sectors_written(device: str) -> int:
    """512-byte sectors written to `device` since boot (/proc/diskstats)."""
    for line in Path("/proc/diskstats").read_text().splitlines():
        f = line.split()
        if f[2] == device:
            return int(f[9])
    raise ManifestError(f"no /proc/diskstats entry {device}")


class Gate:
    """Wait for a quiet host before each run (see the module docstring)."""

    def __init__(self, load_max: float, wait_s: float, log: Path, enforce: bool = True,  # noqa: PLR0913
                 cpus: frozenset[int] | None = None, disk: str | None = None,
                 disk_write_max: float | None = None) -> None:
        """Gate below `load_max`, waiting at most `wait_s`, logging to `log`;
        with `enforce` off it only records what it saw (dry runs). With
        `cpus` (an isolated CPU set the runs are pinned to) the gate is
        busy cores on that set only, below `load_max`; the load average and
        the other CPUs are recorded as context. With `disk_write_max` the
        write rate to block device `disk` over the same sample (MB/s) must
        also be below it; with `disk` alone the rate is only recorded."""
        self.disk = disk
        self.disk_write_max = disk_write_max
        self.enforce = enforce
        self.cpus = cpus
        self.others = all_cpus() - cpus if cpus else None
        self.load_max = load_max
        self.wait_s = wait_s
        self.log = log
        self.last_own_end = 0.0

    def wait(self) -> dict:
        """Block until quiet (or the wait cap); return what the gate saw."""
        t0 = time.monotonic()
        while True:
            load = loadavg1()
            own_recent = time.monotonic() - self.last_own_end < LOAD_OWN_S
            context = {}
            w0, tw = (sectors_written(self.disk), time.monotonic()) if self.disk else (0, 0.0)
            if self.cpus:
                b0, b0o = cpu_busy_jiffies(self.cpus)[0], cpu_busy_jiffies(self.others)[0]
                time.sleep(GATE_SAMPLE_S)
                busy = (cpu_busy_jiffies(self.cpus)[0] - b0) / CLK_TCK / GATE_SAMPLE_S
                other = (cpu_busy_jiffies(self.others)[0] - b0o) / CLK_TCK / GATE_SAMPLE_S
                context = {"busy_other_cpus": round(other, 2)}
                quiet = busy < self.load_max
            else:
                busy = busy_cores(GATE_SAMPLE_S)
                quiet = busy < self.load_max and (load < self.load_max or own_recent)
            if self.disk:
                rate = (sectors_written(self.disk) - w0) * 512 / 1e6 / (time.monotonic() - tw)
                context["disk_write_mbps"] = round(rate, 1)
                if self.disk_write_max is not None:
                    quiet = quiet and rate < self.disk_write_max
            waited = time.monotonic() - t0
            if quiet or waited > self.wait_s or not self.enforce:
                verdict = "quiet" if quiet else "NOT_ENFORCED" if not self.enforce else "TIMED_OUT"
                rec = {"load1": load, "busy_cores": round(busy, 2), **context,
                       "waited_s": round(waited, 1), "own_recent": own_recent,
                       "gate": verdict}
                if waited > GATE_SAMPLE_S * 2 or not quiet:
                    self._log(rec)
                return rec
            self._log({"load1": load, "busy_cores": round(busy, 2), **context,
                       "waiting_s": round(waited)})
            time.sleep(GATE_POLL_S)

    def _log(self, rec: dict) -> None:
        line = json.dumps({"t": time.strftime("%FT%T"), **rec})
        print(f"  gate {line}", flush=True)
        with self.log.open("a") as f:
            f.write(line + "\n")


def require_disk(path: Path, min_free_gb: float) -> None:
    """Stop the whole run when the disk under `path` is below the floor."""
    free = shutil.disk_usage(path).free / (1 << 30)
    if free < min_free_gb:
        raise SystemExit(f"stopping: {free:.0f} GB free under {path}, below the "
                         f"{min_free_gb:.0f} GB floor (min_free_gb)")


def detect_allocator(binary: Path) -> str:
    """`mimalloc` when the binary links it, else `glibc` (the gnu target's
    default). The library's own option-parsing message survives a stripped
    release build; `mi_option` symbols only an unstripped one."""
    data = binary.read_bytes()
    linked = b'environment option "mimalloc_' in data or (b"mimalloc" in data and b"mi_option" in data)
    return "mimalloc" if linked else "glibc"


def parse_bin(spec: str) -> dict:
    """`label=path` or `label=allocator:path` (the allocator is checked)."""
    label, _, rest = spec.partition("=")
    if not label or not rest:
        raise ManifestError(f"--bin {spec!r}: want label=path or label=allocator:path")
    claimed, _, path = rest.partition(":") if ":" in rest else ("", "", rest)
    p = Path(path).expanduser().resolve()
    if not os.access(p, os.X_OK):
        raise ManifestError(f"--bin {label}: {p} is not executable")
    found = detect_allocator(p)
    if claimed and claimed != found:
        raise ManifestError(f"--bin {label}: claimed {claimed} but the binary links {found}")
    return {"label": label, "path": str(p), "allocator": found, "sha256": sha256_file(p)}


BUILD_KEYS = ("label", "sha256", "allocator", "env", "affinity_cpus")


def check_resume(saved: Path, bins: list[dict]) -> None:
    """Refuse to resume a run whose saved binaries (`binaries.json`) differ
    from these in hash, allocator, per-binary environment or CPU count: the
    saved results would be reported under the new build, and a partial
    resume would mix two builds in one median."""
    if not saved.exists():
        return
    old = [{k: b.get(k) for k in BUILD_KEYS} for b in json.loads(saved.read_text())]
    new = [{k: b.get(k) for k in BUILD_KEYS} for b in bins]
    if old != new:
        raise ManifestError(f"{saved.parent} holds results of other binaries or settings "
                            f"(saved {old}, given {new}); resume with the same --bin and "
                            "--bin-env, or start a new --name")


def scope_prefix(memory_max: str) -> list[str]:
    """A transient systemd scope with a hard memory cap and no swap."""
    props = ["-p", f"MemoryMax={memory_max}", "-p", "MemorySwapMax=0"]
    probe = ["systemd-run", "--user", "--scope", "--quiet", *props, "--", "/bin/true"]
    if subprocess.run(probe, capture_output=True, check=False).returncode == 0:
        return ["systemd-run", "--user", "--scope", "--quiet", *props, "--"]
    uid, gid = str(os.getuid()), str(os.getgid())
    sudo = ["sudo", "-n", "systemd-run", "--scope", "--quiet", f"--uid={uid}", f"--gid={gid}", *props, "--"]
    if subprocess.run([*sudo, "/bin/true"], capture_output=True, check=False).returncode == 0:
        return sudo
    raise ManifestError("memory_max needs systemd-run --user or sudo -n systemd-run")


class Server:
    """logging_server.py on a free localhost port, serving a directory of
    symlinks to the cell's inputs (wherever each input was found)."""

    def __init__(self, root: Path, files: list[Path], serve: dict, log: Path) -> None:
        """Link `files` into `root` and serve it with the cell's TTFB and
        bandwidth emulation."""
        if root.exists():
            shutil.rmtree(root)
        root.mkdir(parents=True)
        for f in files:
            (root / f.name).symlink_to(f)
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        env = {**os.environ, "TTFB_MS": str(serve.get("ttfb_ms", 0)),
               "CONN_BW_MBPS": str(serve.get("conn_bw_mbps", 0))}
        self.proc = subprocess.Popen(  # noqa: S603
            [sys.executable, "-I", str(LOGGING_SERVER), str(root), str(self.port)],
            env=env, stdout=log.open("a"), stderr=subprocess.STDOUT)
        for _ in range(50):
            try:
                self.get("/__stats")
                return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError("logging_server did not start")

    def url(self, rel: str) -> str:
        """The served URL of a file under the root."""
        return f"http://127.0.0.1:{self.port}/{rel}"

    def get(self, path: str) -> dict:
        """GET a control endpoint (never counted by the server)."""
        with urllib.request.urlopen(self.url(path.lstrip("/")), timeout=5) as r:  # noqa: S310
            body = r.read()
        return json.loads(body) if body.strip().startswith(b"{") else {}

    def stop(self) -> None:
        """Terminate the server."""
        self.proc.terminate()
        self.proc.wait(timeout=10)


# --- one run ----------------------------------------------------------------


def tree_rss_kib(pid: int) -> int:
    """Resident KiB of pid and every descendant."""
    total, stack = 0, [pid]
    while stack:
        p = stack.pop()
        try:
            for line in Path(f"/proc/{p}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    total += int(line.split()[1])
                    break
            for task in Path(f"/proc/{p}/task").iterdir():
                stack.extend(int(c) for c in (task / "children").read_text().split())
        except (OSError, ValueError):
            pass
    return total


def parse_gnu_time(stderr: str) -> dict:
    """Wall, CPU, peak RSS and exit status from `/usr/bin/time -v`."""
    def grab(pattern: str) -> str | None:
        m = re.search(pattern + r":\s*([\d.:]+)", stderr)
        return m.group(1) if m else None

    wall = grab(r"Elapsed \(wall clock\) time \(h:mm:ss or m:ss\)")
    secs = None
    if wall:
        secs = 0.0
        for part in wall.split(":"):
            secs = secs * 60 + float(part)
    user, sys_t = grab(r"User time \(seconds\)"), grab(r"System time \(seconds\)")
    rss = grab(r"Maximum resident set size \(kbytes\)")
    status = grab("Exit status")
    return {
        "wall_s": secs,
        "cpu_s": round(float(user) + float(sys_t), 2) if user and sys_t else None,
        "peak_rss_kib": int(rss) if rss else None,
        "exit_status": int(status) if status else None,
    }


def read_profile(path: Path) -> dict:
    """The convert and export lines of a fresh TYLERTOO_PROFILE_JSON file,
    reduced to what the report compares."""
    out: dict = {}
    if not path.is_file():
        return out
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        rec = json.loads(line)
        if rec.get("phase") == "export":
            ex = rec.get("export", {})
            out["export"] = {
                "mode": ex.get("mode"),
                "phase_walls": ex.get("phase_walls"),
                "stage_secs": ex.get("stage_secs"),
                "true_peak_mib": (ex.get("rss_sampler") or {}).get("true_peak_mib"),
                "true_anon_peak_mib": (ex.get("rss_sampler") or {}).get("true_anon_peak_mib"),
                "tiles": sum(z.get("tiles", 0) for z in ex.get("per_zoom", [])),
                "threads": ex.get("threads"),
            }
        else:
            out["convert"] = {
                "phase_walls": rec.get("phase_walls"),
                "pass1_rows": (rec.get("pass1") or {}).get("rows"),
                "pass2_rows": (rec.get("pass2") or {}).get("rows"),
                "true_peak_mib": (rec.get("rss_sampler") or {}).get("true_peak_mib"),
                "true_anon_peak_mib": (rec.get("rss_sampler") or {}).get("true_anon_peak_mib"),
                "memory_profile": rec.get("memory_profile"),
                "threads": rec.get("threads"),
            }
    return out


def dir_bytes(path: Path) -> int:
    """Bytes of every file under path (files may vanish while we walk)."""
    total = 0
    for root, _, files in os.walk(path):
        for f in files:
            try:
                total += os.lstat(os.path.join(root, f)).st_size
            except OSError:
                pass
    return total


def run_capped(argv: list[str], caps: dict, env: dict, log: Path, scratch: Path,
               cpus: frozenset[int] | None = None) -> dict:
    """Run argv (already wrapped in GNU time). A wall, RSS or scratch-disk
    cap (`caps`: wall_s, rss_gib, scratch_gib) kills the process group and
    is reported as the status, not raised. `foreign_cores` is the CPU other
    processes used during the run: on `cpus` when given (the isolated set the
    run is pinned to; the rest is `context_cores`), else on the whole host."""
    others = all_cpus() - cpus if cpus else None
    o0 = cpu_busy_jiffies(others)[0] if cpus else 0
    cap_kib = int(caps["rss_gib"] * 1024 * 1024)
    wall_s = caps["wall_s"]
    scratch_cap = int(caps["scratch_gib"] * (1 << 30))
    b0, _ = cpu_busy_jiffies(cpus)
    t0 = time.monotonic()
    with log.open("w") as logf:
        proc = subprocess.Popen(argv, stdout=logf, stderr=subprocess.PIPE, text=True,  # noqa: S603
                                env=env, start_new_session=True)
        outcome = {"cap": None, "peak_poll_kib": 0, "scratch_peak": 0, "watch_peak": 0, "cgroup": None}
        watch_dir = scratch / caps["watch"] if caps.get("watch") else None

        def watch() -> None:
            tick = 0
            while proc.poll() is None:
                rss = tree_rss_kib(proc.pid)
                outcome["peak_poll_kib"] = max(outcome["peak_poll_kib"], rss)
                if tick % 10 == 0:  # the scratch walk every ~2 s
                    outcome["scratch_peak"] = max(outcome["scratch_peak"], dir_bytes(scratch))
                if watch_dir:  # a cell's own watched subdirectory, every tick
                    outcome["watch_peak"] = max(outcome["watch_peak"], dir_bytes(watch_dir))
                if caps.get("memory_max"):  # the scope's own counters, gone once it exits
                    outcome["cgroup"] = scope_memory(proc.pid) or outcome["cgroup"]
                tick += 1
                elapsed = time.monotonic() - t0
                reason = ("RSS_CAP" if rss > cap_kib else "TIMEOUT" if elapsed > wall_s
                          else "SCRATCH_CAP" if outcome["scratch_peak"] > scratch_cap else None)
                if reason:
                    outcome["cap"] = reason
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    return
                time.sleep(0.2)

        watcher = threading.Thread(target=watch, daemon=True)
        watcher.start()
        _, stderr = proc.communicate()
        watcher.join()
        logf.write("\n--- stderr ---\n" + stderr)
    wall_poll = time.monotonic() - t0
    b1, _ = cpu_busy_jiffies(cpus)
    context = (cpu_busy_jiffies(others)[0] - o0) / CLK_TCK / max(wall_poll, 1e-6) if cpus else None
    gt = parse_gnu_time(stderr)
    if outcome["cap"]:
        status = outcome["cap"]
    elif proc.returncode == 0:
        status = "ok"
    elif gt["exit_status"] is None or proc.returncode in (-9, 137) or "Killed" in stderr:
        status = "KILLED"
    else:
        status = "FAILED"
    wall = gt["wall_s"] if status == "ok" and gt["wall_s"] else wall_poll
    busy_s = (b1 - b0) / CLK_TCK
    foreign = max(0.0, busy_s - (gt["cpu_s"] or 0.0)) / max(wall_poll, 1e-6)
    return {
        "status": status,
        "returncode": proc.returncode,
        "wall_s": round(wall, 3),
        "cpu_s": gt["cpu_s"],
        "peak_rss_kib": gt["peak_rss_kib"] if gt["peak_rss_kib"] else outcome["peak_poll_kib"],
        "peak_poll_kib": outcome["peak_poll_kib"],
        "foreign_cores": round(foreign, 2),
        "context_cores": round(context, 2) if context is not None else None,
        "scratch_peak_mib": round(outcome["scratch_peak"] / (1 << 20)),
        "watch_peak_mib": round(outcome["watch_peak"] / (1 << 20), 1) if watch_dir else None,
        "cgroup_memory": outcome["cgroup"],
        "stderr_tail": stderr[-2500:] if status != "ok" else None,
    }


def scope_memory(pid: int) -> dict | None:
    """memory.events and memory.peak of the transient scope `pid` runs in
    (None until systemd-run has moved it into a `run-*.scope`)."""
    try:
        path = Path("/proc", str(pid), "cgroup").read_text().strip().split("::", 1)[1]
        if not path.endswith(".scope"):
            return None
        cg = Path("/sys/fs/cgroup") / path.lstrip("/")
        events = dict(ln.split() for ln in (cg / "memory.events").read_text().splitlines())
        peak = (cg / "memory.peak").read_text().strip() if (cg / "memory.peak").exists() else None
    except (OSError, IndexError, ValueError):
        return None
    return {"scope": path, "events": {k: int(v) for k, v in events.items()},
            "peak_mib": round(int(peak) / (1 << 20), 1) if peak else None}


def output_digest(out_dir: Path, output: str) -> dict:
    """Size and sha256 of the cell's output (a file, or every file of a dir)."""
    target = out_dir / output
    if target.is_file():
        return {"bytes": target.stat().st_size, "sha256": sha256_file(target)}
    if target.is_dir():
        h, size = hashlib.sha256(), 0
        for f in sorted(p for p in target.rglob("*") if p.is_file()):
            h.update(f.relative_to(target).as_posix().encode())
            h.update(sha256_file(f).encode())
            size += f.stat().st_size
        return {"bytes": size, "sha256": h.hexdigest()}
    return {"bytes": None, "sha256": None}


# --- cells ------------------------------------------------------------------


class Runner:
    """One `run` invocation: a manifest, binaries, a results directory."""

    def __init__(self, args: argparse.Namespace, manifest: dict) -> None:
        """Resolve directories and binaries; nothing runs yet."""
        self.args = args
        self.m = manifest
        self.data = Path(args.data).expanduser().resolve()
        self.input_dirs = [Path(d).expanduser().resolve() for d in args.input_dir]
        self.results = results_dir(args)
        self.results.mkdir(parents=True, exist_ok=True)
        self.bench_out = Path(args.bench_out).expanduser().resolve()
        # Scratch never outlives a run: clear what an interrupted one left.
        shutil.rmtree(self.bench_out / args.name, ignore_errors=True)
        self.min_free_gb = manifest.get("defaults", {}).get("min_free_gb", 30)
        self.bins = [parse_bin(b) for b in args.bin]
        labels = [b["label"] for b in self.bins]
        if len(set(labels)) != len(labels):
            raise ManifestError("--bin labels must be distinct")
        for b in self.bins:
            b["env"] = {}
        for spec in args.bin_env:
            label, _, kv = spec.partition(":")
            key, eq, val = kv.partition("=")
            if label not in labels or not key or not eq:
                raise ManifestError(f"--bin-env {spec!r}: want label:KEY=VALUE for a --bin label")
            next(b for b in self.bins if b["label"] == label)["env"][key] = val
        defaults = manifest.get("defaults", {})
        self.cpus = parse_cpus(args.gate_cpus) if args.gate_cpus else None
        affinity = frozenset(os.sched_getaffinity(0))
        if self.cpus and not affinity <= self.cpus:
            raise ManifestError(f"--gate-cpus {args.gate_cpus}: the runner may run on "
                                f"{sorted(affinity)}; start it under `taskset -c {args.gate_cpus}`")
        for b in self.bins:
            b["affinity_cpus"] = len(affinity)
        self.bench_out.mkdir(parents=True, exist_ok=True)
        self.disk = block_device(self.bench_out)
        self.gate = Gate(args.load_max or defaults.get("load_max", 3.0),
                         defaults.get("load_wait_s", 3600), self.results / "gate.log",
                         enforce=not args.no_gate, cpus=self.cpus, disk=self.disk,
                         disk_write_max=args.disk_write_max)
        check_resume(self.results / "binaries.json", self.bins)
        (self.results / "binaries.json").write_text(json.dumps(self.bins, indent=2))

    def cells(self) -> list[str]:
        """The cells this run covers: --cells, else --set, else every light cell."""
        cells = self.m["cells"]
        if self.args.cells:
            names = self.args.cells.split(",")
        elif self.args.set:
            names = self.m["sets"][self.args.set]
        else:
            names = [c for c, v in cells.items() if not v["heavy"]]
        for n in names:
            if n not in cells:
                raise ManifestError(f"unknown cell {n}")
        return names

    def pairs(self, cell: dict) -> int:
        """Rounds for a cell: 1 on a dry run, else the manifest's (at least 3,
    except for a heavy cell, which may name fewer)."""
        if self.args.dry_run:
            return 1
        n = self.args.pairs or cell["pairs"] or MIN_PAIRS
        if n < MIN_PAIRS and not cell["heavy"] and not self.args.few_pairs:
            raise ManifestError(f"pairs={n}: a timing run needs at least {MIN_PAIRS}")
        return n

    def run_all(self) -> None:
        """Every selected cell in manifest order, then the report."""
        for name in self.cells():
            self.run_cell(name)
        write_report(self.results)

    def run_cell(self, name: str) -> None:
        """Setup, then the interleaved rounds; skip what is already recorded."""
        cell = self.m["cells"][name]
        cdir = self.results / name
        if self.args.force and cdir.exists():
            shutil.rmtree(cdir)
        if (cdir / "summary.json").exists():
            print(f"skip  {name} (summary.json exists; --force reruns it)")
            return
        cdir.mkdir(parents=True, exist_ok=True)
        local = [resolve_input(i, self.m["inputs"][i], self.data, self.input_dirs)
                 for i in cell_input_names(cell)]
        scratch = self.bench_out / self.args.name / name
        server = None
        if "serve" in cell:
            root = self.bench_out / self.args.name / name / "served"
            server = Server(root, local, cell["serve"], cdir / "server.log")
        try:
            subs = self.substitutions(name, local, server)
            if "setup" in cell:
                subs["setup"] = str(self.setup(name, cell, subs, scratch / "setup"))
            prefix = scope_prefix(cell["memory_max"]) if "memory_max" in cell else []
            labels = [b["label"] for b in self.bins]
            todo = [(r, lb) for r in range(self.pairs(cell)) for lb in round_order(labels, r)
                    if not (cdir / f"{lb}-r{r}.json").exists()]
            warmups = self.args.warmup if self.args.warmup is not None else (cell["warmup"] or 0)
            if todo:
                # Untimed runs of the first binary bring the inputs into the page
                # cache, so round 0's first binary does not pay a cold read alone.
                for w in range(warmups):
                    self.run_one(name, cell, w, labels[0], subs, prefix, server, warmup=True)
            for r in range(self.pairs(cell)):
                for label in round_order(labels, r):
                    self.run_one(name, cell, r, label, subs, prefix, server)
        finally:
            if server:
                server.stop()
            shutil.rmtree(scratch, ignore_errors=True)
        summary = summarize(load_runs(cdir), self.bins)
        (cdir / "summary.json").write_text(json.dumps(summary, indent=2))
        write_report(self.results)

    def substitutions(self, name: str, local: list[Path], server: Server | None) -> dict:
        """`{input}` and `{files_from}`: local paths, or served URLs."""
        if server:
            refs = [server.url(p.name) for p in local]
        else:
            refs = [str(p) for p in local]
        lists = self.results / name / "files_from.txt"
        lists.write_text("".join(r + "\n" for r in refs))
        return {"input": refs[0], "files_from": str(lists)}

    def setup(self, name: str, cell: dict, subs: dict, sdir: Path) -> Path:
        """Run the cell's untimed setup with the first binary, in the cell's
        scratch (deleted with the cell, so it is rebuilt on a resume)."""
        base = self.bins[0]
        require_disk(self.bench_out, self.min_free_gb)
        shutil.rmtree(sdir, ignore_errors=True)
        (sdir / "tmp").mkdir(parents=True)
        argv = [base["path"], *build_argv(cell["setup"], {**subs, "setup": str(sdir), "out": str(sdir)})]
        print(f"setup {name}: {' '.join(argv)}", flush=True)
        t0 = time.monotonic()
        env = {**os.environ, **{k: str(v) for k, v in cell["env"].items()}, **base["env"],
               "TMPDIR": str(sdir / "tmp")}
        res = subprocess.run(argv, capture_output=True, text=True, env=env, check=False)  # noqa: S603
        (self.results / name / "setup.log").write_text(
            " ".join(argv) + "\n" + res.stdout + "\n--- stderr ---\n" + res.stderr)
        if res.returncode != 0:
            raise RuntimeError(f"setup {name} failed ({res.returncode}): {res.stderr[-1500:]}")
        shutil.rmtree(sdir / "tmp", ignore_errors=True)
        print(f"setup {name}: done in {time.monotonic() - t0:.1f}s, "
              f"{dir_bytes(sdir) >> 20} MiB", flush=True)
        return sdir

    def run_one(self, name: str, cell: dict, r: int, label: str, subs: dict,  # noqa: PLR0913
                prefix: list[str], server: Server | None, warmup: bool = False) -> None:
        """One timed run of one binary; writes <cell>/<label>-r<r>.json, or
        <cell>/warmup/<label>-w<r>.json for an untimed warm-up (never summarized)."""
        cdir = self.results / name / "warmup" if warmup else self.results / name
        cdir.mkdir(exist_ok=True)
        tag = f"{label}-w{r}" if warmup else f"{label}-r{r}"
        rec_path = cdir / f"{tag}.json"
        if rec_path.exists():
            return
        b = next(x for x in self.bins if x["label"] == label)
        out = self.bench_out / self.args.name / name / tag
        if out.exists():
            shutil.rmtree(out)
        (out / "tmp").mkdir(parents=True)
        if cell.get("watch"):  # e.g. a --spill-dir, which must exist beforehand
            (out / cell["watch"]).mkdir(parents=True, exist_ok=True)
        prof = cdir / f"{tag}.profile.jsonl"
        prof.unlink(missing_ok=True)
        argv = [b["path"], *build_argv(cell, {**subs, "out": str(out)})]
        env = {**os.environ, **{k: str(v) for k, v in cell["env"].items()}, **b["env"],
               "TYLERTOO_PROFILE_JSON": str(prof), "TMPDIR": str(out / "tmp")}
        require_disk(self.bench_out, self.min_free_gb)
        gate = self.gate.wait()
        if server:
            server.get("/__reset")
        print(f"{'warm' if warmup else 'run '}  {name} {tag}: {' '.join(argv[1:])}", flush=True)
        w0 = sectors_written(self.disk)
        try:
            res = run_capped([*prefix, "/usr/bin/time", "-v", *argv], cell, env,
                             cdir / f"{tag}.log", out, self.cpus)
            # Everything written to the device during the run, ours and others'
            # (our part still in the page cache at exit is not counted).
            res["device_write_mib"] = round((sectors_written(self.disk) - w0) * 512 / (1 << 20), 1)
            output = output_digest(out, cell["output"])
        finally:
            # Only the result row, the profile JSONL and the log survive a run.
            shutil.rmtree(out, ignore_errors=True)
        self.gate.last_own_end = time.monotonic()
        if "memory_max" in cell and res["status"] == "KILLED":
            res["status"] = "OOM"
        rec = {
            "run": self.args.name, "cell": name, "binary": label, "round": r, "warmup": warmup,
            "allocator": b["allocator"], "allocator_expected": cell["allocator"],
            "binary_sha256": b["sha256"], "binary_env": b["env"], "argv": argv, "dry_run": self.args.dry_run,
            "started": time.strftime("%FT%T"), "gate": gate, **res,
            "load1_after": loadavg1(),
            "output": output,
            "profile": read_profile(prof),
            "served": server.get("/__stats") if server else None,
            "memory_max": cell.get("memory_max"),
        }
        subprocess.run(["sync"], check=False)
        rec_path.write_text(json.dumps(rec, indent=2))
        if not warmup:
            with (self.results / "results.jsonl").open("a") as f:
                f.write(json.dumps(rec) + "\n")
        rss = (rec["peak_rss_kib"] or 0) / 1024
        print(f"      {res['status']} wall {res['wall_s']:.2f}s cpu {res['cpu_s']}s "
              f"rss {rss:.0f} MiB foreign {res['foreign_cores']} cores", flush=True)


def results_dir(args: argparse.Namespace) -> Path:
    """<data>/results/<name> unless --results says otherwise."""
    if args.results:
        return Path(args.results).expanduser().resolve()
    return Path(args.data).expanduser().resolve() / "results" / args.name


# --- summary and report -----------------------------------------------------


def load_runs(cdir: Path) -> list[dict]:
    """Every run record of a cell."""
    return [json.loads(p.read_text()) for p in sorted(cdir.glob("*-r*.json"))]


def spread(values: list[float]) -> dict:
    """Median, min, max and (max-min)/median of a sample."""
    if not values:
        return {"n": 0, "median": None, "min": None, "max": None, "spread": None}
    med = statistics.median(values)
    return {"n": len(values), "median": round(med, 3), "min": round(min(values), 3),
            "max": round(max(values), 3),
            "spread": round((max(values) - min(values)) / med, 4) if med else None}


def true_peak(run: dict) -> float | None:
    """The sampler's true peak RSS (MiB): the largest of the run's phases."""
    peaks = [(run.get("profile", {}).get(ph) or {}).get("true_peak_mib") for ph in ("convert", "export")]
    peaks = [p for p in peaks if p is not None]
    return max(peaks) if peaks else None


def phase_total(run: dict, phase: str) -> float | None:
    """`phase_walls.total` of the convert or export line, if the run wrote one."""
    walls = (run.get("profile", {}).get(phase) or {}).get("phase_walls") or {}
    return walls.get("total")


def phase_wall(run: dict, name: str) -> float | None:
    """One convert `phase_walls` entry (pass1, assign, pass2, writer_finish)."""
    return ((run.get("profile", {}).get("convert") or {}).get("phase_walls") or {}).get(name)


def run_status(run: dict) -> str:
    """The run's status, or `GATE_TIMED_OUT` for a run that finished but
    started after the load gate gave up waiting for a quiet host: its
    timing was taken under load, so it stays out of the medians."""
    if run["status"] == "ok" and (run.get("gate") or {}).get("gate") == "TIMED_OUT":
        return "GATE_TIMED_OUT"
    return run["status"]


def summarize(runs: list[dict], bins: list[dict]) -> dict:
    """Per binary: wall, CPU, peak RSS and phase totals over its ok runs
    (a run whose gate timed out is not ok, see `run_status`)."""
    out = {}
    for b in bins:
        mine = [r for r in runs if r["binary"] == b["label"]]
        ok = [r for r in mine if run_status(r) == "ok"]
        out[b["label"]] = {
            "allocator": b["allocator"],
            "statuses": sorted({run_status(r) for r in mine}),
            "wall_s": spread([r["wall_s"] for r in ok]),
            "cpu_s": spread([r["cpu_s"] for r in ok if r["cpu_s"] is not None]),
            "peak_rss_mib": spread([r["peak_rss_kib"] / 1024 for r in ok if r["peak_rss_kib"]]),
            "convert_s": spread([v for r in ok if (v := phase_total(r, "convert")) is not None]),
            "export_s": spread([v for r in ok if (v := phase_total(r, "export")) is not None]),
            "true_peak_mib": spread([v for r in ok if (v := true_peak(r)) is not None]),
            "pass2_s": spread([v for r in ok if (v := phase_wall(r, "pass2")) is not None]),
            "writer_finish_s": spread([v for r in ok if (v := phase_wall(r, "writer_finish")) is not None]),
            "gate_disk_mbps": spread([r["gate"]["disk_write_mbps"] for r in mine
                                      if r.get("gate", {}).get("disk_write_mbps") is not None]),
            "device_write_mib": spread([r["device_write_mib"] for r in ok if r.get("device_write_mib") is not None]),
            "watch_peak_mib": spread([r["watch_peak_mib"] for r in ok if r.get("watch_peak_mib") is not None]),
            "foreign_cores_max": max((r["foreign_cores"] for r in mine), default=None),
            "context_cores": spread([r["context_cores"] for r in mine if r.get("context_cores") is not None]),
            "outputs": sorted({r["output"]["sha256"] or "-" for r in ok}),
            "output_bytes": sorted({r["output"]["bytes"] for r in ok if r["output"]["bytes"]}),
        }
    return out


def fmt(s: dict, unit: str = "") -> str:
    """`median unit (±spread%)` or a dash."""
    if not s or not s["n"]:
        return "—"
    pct = f" ±{s['spread'] * 50:.1f}%" if s["spread"] is not None and s["n"] > 1 else ""
    return f"{s['median']:.2f}{unit}{pct}"


def write_report(results: Path) -> None:
    """report.md: one row per cell and binary, wall and RSS ratios against the
    first binary and against the binary before it. With three binaries in
    order (origin/main glibc, head glibc, head mimalloc) `vs prev` splits a
    change into the code's gain and the allocator's."""
    bins_path = results / "binaries.json"
    if not bins_path.exists():
        return
    bins = json.loads(bins_path.read_text())
    lines = [f"# Benchmark run `{results.name}`", "",
             "Medians over ok runs; ± is half the min–max range over the median.",
             "`foreign` is the most cores other processes used during any run of the row (on the "
             "--gate-cpus set when given); `context` is the median busy cores on the other CPUs.", "",
             "Binaries: " + ", ".join(f"`{b['label']}` ({b['allocator']}, {b['sha256'][:12]})" for b in bins),
             "", "| cell | binary | runs | status | wall | vs first | vs prev | convert | export | CPU s "
             "| peak RSS MiB | true peak MiB | RSS vs first | RSS vs prev | foreign | context | output |",
             "|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"]
    for cdir in sorted(p for p in results.iterdir() if p.is_dir()):
        runs = load_runs(cdir)
        if not runs:
            continue
        summ = summarize(runs, bins)
        shas = {s for v in summ.values() for s in v["outputs"]}

        def ratio(i: int, j: int, key: str) -> str:
            num = summ[bins[i]["label"]][key]["median"]
            den = summ[bins[j]["label"]][key]["median"]
            return f"{num / den:.3f}" if i != j and num and den else ""

        for i, b in enumerate(bins):
            s = summ[b["label"]]
            if not s["statuses"]:
                continue
            prev = max(i - 1, 0)
            same = "same" if len(shas) == 1 else "DIFFERS" if len(shas) > 1 else ""
            lines.append(
                f"| {cdir.name} | {b['label']} | {s['wall_s']['n']} | {','.join(s['statuses'])} "
                f"| {fmt(s['wall_s'], ' s')} | {ratio(i, 0, 'wall_s')} | {ratio(i, prev, 'wall_s')} "
                f"| {fmt(s['convert_s'])} | {fmt(s['export_s'])} "
                f"| {fmt(s['cpu_s'])} | {fmt(s['peak_rss_mib'])} | {fmt(s['true_peak_mib'])} "
                f"| {ratio(i, 0, 'peak_rss_mib')} | {ratio(i, prev, 'peak_rss_mib')} "
                f"| {s['foreign_cores_max']} | {fmt(s['context_cores'])} "
                f"| {same} {s['output_bytes'][0] if s['output_bytes'] else ''} |")
    lines += ["", "Convert phases, spill and disk: `spill peak` is the cell's watched directory "
              "(e.g. --spill-dir) sampled every 0.2 s; `gate disk` is the device write rate (MB/s, "
              "everyone's) in the 3 s before each run; `device written` is everything written to "
              "the device during a run.", "",
              "| cell | binary | pass 2 s | writer_finish s | spill peak MiB | gate disk MB/s "
              "| device written MiB |", "|---|---|---:|---:|---:|---:|---:|"]
    for cdir in sorted(p for p in results.iterdir() if p.is_dir()):
        runs = load_runs(cdir)
        if not runs:
            continue
        for b, s in zip(bins, summarize(runs, bins).values(), strict=True):
            if s["statuses"] and (s["pass2_s"]["n"] or s["watch_peak_mib"]["n"]):
                lines.append(f"| {cdir.name} | {b['label']} | {fmt(s['pass2_s'])} "
                             f"| {fmt(s['writer_finish_s'])} | {fmt(s['watch_peak_mib'])} "
                             f"| {fmt(s['gate_disk_mbps'])} | {fmt(s['device_write_mib'])} |")
    (results / "report.md").write_text("\n".join(lines) + "\n")


# --- CLI --------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    """Subcommands: list, fetch, run, report."""
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--manifest", default=str(MANIFEST))
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("list", help="cells, inputs and sets")
    for name in ("fetch", "run", "report"):
        p = sub.add_parser(name)
        p.add_argument("--data", default="~/bench-data/manifest", help="inputs/, setup/, results/")
        p.add_argument("--input-dir", action="append", default=[], help="more places to find inputs")
        if name != "fetch":
            p.add_argument("--name", required=True, help="the run's name; results/<name> resumes it")
            p.add_argument("--results", help="results directory (default <data>/results/<name>)")
    run = sub.choices["run"]
    run.add_argument("--bin", action="append", required=True, help="label=path or label=allocator:path; first is the base")
    run.add_argument("--cells", help="comma-separated cells (default: every cell not marked heavy)")
    run.add_argument("--set", help="a [sets] entry of the manifest")
    run.add_argument("--pairs", type=int, help=f"rounds per cell (at least {MIN_PAIRS})")
    run.add_argument("--warmup", type=int, help="untimed runs of the first binary before a cell's rounds "
                     "(manifest default)")
    run.add_argument("--few-pairs", action="store_true",
                     help=f"allow --pairs below {MIN_PAIRS} on a light cell (a short, asked-for check)")
    run.add_argument("--dry-run", action="store_true", help="one round per cell; not a baseline")
    run.add_argument("--force", action="store_true", help="rerun cells that already have results")
    run.add_argument("--load-max", type=float, help="load gate threshold (manifest default)")
    run.add_argument("--gate-cpus", help="an isolated CPU set the run is pinned to (taskset -c): "
                     "gate on busy cores there (< --load-max) instead of the load average, "
                     "and count foreign CPU there only")
    run.add_argument("--bin-env", action="append", default=[], metavar="LABEL:KEY=VALUE",
                     help="an environment variable for one --bin only (e.g. a codec selector)")
    run.add_argument("--disk-write-max", type=float, metavar="MBPS",
                     help="also gate on the write rate to the --bench-out device (MB/s over the "
                     "gate sample); the rate is recorded either way")
    run.add_argument("--no-gate", action="store_true",
                     help="record the load but never wait (dry runs on a busy host)")
    run.add_argument("--bench-out", default=str(ROOT / ".bench-out"), help="scratch for outputs, emptied per run")
    args = ap.parse_args(argv)
    manifest = load_manifest(Path(args.manifest))
    if args.cmd == "list":
        for name, c in manifest["cells"].items():
            heavy = " [heavy]" if c["heavy"] else ""
            print(f"{name:22} {c['command']:15} {','.join(cell_input_names(c)):28} "
                  f"zoom={c.get('zoom', '-')} pairs={c['pairs']}{heavy}  {c.get('label', '')}")
        for name, members in manifest.get("sets", {}).items():
            print(f"set {name}: {', '.join(members)}")
        return 0
    if args.cmd == "fetch":
        fetch(manifest, Path(args.data).expanduser(), [Path(d).expanduser() for d in args.input_dir],
              list(manifest["inputs"]))
        return 0
    if args.cmd == "report":
        write_report(results_dir(args))
        print((results_dir(args) / "report.md").read_text())
        return 0
    Runner(args, manifest).run_all()
    print((results_dir(args) / "report.md").read_text())
    return 0


if __name__ == "__main__":
    sys.exit(main())
