INSTALL spatial; LOAD spatial;
SET preserve_insertion_order = true;

CREATE TABLE canon AS
SELECT id, building_class, height, geometry, bbox,
       row_number() OVER () AS rn
FROM read_parquet('buildings-de-central.dup.parquet')
WHERE level = 7;

CREATE TABLE lv AS
SELECT id, level, geometry
FROM read_parquet('buildings-de-central.dup.parquet')
WHERE level < 7;

CREATE TABLE piv AS
SELECT id,
  any_value(geometry) FILTER (level = 0) AS geom_z7,
  any_value(geometry) FILTER (level = 1) AS geom_z8,
  any_value(geometry) FILTER (level = 2) AS geom_z9,
  any_value(geometry) FILTER (level = 3) AS geom_z10,
  any_value(geometry) FILTER (level = 4) AS geom_z11,
  any_value(geometry) FILTER (level = 5) AS geom_z12,
  any_value(geometry) FILTER (level = 6) AS geom_z13,
  min(level) AS first_level
FROM lv GROUP BY id;

COPY (
  SELECT c.id, c.building_class, c.height,
    p.geom_z7, p.geom_z8, p.geom_z9, p.geom_z10,
    p.geom_z11, p.geom_z12, p.geom_z13,
    c.geometry, c.bbox
  FROM canon c LEFT JOIN piv p USING (id)
  ORDER BY COALESCE(p.first_level, 7), c.rn
) TO 'buildings-de-central.cpz.parquet'
(FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 10000,
 KV_METADATA {
  'geo:overviews': '{"version":"0.3.0-proto","mode":"column-per-zoom","levels":[{"zoom":7,"column":"geom_z7"},{"zoom":8,"column":"geom_z8"},{"zoom":9,"column":"geom_z9"},{"zoom":10,"column":"geom_z10"},{"zoom":11,"column":"geom_z11"},{"zoom":12,"column":"geom_z12"},{"zoom":13,"column":"geom_z13"},{"zoom":14,"column":"geometry"}]}'
 });
