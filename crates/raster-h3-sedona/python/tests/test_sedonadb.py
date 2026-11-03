"""raster_h3_sedona.RasterH3Table scanned by SedonaDB through its table provider capsule."""

import re
import struct

import pytest

sedonadb = pytest.importorskip("sedonadb")

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
    return sedonadb.connect()


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


def test_categorical(sd, raster):
    sd.create_data_frame(RasterH3Table.categorical(raster, 8)).to_view("classes")
    assert scalar(sd, "SELECT SUM(total_count) FROM classes") == 200 * 200
    assert scalar(sd, "SELECT MIN(majority_fraction) FROM classes") > 0


def test_invalid_arguments():
    with pytest.raises(ValueError, match="between 0 and 15"):
        RasterH3Table.continuous("x.tif", 16)
    with pytest.raises(ValueError):
        RasterH3Table.continuous("/nonexistent/raster.tif", 8)
