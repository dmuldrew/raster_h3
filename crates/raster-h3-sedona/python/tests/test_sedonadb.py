"""raster_h3_sedona.RasterH3Table scanned by SedonaDB through its table provider capsule."""

import re
import struct

import pytest

sedonadb = pytest.importorskip("sedonadb")

import raster_h3_sedona  # noqa: E402
from raster_h3_sedona import RasterH3Table  # noqa: E402


def write_gradient_tif(path, width, height):
    """A 0.001 degree EPSG:4326 float32 GeoTIFF over San Francisco; values increase left to right.

    Matches `gradient_tif` in the Rust tests: origin (-122.45, 37.80).
    """
    pixels = b"".join(
        struct.pack("<f", float(x)) for _ in range(height) for x in range(width)
    )
    scale = struct.pack("<3d", 0.001, 0.001, 0.0)
    tiepoint = struct.pack("<6d", 0.0, 0.0, 0.0, -122.45, 37.80, 0.0)
    # GTModelType = geographic, GeographicType = EPSG:4326.
    geokeys = struct.pack("<12H", 1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326)

    # (tag, type, count, payload): type 3 = SHORT, 4 = LONG, 12 = DOUBLE.
    entries = [
        (256, 4, 1, struct.pack("<I", width)),
        (257, 4, 1, struct.pack("<I", height)),
        (258, 3, 1, struct.pack("<H", 32)),
        (259, 3, 1, struct.pack("<H", 1)),
        (262, 3, 1, struct.pack("<H", 1)),
        (273, 4, 1, None),  # StripOffsets, patched below
        (277, 3, 1, struct.pack("<H", 1)),
        (278, 4, 1, struct.pack("<I", height)),
        (279, 4, 1, struct.pack("<I", len(pixels))),
        (339, 3, 1, struct.pack("<H", 3)),  # SampleFormat = IEEE float
        (33550, 12, 3, scale),
        (33922, 12, 6, tiepoint),
        (34735, 3, 12, geokeys),
    ]
    ifd_offset = 8
    ifd_size = 2 + 12 * len(entries) + 4
    extra_offset = ifd_offset + ifd_size
    extra = b""
    payloads = []
    for tag, typ, count, payload in entries:
        if payload is not None and len(payload) > 4:
            payloads.append(struct.pack("<I", extra_offset + len(extra)))
            extra += payload
        else:
            payloads.append(payload)
    pixel_offset = extra_offset + len(extra)

    ifd = struct.pack("<H", len(entries))
    for (tag, typ, count, _), payload in zip(entries, payloads):
        if payload is None:
            payload = struct.pack("<I", pixel_offset)
        ifd += struct.pack("<HHI", tag, typ, count) + payload.ljust(4, b"\0")
    ifd += struct.pack("<I", 0)

    with open(path, "wb") as f:
        f.write(b"II*\0" + struct.pack("<I", ifd_offset) + ifd + extra + pixels)


@pytest.fixture
def raster(tmp_path):
    path = tmp_path / "gradient.tif"
    # 0.2 degree square: lon -122.45..-122.25, lat 37.60..37.80.
    write_gradient_tif(path, 200, 200)
    return str(path)


@pytest.fixture
def sd():
    sd = sedonadb.connect()
    sd.register(raster_h3_sedona.scalar_functions())
    return sd


def scalar(sd, sql):
    return sd.sql(sql).to_pandas().iloc[0, 0]


def rows(sd, sql):
    return [tuple(r) for r in sd.sql(sql).to_pandas().itertuples(index=False)]


def test_scan_project_limit_and_count(sd, raster):
    table = RasterH3Table.continuous(raster, 8)
    assert "continuous" in repr(table)
    sd.create_data_frame(table).to_view("cells")

    n = scalar(sd, "SELECT COUNT(*) FROM cells")
    assert n > 100
    assert len(rows(sd, "SELECT h3_index, mean FROM cells LIMIT 3")) == 3
    # Each pixel is one sample.
    assert scalar(sd, "SELECT SUM(count) FROM cells") == 200 * 200
    # Pushed-down thresholds agree with post-filtering.
    expected = scalar(sd, "SELECT COUNT(*) FROM cells WHERE count + 0 >= 40")
    assert 0 < expected < n
    assert scalar(sd, "SELECT COUNT(*) FROM cells WHERE count >= 40") == expected


def test_cells_are_sedona_geometry(sd, raster):
    sd.create_data_frame(RasterH3Table.continuous(raster, [7, 8])).to_view("cells")
    kinds = rows(
        sd, "SELECT DISTINCT ST_GeometryType(wkb), ST_SRID(wkb) FROM cells"
    )
    assert kinds == [("ST_Polygon", 4326)]
    assert scalar(sd, "SELECT COUNT(DISTINCT resolution) FROM cells") == 2


POLYGON = (
    "ST_SetSRID(ST_GeomFromWKT('POLYGON ((-122.40 37.72, -122.38 37.72, "
    "-122.38 37.74, -122.40 37.74, -122.40 37.72))'), 4326)"
)
POINT = "ST_SetSRID(ST_GeomFromWKT('POINT (-122.35 37.70)'), 4326)"


