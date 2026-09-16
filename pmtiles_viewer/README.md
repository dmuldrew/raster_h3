# PMTiles Hexagon Studio

[← Back to README](../README.md) · [PMTiles v3 Technical Details](../docs/pmtiles.md)

A lightweight, GPU-accelerated web studio built on [MapLibre GL JS](https://maplibre.org/) and the [PMTiles v3 specification](https://github.com/protomaps/PMTiles) for visual inspection of multi-resolution H3 hexagonal vector tile pyramids.

## Highlights

- **Zero Backend Services** — Runs entirely client-side via HTTP byte-range queries (`pmtiles.FetchSource`) or 100% offline via drag-and-drop (`pmtiles.FileSource`).
- **Auto-Detects Continuous vs. Categorical** — Dynamically switches between continuous surfaces (elevation, temperature, NDVI) and discrete classifications (land cover, zoning, LANDFIRE fuel models).
- **3D Hexagon Extrusion** — Extrude hexagons in WebGL with variable pitch, bearing, and property-based height scaling.
- **Resolution-Adaptive Color Calibration** — Automatically adapts color ramp min/max across pyramid zoom transitions.
- **Built-in Cartographic Colormaps** — Turbo, Viridis, Magma, Plasma, Inferno, Cividis, Spectral, Coolwarm, Thermal, and categorical color tables.
- **LANDFIRE FBFM40 Built-in** — Complete 40-class fire behavior palette with editable labels and live color pickers.
- **Real-Time HUD & Tooltips** — Live viewport coordinates, zoom level, active H3 resolution, and cell attribute readouts on hover.

---

## Quickstart

### Docker Compose (Recommended)

```bash
docker compose up viewer          # foreground
docker compose up -d viewer       # detached
```

### Standalone Docker

```bash
docker run --rm -p 8080:8080 -v $(pwd):/app python:3.11-slim python3 -u /app/pmtiles_viewer/server.py 8080
```

### Local Python Server

```bash
python3 pmtiles_viewer/server.py 8080
```

### Offline Drag-and-Drop (No Server)

Open `pmtiles_viewer/index.html` directly in your browser. Chrome blocks `fetch()` on `file://` URLs, so use the sidebar dropzone (**📁 Click to select file from disk**) or drag-and-drop any `.pmtiles` file to load it via the browser's native `FileReader` API.

---

**Once running**, open **`http://localhost:8080/pmtiles_viewer/`** (or **`http://localhost:8080/`**).

Enter any PMTiles path relative to the repo root in the sidebar input (e.g., `/data/sample_sf.pmtiles`). The viewer streams only the byte ranges needed for your current viewport — no full-file downloads.

---

## Supported Data Schemas

### Continuous Pyramids

Generated via `h3_raster_to_pmtiles(...)` or the `raster_to_pmtiles` CLI.

| Attribute | Type | Description |
| :--- | :--- | :--- |
| `mean` | Float | Average pixel value in the H3 cell |
| `min` | Float | Minimum pixel value |
| `max` | Float | Maximum pixel value |
| `sum` | Float | Total sum of pixel values |
| `count` | Float | Number of valid (non-NoData) samples |
| `stddev` | Float | Standard deviation of cell values |
| `resolution` | Integer | H3 grid resolution (e.g., 6–10) |

### Categorical Pyramids

Generated from classification rasters (LANDFIRE, ESA WorldCover, NLCD).

| Attribute | Type | Description |
| :--- | :--- | :--- |
| `majority_class` | Integer | Dominant raster category class ID |
| `majority_fraction` | Float | Dominance fraction (0.0 to 1.0) |
| `purity` | Float | Class purity ratio |
| `total_count` | Float | Total valid pixel count in cell |
| `distinct_classes` | Integer | Number of unique classes in cell |

---

## Studio Controls

### Layers (`📂 Data Source & Metric`)

- **Active PMTiles Archive** — Select preloaded demos or enter a custom path.
- **Fit Extent (`🎯`)** — Animate the camera to the dataset bounding box.
- **Colorize Attribute** — Choose which property drives fill colors.
- **Adaptive Scale** — Auto-adjusts thresholds as you zoom across H3 resolutions.
- **Visible Extent Calibration (`👁️`)** — Samples rendered viewport tiles for maximum local contrast.
- **3D Extrusion (`🏢`)** — Toggle volumetric polygons scaled by value or certainty.
- **Opacity & Wireframe** — Control polygon opacity and cell outline visibility.

### Colormaps (`🎨 Palettes & Categorical Styling`)

- **Continuous Palettes** — Turbo, Viridis, Magma, Plasma, Inferno, Cividis, Spectral, Coolwarm.
- **Invert Scale** — Reverse color gradient direction.
- **LANDFIRE Fuel Models** — Interactive table for 40 FBFM40 classes with color swatches and visibility toggles.

### Metadata (`ℹ️ Archive Header & Tile Stats`)

- PMTiles v3 header: min/max zoom, tile compression, layer names, field types.
- Bounding box coordinates.
- Multi-resolution histogram summaries.

### Basemap (`🗺️ Cartography`)

- Switch between dark, light, OpenStreetMap, and satellite backgrounds.

---

## Directory Structure

```
pmtiles_viewer/
├── index.html        # Single-page MapLibre GL JS + PMTiles Studio
├── server.py         # Python HTTP server with Range and CORS headers
├── README.md         # This file
└── debug/            # Headless test scripts
    ├── README.md     # Debug script documentation
    ├── test.js       # Puppeteer browser validation
    ├── test_mvt.js   # Raw MVT protobuf decoder
    └── package.json  # NPM dependencies (puppeteer, @mapbox/vector-tile, pbf)
```
