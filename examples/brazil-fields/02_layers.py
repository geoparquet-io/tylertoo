"""Check the extract, then derive the two layers the map needs."""

import duckdb

import tylertoo

OV = "fields-ov.parquet"

result = tylertoo.validate(OV)
print(f"valid={result['valid']}  checks={len(result['checks'])}")

con = duckdb.connect()
con.sql("INSTALL spatial; LOAD spatial;")
finest = con.sql(f"SELECT max(level) FROM '{OV}'").fetchone()[0]

# The finest level holds every field at full detail: it is the extract.
con.sql(f"""
    COPY (SELECT * EXCLUDE (level, coalesced_count) FROM '{OV}' WHERE level = {finest})
    TO 'fields-raw.parquet' (FORMAT parquet)
""")

# Zoomed out, single fields are too small to see. Count them per
# 0.01-degree cell (about 1 km) instead, and keep the cell as a square.
con.sql(f"""
    COPY (
        SELECT count(*) AS fields,
               round(sum("metrics:area") / 1e4, 1) AS hectares,
               ST_MakeEnvelope(gx * 0.01, gy * 0.01, (gx + 1) * 0.01, (gy + 1) * 0.01)
                   AS geometry
        FROM (
            SELECT "metrics:area",
                   floor(ST_X(ST_Centroid(geometry)) / 0.01) AS gx,
                   floor(ST_Y(ST_Centroid(geometry)) / 0.01) AS gy
            FROM '{OV}' WHERE level = {finest}
        )
        GROUP BY gx, gy
        ORDER BY gx, gy
    ) TO 'density.parquet' (FORMAT parquet)
""")

con.sql("""
    SELECT count(*) AS cells, sum(fields) AS fields,
           max(fields) AS busiest_cell, round(sum(hectares)) AS hectares
    FROM 'density.parquet'
""").show()