@pytest.mark.parametrize(
    "predicate",
    [
        f"ST_Intersects(wkb, {POLYGON})",
        f"ST_Within(wkb, {POLYGON})",
        f"ST_DWithin(wkb, {POINT}, 0.01)",
        f"ST_Intersects({POLYGON}, h3_to_wkb(h3_index))",
        "ST_Intersects(wkb, h3_to_wkb(613196570287931391))",
    ],
)
def test_spatial_filters_match_an_unpruned_scan(sd, raster, predicate):
    sd.create_data_frame(RasterH3Table.continuous(raster, 8)).to_view("cells")
    # A materialised copy cannot push anything down: the reference answer.
    sd.sql("SELECT * FROM cells").to_memtable().to_view("full_scan")

    query = "SELECT h3_index, mean, count FROM {} WHERE {} ORDER BY h3_index"
    expected = rows(sd, query.format("full_scan", predicate))
    assert expected
    assert rows(sd, query.format("cells", predicate)) == expected


def test_spatial_filter_off_the_raster_is_empty(sd, raster):
    sd.create_data_frame(RasterH3Table.continuous(raster, 8)).to_view("cells")
    far = "ST_SetSRID(ST_GeomFromWKT('POINT (-100 40)'), 4326)"
    assert scalar(sd, f"SELECT COUNT(*) FROM cells WHERE ST_Intersects(wkb, {far})") == 0


def test_spatial_filter_prunes_the_scan(sd, raster):
    """The predicate reaches the scan across SedonaDB's extension ABI.

    `datafusion-proto` strips the literal's `geoarrow.wkb` tag on the way, so this checks
    that the table still accepts the untagged geometry and pushes its box down.
    """
    sd.create_data_frame(RasterH3Table.continuous(raster, 8)).to_view("cells")
    total = scalar(sd, "SELECT COUNT(*) FROM cells")
    plan = "\n".join(
        sd.sql(
            f"EXPLAIN ANALYZE SELECT h3_index FROM cells WHERE ST_Intersects(wkb, {POINT})"
        )
        .to_pandas()
        .iloc[:, -1]
    )
    # The FilterExec above the scan reports `selectivity=...(matched/input)`.
    match = re.search(r"FilterExec: st_intersects.*?\((\d+)/(\d+)\)", plan)
    assert match, plan
    matched, scanned = map(int, match.groups())
    assert matched == 1
    assert scanned * 5 < total, plan


def test_scalar_functions_register():
    functions = raster_h3_sedona.scalar_functions()
    names = functions.names
    assert len(functions) == len(names) == len(set(names))
    for name in ["h3_polygon_to_cells", "h3_to_wkb", "h3_to_geometry", "h3_cell_to_parent"]:
        assert name in names
    assert [f.name for f in functions] == names


def test_scalar_functions(sd, raster):
    sd.create_data_frame(RasterH3Table.continuous(raster, 8)).to_view("cells")
    assert scalar(sd, "SELECT h3_to_string(599686042433355775)") == "85283473fffffff"
    mismatches = scalar(
        sd,
        """SELECT COUNT(*) FROM cells
           WHERE h3_to_string(h3_index) <> h3_hex
              OR h3_get_resolution(h3_index) <> 8
              OR h3_get_resolution(h3_cell_to_parent(h3_index, 6)) <> 6
              OR NOT ST_Equals(h3_to_wkb(h3_index), wkb)
              OR NOT ST_Equals(h3_cell_to_geometry(h3_hex), wkb)""",
    )
    assert mismatches == 0
    assert rows(sd, "SELECT ST_SRID(h3_to_wkb(599686042433355775))") == [(4326,)]


def test_polygon_to_cells_joins_on_h3_index(sd, raster):
    sd.create_data_frame(RasterH3Table.continuous(raster, 8)).to_view("cells")
    sd.sql(f"SELECT 'aoi' AS name, {POLYGON} AS geom").to_view("areas")
    joined = rows(
        sd,
        """WITH aoi AS (
               SELECT name, unnest(h3_polygon_to_cells(geom, 8, 'covers')) AS h3_index
               FROM areas
           )
           SELECT c.h3_index FROM aoi JOIN cells AS c ON c.h3_index = aoi.h3_index
           ORDER BY c.h3_index""",
    )
    assert len(joined) > 1
    # Every polyfilled cell overlaps the polygon, and every cell whose centre lies inside it
    # is polyfilled.
    covered = {h for (h,) in joined}
    overlapping = {
        h for (h,) in rows(sd, f"SELECT h3_index FROM cells WHERE ST_Intersects(wkb, {POLYGON})")
    }
    centred = {
        h
        for (h,) in rows(
            sd, f"SELECT h3_index FROM cells WHERE ST_Intersects(ST_Centroid(wkb), {POLYGON})"
        )
    }
    assert centred <= covered <= overlapping
    # The default containment keeps cells whose centre is inside.
    default = {
        h
        for (h,) in rows(
            sd, "SELECT unnest(h3_polygon_to_cells(geom, 8)) FROM areas"
        )
    }
    assert default == centred


