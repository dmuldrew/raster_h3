#!/usr/bin/env python3
"""Regenerate the NTv2 golden values using the installed PROJ C library.

Requires libproj. This tool is not used by the Rust tests. It writes only a
synthetic grid in a temporary directory and prints forward/inverse coordinates.
"""

import ctypes
import ctypes.util
import pathlib
import struct
import tempfile


class Coord(ctypes.Union):
    _fields_ = [("v", ctypes.c_double * 4)]


def grid_bytes():
    data = bytearray(176 * 2 + 16 * 4 + 16)

    def record(offset, key, value):
        data[offset : offset + 8] = key.encode("ascii").ljust(8)
        data[offset + 8 : offset + 16] = value

    def integer(value):
        return struct.pack("<i", value) + bytes(4)

    def string(value):
        return value.encode("ascii").ljust(8)

    def double(value):
        return struct.pack("<d", value)

    overview = [
        ("NUM_OREC", integer(11)),
        ("NUM_SREC", integer(11)),
        ("NUM_FILE", integer(1)),
        ("GS_TYPE", string("SECONDS")),
        ("VERSION", string("TEST")),
        ("SYSTEM_F", string("SRC")),
        ("SYSTEM_T", string("DST")),
        ("MAJOR_F", double(6378137)),
        ("MINOR_F", double(6356752)),
        ("MAJOR_T", double(6378137)),
        ("MINOR_T", double(6356752)),
    ]
    subgrid = [
        ("SUB_NAME", string("ROOT")),
        ("PARENT", string("NONE")),
        ("CREATED", string("20261112")),
        ("UPDATED", string("20261112")),
        ("S_LAT", double(0)),
        ("N_LAT", double(3600)),
        ("E_LONG", double(0)),
        ("W_LONG", double(3600)),
        ("LAT_INC", double(3600)),
        ("LONG_INC", double(3600)),
        ("GS_COUNT", integer(4)),
    ]
    for base, entries in [(0, overview), (176, subgrid)]:
        for i, (key, value) in enumerate(entries):
            record(base + i * 16, key, value)
    for i in range(4):
        struct.pack_into("<ffff", data, 352 + i * 16, i + 0.5, -float(i), 0, 0)
    record(416, "END", bytes(8))
    return data


def main():
    library = ctypes.util.find_library("proj")
    if not library:
        raise RuntimeError("libproj not found")
    proj = ctypes.CDLL(library)
    proj.proj_context_create.restype = ctypes.c_void_p
    proj.proj_context_destroy.argtypes = [ctypes.c_void_p]
    proj.proj_create.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    proj.proj_create.restype = ctypes.c_void_p
    proj.proj_destroy.argtypes = [ctypes.c_void_p]
    proj.proj_trans.argtypes = [ctypes.c_void_p, ctypes.c_int, Coord]
    proj.proj_trans.restype = Coord
    with tempfile.TemporaryDirectory() as directory:
        path = pathlib.Path(directory) / "grid.gsb"
        path.write_bytes(grid_bytes())
        pipeline = (
            "+proj=pipeline +step +proj=unitconvert +xy_in=deg +xy_out=rad "
            f"+step +proj=hgridshift +grids={path} "
            "+step +proj=unitconvert +xy_in=rad +xy_out=deg"
        )
        context = proj.proj_context_create()
        operation = proj.proj_create(context, pipeline.encode())
        if not operation:
            raise RuntimeError("PROJ could not create the grid-shift operation")
        try:
            for lon, lat in [(-0.5, 0.5), (-0.25, 0.75), (-0.75, 0.25)]:
                point = Coord()
                point.v[:] = [lon, lat, 0, 0]
                forward = proj.proj_trans(operation, 1, point)
                inverse = proj.proj_trans(operation, -1, forward)
                print(lon, lat, forward.v[0], forward.v[1], inverse.v[0], inverse.v[1])
        finally:
            proj.proj_destroy(operation)
            proj.proj_context_destroy(context)


if __name__ == "__main__":
    main()
