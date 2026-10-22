# `bench-coalesce-v1`: input sources and licenses

The coalesce benchmark (`benchmarks/coalesce/run.py`, #755) uses these
inputs. Each file remains unchanged under this tag. If you add or change an
input, publish it under a new tag. `SHA256SUMS` lists each file's checksum, and
`benchmarks/coalesce/bench.toml` pins the same checksums.

The benchmark reads the `*-hilbert.parquet` files. `prepare_data.py` sorts
them with
`uvx --from geoparquet-io==1.7.0 gpio sort hilbert --add-bbox --row-group-size 49152`.
Sorting changes
only the row order and row groups, without changing any row. The other files
retain the data as delivered for provenance and re-sorting.

## FTW Moldova: `moldova.parquet`, `moldova-hilbert.parquet`

The Moldova extract contains 631,910 field polygons from the Fields of The
World global field-boundary predictions. The Taylor Geospatial Institute and
the Microsoft AI for Good Research Lab produce these predictions. The extract
is licensed under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).

- Source:
  `https://data.source.coop/ftw/global-data/predictions/vectors/alpha/results-by-admin/admin:country_code=MD/Moldova.parquet`,
  downloaded 2026-10-20: 103,621,959 bytes, last modified 2026-06-11.
- Collection: [Fields of The World global data](https://source.coop/ftw/global-data).
- Changes: `moldova-hilbert.parquet` is the same data in Hilbert order.
  `moldova.parquet` is unchanged.

## Madagascar admin level 4: `madagascar-adm4-hilbert.parquet`

The Madagascar admin-4 boundaries contain 17,465 polygons from
[FieldMaps edge-matched humanitarian boundaries](https://fieldmaps.io),
courtesy of Maxym Malynowsky. The data is © FieldMaps and licensed under the
[Open Data Commons Open Database License (ODbL) v1.0](https://opendatacommons.org/licenses/odbl/1-0/).
It contains [UN OCHA COD-AB data from Madagascar's BNGRC](https://data.humdata.org/dataset/cod-ab-mdg),
licensed under
[CC BY 3.0 IGO](https://creativecommons.org/licenses/by/3.0/igo/legalcode).

- Source: `fieldmaps-madagascar-adm4.parquet` on this repository's
  `fixtures-v1` release, a Madagascar filter of
  `https://data.fieldmaps.io/edge-matched/humanitarian/intl/adm4_polygons.parquet`.
- Changes: the preparation process adds GeoParquet 1.1 metadata (the fixture
  has none), then sorts the data into Hilbert order with a `geometry_bbox`
  covering column. The derived database remains under ODbL 1.0.

## NZ Parcels: `nz-parcels.parquet`, `nz-parcels-hilbert.parquet`

NZ Parcels contains 4,343,547 polygons from LINZ layer 51571. Attribution, as
LINZ specifies for adapted data:

> Contains data sourced from the LINZ Data Service licensed for reuse under
> [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).

- Source: [NZ Parcels, LINZ layer 51571](https://data.linz.govt.nz/layer/51571/),
  retrieved in pages over WFS in EPSG:4326 by
  `benchmarks/coalesce/fetch_nz_parcels.py` on 2026-10-20.
- Changes: the conversion from GeoJSON pages to GeoParquet keeps the columns
  that `fetch_nz_parcels.py` lists and skips null geometries.
  `nz-parcels-hilbert.parquet` is the same data in Hilbert order, with a bbox
  covering column added.
