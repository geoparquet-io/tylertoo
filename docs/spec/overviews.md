# The `geo:overviews` format

tylertoo's overview files follow the `geo:overviews` specification:
COG-style multi-resolution overviews embedded in a GeoParquet file that
remains a fully valid GeoParquet 1.1 file. The full specification is
rendered below from its canonical source,
[`context/OVERVIEWS_SPEC.md`](https://github.com/geoparquet-io/tylertoo/blob/main/context/OVERVIEWS_SPEC.md)
in the tylertoo repository, so this page and the repository file can't
drift apart.

!!! warning "Draft specification"

    `geo:overviews` is a **draft**. The format can still change in
    response to implementation experience and GeoParquet community
    review. The formal stability declaration — the compatibility
    promise tylertoo 1.0 makes to files it has already written — is
    not yet part of this document; it is tracked in
    [#441](https://github.com/geoparquet-io/tylertoo/issues/441).

!!! info "Status"

    - **Spec**: draft, version 0.2.0.
    - **Implementation**: tylertoo writes spec version 0.2.0 (as of
      tylertoo 0.8.0) and checks files against it with its `validate`
      subcommand.
    - **Standardization**: submission to the official GeoParquet
      process
      ([opengeospatial/geoparquet](https://github.com/opengeospatial/geoparquet))
      is pending — nothing has been filed yet. The remaining steps are
      tracked in
      [#442](https://github.com/geoparquet-io/tylertoo/issues/442).

---

--8<-- "context/OVERVIEWS_SPEC.md"
