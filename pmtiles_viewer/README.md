# PMTiles Hexagon Studio (`pmtiles_viewer`)

A lightweight, GPU-accelerated web studio built on [MapLibre GL JS](https://maplibre.org/) and the [PMTiles v3 specification](https://github.com/protomaps/PMTiles) for high-performance visual inspection of multi-resolution H3 hexagonal vector tile pyramids.

---

## 🌟 Highlights

- **Zero External Backend Services**: Runs completely client-side in your web browser with HTTP byte-range queries (`pmtiles.FetchSource`) or 100% offline via local drag-and-drop (`pmtiles.FileSource`).
- **Continuous & Categorical Auto-Detection**: Dynamically switches styling rules between continuous surfaces (elevation, temperature, NDVI, crown fire) and discrete classifications (land cover, zoning, LANDFIRE fire behavior models).
- **3D Volumetric Hexagon Extrusion**: Extrude hexagons in 3D WebGL space with variable pitch, bearing, and property-based height scaling.
- **Resolution-Adaptive Dynamic Range Calibration**: Automatically calculates and adapts color ramp min/max scales across pyramid zoom transitions to preserve visual contrast.
- **Built-in Cartographic Colormaps**: Includes Turbo, Viridis, Magma, Plasma, Inferno, Cividis, Spectral, Coolwarm, Thermal, and categorical color tables.
- **LANDFIRE FBFM40 Fuel Models Built-in**: Complete 40-class fire behavior palette with editable class labels and live color pickers.
- **Real-Time HUD & Interactive Tooltip**: Live display of viewport center coordinates, zoom level, active H3 pyramid resolution, and exact cell attribute readouts on hover.

---

## 🚀 Quickstart & Launch Methods

### Method 1: Docker Compose (Recommended)
The repository includes a ready-to-use Compose configuration:

```bash
# Start viewer on port 8080
docker compose up viewer

# Run in background (detached)
docker compose up -d viewer
```
Navigate to **`http://localhost:8080/pmtiles_viewer/`** (or **`http://localhost:8080/`**).

---

### Method 2: Standalone Docker Run
Run directly with Docker without needing Docker Compose or a local Python install:

```bash
docker run --rm -p 8080:8080 -v $(pwd):/app python:3.11-slim python3 -u /app/pmtiles_viewer/server.py 8080
```

---

### Method 3: Local Python Streaming Server
If Python 3 is installed on your host system:

```bash
# Launch HTTP byte-range server from repo root
python3 pmtiles_viewer/server.py 8080
```
Open **`http://localhost:8080/pmtiles_viewer/`**.

---

### Method 4: Offline File Drag-and-Drop (Zero Web Server)
If you do not want to run an HTTP server:
1. Open `pmtiles_viewer/index.html` directly in your browser (`file:///.../pmtiles_viewer/index.html`).
2. Chrome blocks local network `fetch()` requests on `file://` URLs.
3. Click the sidebar dropzone (**📁 Click to select file from disk**) or drag-and-drop any `.pmtiles` file from your desktop directly into the browser window.
4. The file will load immediately using the browser's native `FileReader` API.

---

## 📊 Supported Data Schemas

### 1. Continuous PMTiles Pyramids
Generated via `h3_raster_to_pmtiles(...)` or `raster_to_pmtiles CLI`.

| Layer Attribute | Type | Description |
| :--- | :--- | :--- |
| `mean` | Float | Average pixel value in the H3 cell |
| `min` | Float | Minimum pixel value in the H3 cell |
| `max` | Float | Peak pixel value in the H3 cell |
| `sum` | Float | Total sum of pixel values |
| `count` | Float | Number of valid (non-NoData) samples |
| `stddev` | Float | Standard deviation of cell values |
| `resolution` | Integer | H3 grid resolution (e.g. 6 to 10) |

### 2. Categorical PMTiles Pyramids
Generated from classification rasters (e.g. LANDFIRE, ESA WorldCover, NLCD).

| Layer Attribute | Type | Description |
| :--- | :--- | :--- |
| `majority_class` | Integer | Dominant raster category class ID |
| `majority_fraction` | Float | Dominance fraction ($0.0 \text{ to } 1.0$) |
| `purity` | Float | Class purity ratio |
| `total_count` | Float | Total valid pixel count in cell |
| `distinct_classes` | Integer | Number of unique classes in cell |

---

## 🎛️ Studio Interface & Controls

### 1. Layers Tab (`📂 Data Source & Metric`)
* **Active PMTiles Archive**: Select preloaded demo archives or enter a custom path (e.g. `/data/my_output.pmtiles`).
* **Fit Extent (`🎯 Fit Extent`)**: Animates the map camera directly to the dataset bounding box.
* **Colorize Attribute**: Select which numerical or categorical attribute determines fill colors.
* **Range & Calibration**:
  * **Adaptive Scale**: Adjusts color thresholds automatically as you zoom across H3 resolutions.
  * **Visible Extent Calibration (`👁️ Visible`)**: Samples currently rendered viewport tiles to maximize local contrast.
  * **Extrusion Height (`🏢 3D Extrusion`)**: Toggles 3D volumetric polygons scaled by values or certainty.
  * **Opacity & Wireframe**: Control polygon opacity and hexagon cell outline visibility.

### 2. Colormaps Tab (`🎨 Palettes & Categorical Styling`)
* **Continuous Palettes**: Turbo, Viridis, Magma, Plasma, Inferno, Cividis, Spectral, Coolwarm.
* **Invert Scale**: Invert color gradient progression.
* **LANDFIRE Fuel Models Table**: Interactive table for 40 fire behavior classes (FBFM40) with color swatches and visibility checkboxes.

### 3. Metadata Tab (`ℹ️ Archive Header & Tile Stats`)
* Dumps PMTiles v3 header information:
  * Minimum / Maximum zoom levels
  * Tile compression (`gzip`)
  * Vector tile layer names and field types
  * Direct ground-truth bounding box ($[\text{min\_lon}, \text{min\_lat}, \text{max\_lon}, \text{max\_lat}]$)
  * Multi-resolution histogram summaries

### 4. Basemap Tab (`🗺️ Cartography`)
* Switch between dark, light, OpenStreetMap, and satellite background tiles.

---

## 📁 Directory Structure

```
pmtiles_viewer/
├── index.html        # Single-page MapLibre GL JS + PMTiles Studio application
├── server.py         # Lightweight Python HTTP server with Range and CORS headers
└── debug/            # Headless Puppeteer and MVT protobuf verification tests
    ├── README.md
    ├── test.js       # Headless browser validation script
    ├── test_mvt.js   # Raw vector tile protobuf decoder
    └── package.json  # NPM test dependencies
```
