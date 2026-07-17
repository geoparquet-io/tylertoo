# Type stubs for the tylertoo pyo3 extension module.
#
# Kept honest by CI: `python -m mypy.stubtest tylertoo` verifies these
# signatures against the compiled module on every PR. If you change a
# #[pyo3(signature = ...)] in crates/python/src/lib.rs, update this file.
from pathlib import Path
from typing import Any

__all__ = ["convert", "export_pmtiles", "overview", "validate"]

def convert(
    input: str,
    output: str,
    min_zoom: int = 0,
    max_zoom: int = 14,
    layer_name: str | None = None,
    tile_size_limit: int | None = None,
    simple_clip_fastpath: bool = True,
) -> None: ...
def overview(
    input: str | list[str],
    output: str,
    *,
    mode: str = "duplicating",
    min_zoom: int = 0,
    max_zoom: int = 6,
    gsds: list[float] | None = None,
    gsd_base: float = 1024.0,
    sort_key: str | None = None,
    sort_direction: str = "desc",
    class_rank_column: str | None = None,
    class_ranks: dict[str, float] | None = None,
    class_rank_unknown: float | None = None,
    no_auto_rank: bool = False,
    simplify_factor: float = 1.0,
    collapse: bool = False,
    collapse_square: bool = False,
    representation: str | None = None,
    cascade: bool = True,
    point_thinning: float | None = None,
    line_thinning: float = 1.0,
    polygon_thinning: float = 1.0,
    line_visibility: float = 2.0,
    polygon_visibility: float = 2.0,
    drop_rate: float = 1.65,
    drop_gamma: float = 1.5,
    density_drop: bool = True,
    cluster: bool = False,
    accumulate_attributes: dict[str, str] | None = None,
    coalesce_lines: bool = True,
    coalesce_snap: float = 1.0,
    coalesce_junction_angle: float = 0.0,
    coalesce_max_level_rows: int = 2_000_000,
    cogp_compat: bool = False,
    row_group_size: int = 10_000,
    full_column_stats: bool = False,
    streaming: bool = True,
    read_batch_size: int = 8192,
    bbox: tuple[float, float, float, float] | None = None,
    filter: str | None = None,
    profile: str = "auto",
    in_flight_batches: int = 0,
    spill_dir: str | Path | None = None,
) -> dict[str, Any]: ...
def export_pmtiles(
    input: str,
    output: str,
    *,
    layer_name: str = "overview",
    tile_buffer: int = 8,
    extent: int = 4096,
    tile_size_limit: int | None = None,
    simple_clip_fastpath: bool = True,
    partition_wave: int = 0,
) -> dict[str, Any]: ...
def validate(file: str) -> dict[str, Any]: ...
