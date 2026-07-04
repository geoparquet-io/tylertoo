#!/usr/bin/env python3
"""Minimal purpose-built parallel range-request reader — issue #201.

Demonstrates the latency floor of the overview GeoParquet read
protocol (OVERVIEWS_SPEC.md §5.1) over object storage:

  1. ONE ranged GET for the parquet footer (suffix request; the
     Content-Range header supplies the object size, so no HEAD).
  2. Parse the footer locally (pyarrow parses the Thrift bytes we
     fetched — pyarrow never touches the network).
  3. Prune row groups with the footer's min/max statistics on
     `level` and the bbox covering columns — exactly the documented
     read protocol.
  4. Fetch every surviving row-group byte range CONCURRENTLY
     (requests.Session pool, 16 threads; adjacent ranges coalesced).
  5. Decode the fetched row groups from memory (pyarrow over a
     sparse in-memory file) and apply the per-feature predicate to
     a feature count.

Two modes:
  cold          -- fresh TLS session, footer fetched fresh.
  footer-cached -- the same reader/session re-runs the viewport:
                   footer bytes + parsed metadata held from the
                   prior fetch (the map-session case; the footer is
                   immutable). Only data ranges are re-fetched.

All fetching is our own HTTP range requests; requests, bytes, and
wall time are counted client-side.

Smoke test (single cell):
  uv run --with pyarrow --with requests python3 parallel_reader.py \
      <presigned-url> <level> <xmin> <ymin> <xmax> <ymax>
"""
import concurrent.futures as cf
import io
import re
import sys
import time

import pyarrow.compute as pc
import pyarrow.parquet as pq

TAIL_GUESS = 512 * 1024   # first suffix fetch; covers post-H1 footers
COALESCE_GAP = 64 * 1024  # merge ranges closer than this
POOL = 16                 # concurrent connections / threads

BBOX_COLS = ("bbox.xmin", "bbox.xmax", "bbox.ymin", "bbox.ymax")


def make_session():
    import requests

    s = requests.Session()
    ad = requests.adapters.HTTPAdapter(
        pool_connections=POOL, pool_maxsize=POOL
    )
    s.mount("https://", ad)
    s.mount("http://", ad)
    return s


class SparseFile(io.RawIOBase):
    """Read-only file backed by explicitly fetched byte ranges.

    Any read outside a fetched range raises — the proof that the
    reader pre-fetched everything it needed and pyarrow issued no
    hidden I/O of its own.
    """

    def __init__(self, size):
        self._size = size
        self._pos = 0
        self._ranges = []  # sorted list of (start, bytes)

    def add(self, start, data):
        self._ranges.append((start, data))
        self._ranges.sort(key=lambda r: r[0])

    def readable(self):
        return True

    def seekable(self):
        return True

    def seek(self, off, whence=0):
        if whence == 0:
            self._pos = off
        elif whence == 1:
            self._pos += off
        else:
            self._pos = self._size + off
        return self._pos

    def tell(self):
        return self._pos

    def read(self, n=-1):
        if n is None or n < 0:
            n = self._size - self._pos
        want_start, want_end = self._pos, self._pos + n
        for start, data in self._ranges:
            if start <= want_start and want_end <= start + len(data):
                out = data[want_start - start:want_end - start]
                self._pos = want_end
                return out
        raise IOError(
            f"read outside fetched ranges: "
            f"[{want_start}, {want_end}) not pre-fetched"
        )


