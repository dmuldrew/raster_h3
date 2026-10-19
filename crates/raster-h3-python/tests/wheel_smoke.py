"""Smoke test for an installed `raster-h3` wheel (run by cibuildwheel after each build).

Checks that the bundled native library loads from the installed package, that the
Arrow/Polars paths work, and that the `raster-h3-cli` executable is on PATH.
Needs polars and pyarrow; writes its own tiny GeoTIFF so it has no file dependencies.
"""

import json
import os
import shutil
import struct
import subprocess
import tempfile
from pathlib import Path

import polars as pl
import pyarrow as pa

import raster_h3


def write_geotiff(path: Path, width: int, height: int) -> None:
    """Uncompressed little-endian float32 GeoTIFF (EPSG:4326 GeoKeys) near San Francisco."""
    values = [float(c + 2 * r) for r in range(height) for c in range(width)]
    data = struct.pack(f"<{len(values)}f", *values)
    scale = struct.pack("<3d", 0.0005, 0.0005, 0.0)
    tiepoint = struct.pack("<6d", 0.0, 0.0, 0.0, -122.45, 37.80, 0.0)
    geokeys = struct.pack("<12H", 1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326)
    blobs = [scale, tiepoint, geokeys]
    # (tag, type, count, value-or-blob-index); SHORT=3, LONG=4, DOUBLE=12
    entries = [
        (256, 3, 1, width), (257, 3, 1, height), (258, 3, 1, 32), (259, 3, 1, 1),
        (262, 3, 1, 1), (273, 4, 1, "data"), (277, 3, 1, 1), (278, 3, 1, height),
        (279, 4, 1, len(data)), (339, 3, 1, 3),
        (33550, 12, 3, 0), (33922, 12, 6, 1), (34735, 3, 12, 2),
    ]
    offset = 8 + 2 + 12 * len(entries) + 4
    blob_offsets = []
    for blob in blobs:
        blob_offsets.append(offset)
        offset += len(blob)
    data_offset = offset

    out = bytearray(b"II*\x00" + struct.pack("<I", 8) + struct.pack("<H", len(entries)))
    for tag, typ, count, value in entries:
        if value == "data":
            value = data_offset
        elif tag in (33550, 33922, 34735):
            value = blob_offsets[value]
        if typ == 3 and count == 1:
            out += struct.pack("<HHIHH", tag, typ, count, value, 0)
        else:
            out += struct.pack("<HHII", tag, typ, count, value)
    out += struct.pack("<I", 0)
    for blob in blobs:
        out += blob
    out += data
    path.write_bytes(bytes(out))


def main() -> None:
    lib = Path(raster_h3._find_native_library()).resolve()
    pkg = Path(raster_h3.__file__).resolve().parent
    assert lib.parent == pkg, f"native library loaded from {lib}, not the installed package"
    print("native library:", lib.name)

    # Scalar functions go through the C ABI.
    cell = 0x8828308281FFFFF
    assert raster_h3.h3_to_string(cell) == "8828308281fffff"
    assert raster_h3.h3_get_resolution(cell) == 8
    assert raster_h3.h3_is_valid("8828308281fffff")
    # Odd-resolution pentagon with 10 boundary vertices: 189 bytes of WKB.
    wkb = raster_h3.h3_to_wkb(0x81083FFFFFFFFFF)
    assert wkb is not None and len(wkb) == 13 + 16 * 11, wkb

    with tempfile.TemporaryDirectory() as tmp:
        tif = Path(tmp) / "smoke.tif"
        write_geotiff(tif, 32, 32)

        stream = raster_h3.RasterH3ArrowStream(tif, 8)
        df = stream.to_polars()
        assert df.height > 0 and df["count"].sum() == 32 * 32, df
        table = pa.table(stream)
        assert table.num_rows == df.height
        batches = list(stream.iter_batches(batch_size=4))
        assert sum(b.height for b in batches) == df.height

        lazy = raster_h3.scan_raster_h3(tif, 8)
        assert lazy.filter(pl.col("mean") > 0).select("h3_index").collect().height > 0
        assert lazy.head(2).collect().height == 2
        print("python: rows", df.height)

        # The CLI ships in the same wheel and must be on PATH.
        cli = shutil.which("raster-h3-cli")
        assert cli, f"raster-h3-cli not on PATH ({os.environ.get('PATH')})"
        out = subprocess.run(
            [cli, "jsonl", str(tif), "-r", "8", "--columns", "h3_index,count"],
            check=True, capture_output=True, text=True,
        ).stdout
        rows = [json.loads(line) for line in out.splitlines()]
        assert len(rows) == df.height and sum(r["count"] for r in rows) == 32 * 32
        bad = subprocess.run([cli, "jsonl", str(tif), "--bbox", "1,2,3"], capture_output=True)
        assert bad.returncode != 0
        print("cli: rows", len(rows))

    print("WHEEL SMOKE TEST PASSED")


if __name__ == "__main__":
    main()
