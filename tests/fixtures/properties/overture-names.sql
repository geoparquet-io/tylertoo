-- Regenerates overture-names.parquet (#434): an Overture-shaped GeoParquet
-- with a `names` struct (a string plus a string-keyed map), a `sources`
-- list, and a binary column the export cannot encode. 20 points, one row
-- group, CRS84.
--
--   duckdb < tests/fixtures/properties/overture-names.sql
LOAD spatial;
COPY (
  SELECT
    i AS id,
    {'primary': 'Place ' || i, 'common': MAP {'fr': 'Lieu ' || i, 'de': 'Ort ' || i}} AS names,
    ['osm', 'meta'] AS sources,
    encode('blob' || i) AS raw,
    ST_Point(-120.0 + i * 0.01, 40.0 + i * 0.01) AS geometry
  FROM range(1, 21) t(i)
) TO 'tests/fixtures/properties/overture-names.parquet' (FORMAT parquet);