class ParallelReader:
    """One overview GeoParquet object behind an HTTPS range URL."""

    def __init__(self, url, session=None):
        self.url = url
        self.session = session or make_session()
        # per-run counters (reset by read_viewport)
        self.requests = 0
        self.bytes = 0
        # footer cache (immutable object => valid for the session)
        self.file_size = None
        self.tail = None       # raw suffix bytes incl. footer
        self.tail_start = None
        self.meta = None       # pyarrow FileMetaData

    # -- raw HTTP ------------------------------------------------
    def _get(self, headers):
        r = self.session.get(self.url, headers=headers)
        r.raise_for_status()
        self.requests += 1
        self.bytes += len(r.content)
        return r

    def _get_range(self, start, end_excl):
        h = {"Range": f"bytes={start}-{end_excl - 1}"}
        return self._get(h).content

    def _get_suffix(self, n):
        r = self._get({"Range": f"bytes=-{n}"})
        m = re.match(
            r"bytes (\d+)-(\d+)/(\d+)", r.headers["Content-Range"]
        )
        return r.content, int(m.group(1)), int(m.group(3))

    # -- footer ---------------------------------------------------
    def fetch_footer(self):
        """1 request in the common case; 2 if the footer > 512 KB."""
        tail, start, size = self._get_suffix(TAIL_GUESS)
        assert tail[-4:] == b"PAR1", "not a parquet file"
        footer_len = int.from_bytes(tail[-8:-4], "little")
        need = footer_len + 8
        if need > len(tail):  # rare: footer bigger than the guess
            tail, start, size = self._get_suffix(need)
        self.file_size = size
        self.tail = tail
        self.tail_start = start
        # pyarrow parses the footer from OUR bytes (no I/O of its own)
        self.meta = pq.read_metadata(io.BytesIO(tail))

    # -- row-group selection (spec §5.1 steps 2-3) ----------------
    def _stats(self, rg):
        out = {}
        for j in range(rg.num_columns):
            col = rg.column(j)
            st = col.statistics
            if st is not None and st.has_min_max:
                out[col.path_in_schema] = (st.min, st.max)
        return out

    def select_row_groups(self, level, bbox):
        """Indices of row groups that can contain matching rows."""
        vxmin, vymin, vxmax, vymax = bbox
        keep = []
        for i in range(self.meta.num_row_groups):
            s = self._stats(self.meta.row_group(i))
            lv = s.get("level")
            if lv is not None and not (lv[0] <= level <= lv[1]):
                continue
            xmin = s.get("bbox.xmin")
            xmax = s.get("bbox.xmax")
            ymin = s.get("bbox.ymin")
            ymax = s.get("bbox.ymax")
            if xmin is not None and xmin[0] > vxmax:
                continue
            if xmax is not None and xmax[1] < vxmin:
                continue
            if ymin is not None and ymin[0] > vymax:
                continue
            if ymax is not None and ymax[1] < vymin:
                continue
            keep.append(i)
        return keep

    # -- byte ranges (spec §5.1 step 4) ---------------------------
    def rg_byte_range(self, i):
        rg = self.meta.row_group(i)
        start, end = None, 0
        for j in range(rg.num_columns):
            c = rg.column(j)
            off = c.data_page_offset
            if c.dictionary_page_offset is not None:
                off = min(off, c.dictionary_page_offset)
            start = off if start is None else min(start, off)
            end = max(end, off + c.total_compressed_size)
        return start, end

    @staticmethod
    def coalesce(ranges, gap=COALESCE_GAP):
        out = []
        for s, e in sorted(ranges):
            if out and s - out[-1][1] <= gap:
                out[-1][1] = max(out[-1][1], e)
            else:
                out.append([s, e])
        return [(s, e) for s, e in out]

    # -- the full protocol ----------------------------------------
    def read_viewport(self, level, bbox):
        """Run the read protocol once. Returns a stats dict.

        Cold if the footer has not been fetched on this reader yet;
        footer-cached otherwise.
        """
        self.requests = 0
        self.bytes = 0
        t0 = time.perf_counter()

        footer_cached = self.meta is not None
        if not footer_cached:
            self.fetch_footer()
        t_footer = time.perf_counter()

        selected = self.select_row_groups(level, bbox)
        ranges = self.coalesce(
            [self.rg_byte_range(i) for i in selected]
        )

        sparse = SparseFile(self.file_size)
        sparse.add(self.tail_start, self.tail)
        with cf.ThreadPoolExecutor(max_workers=POOL) as ex:
            futs = {
                ex.submit(self._get_range, s, e): s
                for s, e in ranges
            }
            for f in cf.as_completed(futs):
                sparse.add(futs[f], f.result())
        t_fetch = time.perf_counter()

        # decode entirely from the fetched bytes
        features = 0
        if selected:
            pf = pq.ParquetFile(sparse, pre_buffer=False)
            table = pf.read_row_groups(selected)
            vxmin, vymin, vxmax, vymax = bbox
            bb = table.column("bbox")
            mask = pc.and_(
                pc.equal(table.column("level"), level),
                pc.and_(
                    pc.and_(
                        pc.less_equal(
                            pc.struct_field(bb, "xmin"), vxmax
                        ),
                        pc.greater_equal(
                            pc.struct_field(bb, "xmax"), vxmin
                        ),
                    ),
                    pc.and_(
                        pc.less_equal(
                            pc.struct_field(bb, "ymin"), vymax
                        ),
                        pc.greater_equal(
                            pc.struct_field(bb, "ymax"), vymin
                        ),
                    ),
                ),
            )
            features = table.filter(mask).num_rows
        t_done = time.perf_counter()

        return {
            "mode": "footer_cached" if footer_cached else "cold",
            "wall_ms": (t_done - t0) * 1000.0,
            "footer_ms": (t_footer - t0) * 1000.0,
            "fetch_ms": (t_fetch - t_footer) * 1000.0,
            "decode_ms": (t_done - t_fetch) * 1000.0,
            "requests": self.requests,
            "bytes": self.bytes,
            "rg_selected": len(selected),
            "rg_total": self.meta.num_row_groups,
            "data_ranges": len(ranges),
            "features": features,
            "level": level,
        }


def main():
    url = sys.argv[1]
    level = int(sys.argv[2])
    bbox = [float(v) for v in sys.argv[3:7]]
    rd = ParallelReader(url)
    for _ in range(2):  # cold, then footer-cached
        st = rd.read_viewport(level, bbox)
        print(
            f"{st['mode']:13s} {st['wall_ms']:8.1f}ms "
            f"(footer {st['footer_ms']:.0f} + fetch "
            f"{st['fetch_ms']:.0f} + decode {st['decode_ms']:.0f}) "
            f"{st['requests']}req {st['bytes']:,}B "
            f"{st['features']}f "
            f"rg {st['rg_selected']}/{st['rg_total']}"
        )


if __name__ == "__main__":
    main()
