"""Check the extract, then export its finest level for tiling."""

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

con.sql("""
    SELECT count(*) AS fields, round(sum("metrics:area") / 1e4) AS hectares
    FROM 'fields-raw.parquet'
""").show()
