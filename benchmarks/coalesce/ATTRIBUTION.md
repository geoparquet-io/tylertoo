# bench-coalesce-v1: inputs and their licenses

These are the inputs to the coalesce benchmark (`benchmarks/coalesce/run.py`,
#755). Each file is published unchanged under this tag. A new or changed input
gets a new tag. `SHA256SUMS` lists each file's checksum, and
`benchmarks/coalesce/bench.toml` pins the same checksums.

The `*-hilbert.parquet` files are the ones the benchmark reads. They were
sorted with `uvx --from geoparquet-io==1.7.0 gpio sort hilbert --add-bbox
--row-group-size 49152` (`benchmarks/coalesce/prepare_data.py`). Sorting does
not change any row, only their order and the row groups. The other files are
the data as delivered, kept for provenance and for re-sorting.

## FTW Moldova: `moldova.parquet`, `moldova-hilbert.parquet`

The Moldova extract (631,910 field polygons) of the Fields of The World
global field-boundary predictions, by the Taylor Geospatial Institute and the
Microsoft AI for Good Research Lab. It is licensed under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).

- Source:
  `https://data.source.coop/ftw/global-data/predictions/vectors/alpha/results-by-admin/admin:country_code=MD/Moldova.parquet`,
  downloaded 2026-10-20: 103,621,959 bytes, last modified 2026-06-11.
- Collection: <https://source.coop/ftw/global-data>.
- Changes: `moldova-hilbert.parquet` is the same data in Hilbert order.
  `moldova.parquet` is unchanged.

## Madagascar admin level 4: `madagascar-adm4-hilbert.parquet`

The Madagascar admin-4 boundaries (17,465 polygons) from FieldMaps
edge-matched humanitarian boundaries (<https://fieldmaps.io>), courtesy of
Maxym Malynowsky. The data is © FieldMaps and licensed under the
[Open Data Commons Open Database License (ODbL) v1.0](https://opendatacommons.org/licenses/odbl/1-0/).
It contains UN OCHA COD-AB data from Madagascar's BNGRC
(<https://data.humdata.org/dataset/cod-ab-mdg>), under
[CC BY 3.0 IGO](https://creativecommons.org/licenses/by/3.0/igo/legalcode).

- Source: `fieldmaps-madagascar-adm4.parquet` on this repository's
  `fixtures-v1` release, a Madagascar filter of
  `https://data.fieldmaps.io/edge-matched/humanitarian/intl/adm4_polygons.parquet`.
- Changes: GeoParquet 1.1 metadata added (the fixture has none), then sorted
  into Hilbert order with a `geometry_bbox` covering column. As a derived
  database, the file stays under ODbL 1.0.

## NZ Parcels: `nz-parcels.parquet`, `nz-parcels-hilbert.parquet`

NZ Parcels (LINZ layer 51571, 4,343,547 polygons). This file contains data
sourced from the LINZ Data Service, licensed for reuse under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).

- Source: <https://data.linz.govt.nz/layer/51571/>, paged over WFS in EPSG:4326
  by `benchmarks/coalesce/fetch_nz_parcels.py` on 2026-10-20.
- Changes: converted from GeoJSON pages to GeoParquet, keeping the columns
  that `fetch_nz_parcels.py` lists and skipping null geometries.
  `nz-parcels-hilbert.parquet` is the same data in Hilbert order, with a bbox
  covering column added.
