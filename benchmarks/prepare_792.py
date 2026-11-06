# /// script
# requires-python = ">=3.11"
# dependencies = ["duckdb"]
# ///
"""prepare_792.py <shapefile> <out.parquet>: the #792 input of valve_guard.py
(1.36M Bavarian buildings), reprojected to EPSG:4326, Hilbert-sorted, with
`_size` (area) for --sort-key. Not byte-stable across DuckDB versions, so
valve_guard.py records the output's sha256 with its results.

    uv run benchmarks/prepare_792.py buildings.shp ~/bench-data/792/buildings.parquet
"""
import duckdb, sys
shp, out = sys.argv[1], sys.argv[2]
con = duckdb.connect()
con.execute("INSTALL spatial; LOAD spatial; SET geometry_always_xy = true")
con.execute(f"""
    CREATE TABLE b AS
    SELECT * EXCLUDE (geom), ST_Transform(geom, 'EPSG:25832', 'EPSG:4326') AS geometry
    FROM ST_Read('{shp}')
""")
con.execute(f"""
    COPY (
        SELECT *, ST_Area(geometry) AS _size FROM b
        ORDER BY ST_Hilbert(geometry, (SELECT {{'min_x': min(ST_XMin(geometry)), 'min_y': min(ST_YMin(geometry)),
                                               'max_x': max(ST_XMax(geometry)), 'max_y': max(ST_YMax(geometry))}}::BOX_2D FROM b))
    ) TO '{out}' (FORMAT parquet, COMPRESSION zstd, ROW_GROUP_SIZE 100000)
""")
print(con.execute(f"SELECT count(*) FROM '{out}'").fetchone()[0], "polygons")
