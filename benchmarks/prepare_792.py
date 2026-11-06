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
