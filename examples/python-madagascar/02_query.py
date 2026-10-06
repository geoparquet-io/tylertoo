"""Query the overview file as plain GeoParquet with DuckDB."""

import duckdb

OV = "madagascar-ov.parquet"

# Every row carries the `level` it belongs to; one GROUP BY shows the pyramid.
duckdb.sql(f"""
    SELECT level, count(*) AS features
    FROM '{OV}'
    GROUP BY level
    ORDER BY level
""").show()

# Which districts are still on the map at level 1 (z2)?
duckdb.sql(f"""
    SELECT adm2_name AS district, count(*) AS features
    FROM '{OV}'
    WHERE level = 1
    GROUP BY district
    ORDER BY features DESC, district
    LIMIT 5
""").show()

# Pull one level out as its own GeoParquet file.
duckdb.sql(f"COPY (SELECT * FROM '{OV}' WHERE level = 4) TO 'level4.parquet'")
print(duckdb.sql("SELECT count(*) FROM 'level4.parquet'").fetchone()[0], "rows")