def test_polygon_to_cells_rejects_projected_geometry(sd):
    with pytest.raises(Exception):
        sd.sql(
            "SELECT h3_polygon_to_cells(ST_SetSRID(ST_GeomFromWKT("
            "'POLYGON ((0 0, 1 0, 1 1, 0 0))'), 3857), 8)"
        ).to_pandas()


def test_categorical(sd, raster):
    sd.create_data_frame(RasterH3Table.categorical(raster, 8)).to_view("classes")
    assert scalar(sd, "SELECT SUM(total_count) FROM classes") == 200 * 200
    assert scalar(sd, "SELECT MIN(majority_fraction) FROM classes") > 0


def test_invalid_arguments():
    with pytest.raises(ValueError, match="between 0 and 15"):
        RasterH3Table.continuous("x.tif", 16)
    with pytest.raises(ValueError):
        RasterH3Table.continuous("/nonexistent/raster.tif", 8)


# -----------------------------------------------------------------------------
# write_pmtiles
# -----------------------------------------------------------------------------


def pmtiles_metadata(path):
    """The archive's JSON metadata (gzipped after the root directory)."""
    import gzip
    import json

    data = path.read_bytes()
    assert data[:7] == b"PMTiles" and data[7] == 3
    offset, length = struct.unpack_from("<QQ", data, 24)
    return json.loads(gzip.decompress(data[offset : offset + length]))


def test_write_pmtiles_from_a_sedonadb_query(sd, raster, tmp_path):
    sd.create_data_frame(RasterH3Table.continuous(raster, [7, 8])).to_view("cells")
    query = "SELECT h3_index, mean, count, resolution, wkb FROM cells WHERE count >= 4"
    expected = scalar(sd, f"SELECT COUNT(*) FROM ({query})")
    out = tmp_path / "cells.pmtiles"
    assert raster_h3_sedona.write_pmtiles(sd.sql(query), out) == expected > 0

    meta = pmtiles_metadata(out)
    fields = meta["vector_layers"][0]["fields"]
    assert fields["mean"] == "Number" and fields["count"] == "Number"
    # The geometry column is not a property: each feature already is the cell polygon.
    assert "wkb" not in fields

    # Only resolution 8's zoom.
    z8 = meta["maxzoom"]
    only8 = tmp_path / "res8.pmtiles"
    written = raster_h3_sedona.write_pmtiles(sd.sql(query), str(only8), min_zoom=z8, memory_budget_mb=1)
    assert written == scalar(sd, f"SELECT COUNT(*) FROM ({query}) WHERE resolution = 8")


def test_write_pmtiles_from_arrow_and_hex_columns(tmp_path):
    pa = pytest.importorskip("pyarrow")
    table = pa.table(
        {
            "name": ["a", "b", "c"],
            "cell_id": ["8828308281fffff", None, "8828308283fffff"],
            "value": pa.array([1.5, 2.5, None]),
        }
    )
    out = tmp_path / "hex.pmtiles"
    assert raster_h3_sedona.write_pmtiles(table, out, h3_column="cell_id") == 2
    fields = pmtiles_metadata(out)["vector_layers"][0]["fields"]
    assert fields["name"] == "String" and fields["value"] == "Number"

    signed = pa.table({"h3_index": pa.array([613196570331971583], pa.int64())})
    assert raster_h3_sedona.write_pmtiles(signed, tmp_path / "signed.pmtiles") == 1


def test_write_pmtiles_errors(tmp_path):
    pa = pytest.importorskip("pyarrow")
    out = tmp_path / "x.pmtiles"
    with pytest.raises(TypeError, match="__arrow_c_stream__"):
        raster_h3_sedona.write_pmtiles([1, 2, 3], out)
    with pytest.raises(ValueError, match="42 is not a valid H3 cell"):
        raster_h3_sedona.write_pmtiles(pa.table({"h3_index": pa.array([42], pa.uint64())}), out)
    with pytest.raises(ValueError, match="h3_column 'nope' not found"):
        raster_h3_sedona.write_pmtiles(pa.table({"h3_index": pa.array([42], pa.uint64())}), out, h3_column="nope")
    with pytest.raises(ValueError, match="has type Float64"):
        raster_h3_sedona.write_pmtiles(pa.table({"mean": [1.0]}), out)
    with pytest.raises(ValueError, match="must not be greater than max_zoom"):
        raster_h3_sedona.write_pmtiles(pa.table({"h3_index": pa.array([42], pa.uint64())}), out, min_zoom=9, max_zoom=3)
    with pytest.raises(ValueError, match="memory_budget_mb"):
        raster_h3_sedona.write_pmtiles(pa.table({"h3_index": pa.array([42], pa.uint64())}), out, memory_budget_mb=0)
    assert not out.exists(), "a failed write must not leave an archive"
