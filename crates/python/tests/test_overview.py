"""Tests for the gpq_tiles overview pipeline bindings.

Covers overview(), export_pmtiles(), and validate(): API surface, knob
plumbing (one happy-path test per knob group), error propagation, and
output-file validity via the validate() checklist.
"""

import tempfile
from pathlib import Path

import pytest

import gpq_tiles

FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
REALDATA_DIR = FIXTURES_DIR / "realdata"

BUILDINGS = REALDATA_DIR / "open-buildings.parquet"
ROADS = REALDATA_DIR / "road-detections.parquet"

needs_buildings = pytest.mark.skipif(
    not BUILDINGS.exists(), reason="open-buildings.parquet fixture not available"
)
needs_roads = pytest.mark.skipif(
    not ROADS.exists(), reason="road-detections.parquet fixture not available"
)


class TestRemoteInput:
    """Remote-URL input handling (#210) — no network needed."""

    def test_unsupported_scheme_is_helpful_error(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "o.parquet"
            with pytest.raises(RuntimeError, match="s3://"):
                gpq_tiles.overview("ftp://example.com/x.parquet", str(out))

    def test_docstring_mentions_remote_urls(self):
        doc = gpq_tiles.overview.__doc__
        assert doc is not None
        assert "s3://" in doc


class TestOverviewApi:
    """API-surface tests for overview() (no fixture needed)."""

    def test_overview_exists(self):
        assert hasattr(gpq_tiles, "overview")
        assert callable(gpq_tiles.overview)

    def test_overview_has_docstring(self):
        doc = gpq_tiles.overview.__doc__
        assert doc is not None
        assert "overview" in doc.lower()
        assert "GeoParquet" in doc

    def test_overview_requires_input_output(self):
        with pytest.raises(TypeError):
            gpq_tiles.overview()  # type: ignore[call-arg]

    def test_overview_invalid_mode(self):
        with pytest.raises(ValueError, match=r"[Ii]nvalid mode"):
            gpq_tiles.overview("/nonexistent.parquet", "/tmp/out.parquet", mode="bogus")

    def test_overview_invalid_sort_direction(self):
        with pytest.raises(ValueError, match="sort_direction"):
            gpq_tiles.overview(
                "/nonexistent.parquet", "/tmp/out.parquet", sort_direction="sideways"
            )

    def test_overview_sort_key_and_class_rank_conflict(self):
        with pytest.raises(ValueError, match="mutually exclusive"):
            gpq_tiles.overview(
                "/nonexistent.parquet",
                "/tmp/out.parquet",
                sort_key="population",
                class_rank_column="road_class",
                class_ranks={"motorway": 5.0},
            )

    def test_overview_class_ranks_without_column(self):
        with pytest.raises(ValueError, match="class_rank"):
            gpq_tiles.overview(
                "/nonexistent.parquet",
                "/tmp/out.parquet",
                class_ranks={"motorway": 5.0},
            )

    def test_overview_class_rank_column_without_ranks(self):
        with pytest.raises(ValueError, match="class_rank"):
            gpq_tiles.overview(
                "/nonexistent.parquet",
                "/tmp/out.parquet",
                class_rank_column="road_class",
            )

    def test_overview_empty_class_ranks(self):
        with pytest.raises(ValueError, match="class_ranks"):
            gpq_tiles.overview(
                "/nonexistent.parquet",
                "/tmp/out.parquet",
                class_rank_column="road_class",
                class_ranks={},
            )

    def test_overview_accumulate_requires_cluster(self):
        with pytest.raises(ValueError, match="cluster"):
            gpq_tiles.overview(
                "/nonexistent.parquet",
                "/tmp/out.parquet",
                accumulate_attributes={"population": "sum"},
            )

    def test_overview_invalid_accumulate_op(self):
        with pytest.raises(ValueError, match="op"):
            gpq_tiles.overview(
                "/nonexistent.parquet",
                "/tmp/out.parquet",
                cluster=True,
                accumulate_attributes={"population": "median"},
            )

    def test_overview_cluster_requires_duplicating(self):
        with pytest.raises(ValueError, match="duplicating"):
            gpq_tiles.overview(
                "/nonexistent.parquet",
                "/tmp/out.parquet",
                mode="partitioning",
                cluster=True,
            )

    @needs_buildings
    def test_overview_invalid_gsds(self):
        """Non-decreasing GSD lists are rejected by core (ValueError)."""
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "out.parquet"
            with pytest.raises(ValueError, match="level"):
                gpq_tiles.overview(str(BUILDINGS), str(out), gsds=[100.0, 200.0])

    @needs_buildings
    def test_overview_invalid_zoom_range(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "out.parquet"
            with pytest.raises(ValueError, match="level"):
                gpq_tiles.overview(str(BUILDINGS), str(out), min_zoom=8, max_zoom=2)

    def test_overview_nonexistent_input(self):
        with pytest.raises(RuntimeError):
            gpq_tiles.overview("/nonexistent/input.parquet", "/tmp/out.parquet")


class TestExportPmtilesApi:
    def test_export_pmtiles_exists(self):
        assert hasattr(gpq_tiles, "export_pmtiles")
        assert callable(gpq_tiles.export_pmtiles)

    def test_export_pmtiles_has_docstring(self):
        doc = gpq_tiles.export_pmtiles.__doc__
        assert doc is not None
        assert "PMTiles" in doc

    def test_export_pmtiles_nonexistent_input(self):
        with pytest.raises(RuntimeError):
            gpq_tiles.export_pmtiles("/nonexistent.parquet", "/tmp/out.pmtiles")


class TestValidateApi:
    def test_validate_exists(self):
        assert hasattr(gpq_tiles, "validate")
        assert callable(gpq_tiles.validate)

    def test_validate_has_docstring(self):
        doc = gpq_tiles.validate.__doc__
        assert doc is not None
        assert "check" in doc.lower()

    def test_validate_nonexistent_file(self):
        with pytest.raises(RuntimeError):
            gpq_tiles.validate("/nonexistent.parquet")


@needs_buildings
class TestOverviewIntegration:
    """End-to-end knob-group coverage against the open-buildings fixture."""

    def _overview(self, tmpdir, **kwargs):
        out = Path(tmpdir) / "overview.parquet"
        report = gpq_tiles.overview(str(BUILDINGS), str(out), **kwargs)
        assert out.exists()
        return out, report

    def test_overview_defaults_and_report(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, report = self._overview(tmpdir)

            assert report["mode"] == "duplicating"
            assert report["input_features"] > 0
            assert report["total_rows"] >= report["input_features"]
            assert report["duration_secs"] > 0
            # Local inputs carry no remote fetch counters (#210).
            assert report["remote_fetch"] is None
            # z0..z6 requested; coarse levels where every (tiny) building falls
            # below the visibility gate are omitted, so <= 7 levels survive and
            # the canonical (finest) one is always z6 with every input feature.
            assert 1 <= len(report["levels"]) <= 7
            for i, lvl in enumerate(report["levels"]):
                assert lvl["level"] == i
                assert lvl["gsd"] > 0
                assert lvl["feature_count"] >= 0
                assert lvl["vertex_count"] >= 0
            assert report["levels"][-1]["zoom"] == 6
            assert report["levels"][-1]["feature_count"] == report["input_features"]

            # Output must pass the spec conformance checklist.
            result = gpq_tiles.validate(str(out))
            assert result["valid"] is True
            assert len(result["checks"]) > 0
            assert all(c["passed"] for c in result["checks"])

    def test_overview_zoom_range(self):
        """Fine zooms suit the small building footprints: all levels survive."""
        with tempfile.TemporaryDirectory() as tmpdir:
            _, report = self._overview(tmpdir, min_zoom=11, max_zoom=14)
            assert len(report["levels"]) == 4
            assert report["levels"][0]["zoom"] == 11
            assert report["levels"][-1]["zoom"] == 14

    def test_overview_explicit_gsds(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            _, report = self._overview(tmpdir, gsds=[8.0, 2.0], polygon_visibility=0.0)
            assert len(report["levels"]) == 2
            assert report["levels"][0]["gsd"] == pytest.approx(8.0)
            # Explicit-GSD levels carry no zoom.
            assert report["levels"][0]["zoom"] is None

    def test_overview_gsd_base(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            _, report = self._overview(tmpdir, max_zoom=3, gsd_base=512.0)
            # gsd(z) = C / base / 2^z; the canonical level is always present.
            assert report["levels"][-1]["gsd"] == pytest.approx(
                40075016.685578488 / 512.0 / 2**3
            )

    def test_overview_partitioning_mode(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, report = self._overview(tmpdir, mode="partitioning", max_zoom=4)
            assert report["mode"] == "partitioning"
            # Partitioning: each feature appears exactly once.
            assert report["total_rows"] == report["input_features"]
            assert gpq_tiles.validate(str(out))["valid"] is True

    def test_overview_thinning_and_visibility_knobs(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            _, dense = self._overview(
                tmpdir,
                min_zoom=10,
                max_zoom=14,
                point_thinning=1.0,
                line_thinning=0.5,
                polygon_thinning=0.5,
                line_visibility=0.5,
                polygon_visibility=0.0,
            )
        with tempfile.TemporaryDirectory() as tmpdir:
            _, sparse = self._overview(
                tmpdir,
                min_zoom=10,
                max_zoom=14,
                polygon_thinning=8.0,
                polygon_visibility=16.0,
            )
        assert sparse["total_rows"] < dense["total_rows"]

    def test_overview_simplify_factor_and_collapse(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            _, crude = self._overview(
                tmpdir, max_zoom=4, simplify_factor=8.0, collapse=True
            )
        with tempfile.TemporaryDirectory() as tmpdir:
            _, fine = self._overview(tmpdir, max_zoom=4, simplify_factor=0.1)
        assert crude["total_vertices"] <= fine["total_vertices"]

    def test_overview_sort_key(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, _ = self._overview(tmpdir, max_zoom=4, sort_key="area_in_meters")
            assert gpq_tiles.validate(str(out))["valid"] is True

    def test_overview_sort_key_ascending(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, _ = self._overview(
                tmpdir,
                max_zoom=4,
                sort_key="area_in_meters",
                sort_direction="asc",
            )
            assert gpq_tiles.validate(str(out))["valid"] is True

    def test_overview_sort_key_missing_column(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "overview.parquet"
            with pytest.raises(ValueError, match="not found"):
                gpq_tiles.overview(str(BUILDINGS), str(out), sort_key="no_such_column")

    def test_overview_no_auto_rank(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, _ = self._overview(tmpdir, max_zoom=4, no_auto_rank=True)
            assert gpq_tiles.validate(str(out))["valid"] is True

    def test_overview_density_budget_knobs(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            _, budget_off = self._overview(tmpdir, max_zoom=4, density_drop=False)
        with tempfile.TemporaryDirectory() as tmpdir:
            _, hard = self._overview(tmpdir, max_zoom=4, drop_rate=4.0, drop_gamma=2.0)
        assert hard["total_rows"] <= budget_off["total_rows"]

    def test_overview_cluster_and_accumulate(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, _ = self._overview(
                tmpdir,
                max_zoom=4,
                cluster=True,
                accumulate_attributes={"area_in_meters": "sum"},
            )
            assert gpq_tiles.validate(str(out))["valid"] is True

    def test_overview_accumulate_missing_column(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "overview.parquet"
            with pytest.raises(ValueError, match="not found"):
                gpq_tiles.overview(
                    str(BUILDINGS),
                    str(out),
                    cluster=True,
                    accumulate_attributes={"no_such_column": "sum"},
                )

    def test_overview_coalesce_knobs(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, _ = self._overview(
                tmpdir,
                max_zoom=4,
                coalesce_lines=True,
                coalesce_snap=2.0,
                coalesce_junction_angle=30.0,
                coalesce_max_level_rows=100_000,
            )
            assert gpq_tiles.validate(str(out))["valid"] is True

    def test_overview_no_coalesce(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, _ = self._overview(tmpdir, max_zoom=4, coalesce_lines=False)
            assert gpq_tiles.validate(str(out))["valid"] is True

    def test_overview_streaming_off_matches_streaming_on(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            _, on = self._overview(tmpdir, max_zoom=4, streaming=True)
        with tempfile.TemporaryDirectory() as tmpdir:
            _, off = self._overview(tmpdir, max_zoom=4, streaming=False)
        assert on["total_rows"] == off["total_rows"]
        assert [lvl["feature_count"] for lvl in on["levels"]] == [
            lvl["feature_count"] for lvl in off["levels"]
        ]

    def test_overview_read_batch_size(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            _, small = self._overview(tmpdir, max_zoom=4, read_batch_size=512)
        with tempfile.TemporaryDirectory() as tmpdir:
            _, default = self._overview(tmpdir, max_zoom=4)
        assert small["total_rows"] == default["total_rows"]

    def test_overview_writer_options(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, _ = self._overview(
                tmpdir, max_zoom=4, row_group_size=500, full_column_stats=True
            )
            assert gpq_tiles.validate(str(out))["valid"] is True

    def test_overview_cogp_compat(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out, _ = self._overview(
                tmpdir, mode="partitioning", max_zoom=4, cogp_compat=True
            )
            assert gpq_tiles.validate(str(out))["valid"] is True


@needs_roads
class TestOverviewClassRankIntegration:
    """road-detections.parquet carries a string `geometry_type` column."""

    def test_overview_class_rank(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "overview.parquet"
            gpq_tiles.overview(
                str(ROADS),
                str(out),
                max_zoom=4,
                class_rank_column="geometry_type",
                class_ranks={"LineString": 3.0, "MultiLineString": 2.0},
            )
            assert gpq_tiles.validate(str(out))["valid"] is True

    def test_overview_class_rank_unknown_override(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "overview.parquet"
            gpq_tiles.overview(
                str(ROADS),
                str(out),
                max_zoom=4,
                class_rank_column="geometry_type",
                class_ranks={"LineString": 3.0},
                class_rank_unknown=-5.0,
            )
            assert out.exists()

    def test_overview_class_rank_missing_column(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "overview.parquet"
            with pytest.raises(ValueError, match="not found"):
                gpq_tiles.overview(
                    str(ROADS),
                    str(out),
                    class_rank_column="no_such_column",
                    class_ranks={"LineString": 3.0},
                )


@needs_buildings
class TestExportPmtilesIntegration:
    def _make_overview(self, tmpdir, **kwargs):
        out = Path(tmpdir) / "overview.parquet"
        # polygon_visibility=0 keeps every level populated for the tiny
        # building footprints, so the export covers the full zoom range.
        kwargs.setdefault("polygon_visibility", 0.0)
        gpq_tiles.overview(str(BUILDINGS), str(out), min_zoom=11, max_zoom=14, **kwargs)
        return out

    def test_export_defaults_and_report(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            ovr = self._make_overview(tmpdir)
            pm = Path(tmpdir) / "out.pmtiles"
            report = gpq_tiles.export_pmtiles(str(ovr), str(pm))

            assert pm.exists()
            assert pm.stat().st_size > 0
            assert report["mode"] == "duplicating"
            assert report["min_zoom"] == 11
            assert report["max_zoom"] == 14
            assert report["total_tiles"] > 0
            assert report["oversized_tiles"] == 0
            assert len(report["zooms"]) == 4
            for z in report["zooms"]:
                assert z["zoom"] == z["level"] + report["min_zoom"]
                assert z["tile_count"] > 0

    def test_export_knobs(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            ovr = self._make_overview(tmpdir)
            pm = Path(tmpdir) / "out.pmtiles"
            report = gpq_tiles.export_pmtiles(
                str(ovr),
                str(pm),
                layer_name="buildings",
                tile_buffer=16,
                extent=8192,
                tile_size_limit=200_000,
            )
            assert pm.exists()
            assert report["total_tiles"] > 0

    def test_export_rejects_non_overview_input(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            pm = Path(tmpdir) / "out.pmtiles"
            with pytest.raises(RuntimeError):
                gpq_tiles.export_pmtiles(str(BUILDINGS), str(pm))


@needs_buildings
class TestValidateIntegration:
    def test_validate_structure(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            ovr = Path(tmpdir) / "overview.parquet"
            gpq_tiles.overview(str(BUILDINGS), str(ovr), max_zoom=3)

            result = gpq_tiles.validate(str(ovr))
            assert set(result.keys()) == {"valid", "checks"}
            assert isinstance(result["valid"], bool)
            for check in result["checks"]:
                assert isinstance(check["name"], str)
                assert isinstance(check["passed"], bool)
                assert isinstance(check["message"], str)

    def test_validate_rejects_plain_geoparquet(self):
        """A plain (non-overview) GeoParquet file must fail validation."""
        result = gpq_tiles.validate(str(BUILDINGS))
        assert result["valid"] is False
        assert any(not c["passed"] for c in result["checks"])
