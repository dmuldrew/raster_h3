"""Hatch build hook: compile the native pieces of the `raster-h3` wheel with Cargo.

- `raster-h3-python` (cdylib) is placed inside the `raster_h3` package, where
  `raster_h3._find_native_library` loads it with ctypes.
- `raster-h3-cli` (executable) goes into the wheel's `scripts` directory, so
  installers put it on PATH.

The wheel is tagged `py3-none-<platform>`: the library never links against libpython.
Cross-compilation follows `CARGO_BUILD_TARGET`, or on macOS a single-arch `ARCHFLAGS`
(as set by cibuildwheel). Linux wheels are tagged `linux_*` here and retagged to
`manylinux_*` by auditwheel.
"""

from __future__ import annotations

import os
import platform
import shlex
import subprocess
import sys
import sysconfig
from pathlib import Path

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

_MACOS_ARCH_TO_TARGET = {
    "x86_64": "x86_64-apple-darwin",
    "arm64": "aarch64-apple-darwin",
}
# Rust's own minimum deployment targets, used when MACOSX_DEPLOYMENT_TARGET is unset.
_MACOS_DEFAULT_MIN = {"x86_64": "10.12", "arm64": "11.0"}


def _macos_arch() -> str:
    target = os.environ.get("CARGO_BUILD_TARGET", "")
    if target.startswith("x86_64-apple"):
        return "x86_64"
    if target.startswith("aarch64-apple"):
        return "arm64"
    flags = shlex.split(os.environ.get("ARCHFLAGS", ""))
    archs = [flags[i + 1] for i, flag in enumerate(flags[:-1]) if flag == "-arch"]
    if len(set(archs)) > 1:
        raise RuntimeError(f"raster-h3 wheels are single-arch; got ARCHFLAGS={archs}")
    return archs[0] if archs else platform.machine()


def _cargo_target() -> str | None:
    if os.environ.get("CARGO_BUILD_TARGET"):
        return os.environ["CARGO_BUILD_TARGET"]
    if sys.platform == "darwin":
        arch = _macos_arch()
        if arch != platform.machine():
            return _MACOS_ARCH_TO_TARGET[arch]
    return None


def _platform_tag() -> str:
    if sys.platform == "darwin":
        arch = _macos_arch()
        minimum = os.environ.get("MACOSX_DEPLOYMENT_TARGET") or _MACOS_DEFAULT_MIN[arch]
        major, _, minor = minimum.partition(".")
        # Since macOS 11 the minor version is not part of the tag.
        minor = "0" if int(major) >= 11 else (minor or "0")
        return f"macosx_{major}_{minor}_{arch}"
    plat = os.environ.get("_PYTHON_HOST_PLATFORM") or sysconfig.get_platform()
    return plat.replace("-", "_").replace(".", "_")


class RustBuildHook(BuildHookInterface):
    PLUGIN_NAME = "custom"

    def initialize(self, version: str, build_data: dict) -> None:
        if self.target_name != "wheel":
            return

        workspace = Path(
            os.environ.get("RASTER_H3_WORKSPACE") or Path(self.root).resolve().parent.parent
        )
        if not (workspace / "Cargo.toml").is_file():
            raise RuntimeError(
                f"Cargo workspace not found at {workspace}. Build from a repository checkout "
                "with an in-tree builder (pip>=21.3, python -m build) or set RASTER_H3_WORKSPACE."
            )
        target = _cargo_target()
        target_dir = Path(os.environ.get("CARGO_TARGET_DIR", workspace / "target"))

        cmd = [
            "cargo", "build", "--release", "--locked",
            "--manifest-path", str(workspace / "Cargo.toml"),
            "-p", "raster-h3-python", "-p", "raster-h3-cli",
        ]
        if target:
            cmd += ["--target", target]
        env = dict(os.environ)
        env.setdefault("CARGO_PROFILE_RELEASE_STRIP", "symbols")
        self.app.display_info(f"Running {' '.join(cmd)}")
        subprocess.run(cmd, check=True, env=env)

        out = target_dir / target / "release" if target else target_dir / "release"
        if sys.platform == "win32":
            lib, exe = "raster_h3_python.dll", "raster-h3-cli.exe"
        elif sys.platform == "darwin":
            lib, exe = "libraster_h3_python.dylib", "raster-h3-cli"
        else:
            lib, exe = "libraster_h3_python.so", "raster-h3-cli"

        build_data["force_include"][str(out / lib)] = f"raster_h3/{lib}"
        build_data["shared_scripts"][str(out / exe)] = exe
        build_data["pure_python"] = False
        build_data["tag"] = f"py3-none-{_platform_tag()}"
