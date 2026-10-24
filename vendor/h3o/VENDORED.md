# Vendored h3o

This is [h3o](https://github.com/HydroniumLabs/h3o) 0.11.0 (upstream commit
`fa1557f`, identical to the crates.io release) with two additions, applied via
`[patch.crates-io]` in the workspace `Cargo.toml`. Every cell either returns
is bit-identical to `LatLng::to_cell`.

- `LatLng::to_cells(resolutions)` indexes one point at several resolutions,
  computing the resolution-independent work (n-vector, closest face, gnomonic
  radius and azimuth) once.
- `LatLngIndexer` indexes a sequence of nearby points. It remembers the last
  face-centered `IJK` coordinates and cell per resolution and only re-encodes
  the cell index (digit extraction, base-cell rotation) when they change,
  which consecutive raster pixels usually share (90% of 30 m pixels at
  resolution 9).

`upstream.patch` is the complete diff against upstream, including the
equivalence tests and scanline benchmarks added to h3o's own suite; apply it
to an h3o checkout to run h3o's tests or to open the upstream PR. Only `src/`,
`LICENSE`, `README.md` and a `Cargo.toml` without dev-only targets are
vendored here.

Remove this directory and the `[patch.crates-io]` entry once an h3o release
contains the API.
