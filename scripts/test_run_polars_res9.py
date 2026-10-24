import contextlib
import io
from pathlib import Path
import runpy
import sys
import types
import unittest
from unittest.mock import Mock, patch


class ArrowStream:
    pass


class BenchmarkTest(unittest.TestCase):
    def run_benchmark(self, scan, args=("input.tif", "output.parquet")):
        module = types.SimpleNamespace(
            RasterH3ArrowStream=ArrowStream,
            scan_raster_h3=Mock(return_value=scan),
            raster_to_parquet=Mock(),
        )
        output = io.StringIO()
        with (
            patch.dict(sys.modules, raster_h3=module),
            patch.object(sys, "argv", ["run_polars_res9.py", *args]),
            patch("time.perf_counter", side_effect=[10.0, 12.5]),
            patch("time.time", side_effect=AssertionError("Use a monotonic timer")),
            contextlib.redirect_stdout(output),
        ):
            runpy.run_path(str(Path(__file__).with_name("run_polars_res9.py")), run_name="__main__")
        return module, output.getvalue()

    def test_lazy_frame_sink(self):
        frame = Mock()
        module, output = self.run_benchmark(frame)
        frame.sink_parquet.assert_called_once_with("output.parquet")
        module.raster_to_parquet.assert_not_called()
        module.scan_raster_h3.assert_called_once_with(
            "input.tif", resolutions=9, columns=["h3_index", "min", "max", "mean"]
        )
        self.assertIn("via Polars IO plugin in 2.50s", output)

    def test_arrow_stream_uses_direct_exporter(self):
        module, output = self.run_benchmark(ArrowStream())
        module.raster_to_parquet.assert_called_once_with(
            "input.tif", "output.parquet", resolutions=9, geoparquet=False
        )
        self.assertIn("Polars IO plugin unavailable; full statistics schema", output)
        self.assertIn("2.50s", output)

    def test_scan_errors_propagate(self):
        frame = Mock()
        frame.sink_parquet.side_effect = RuntimeError("scan failed")
        with self.assertRaisesRegex(RuntimeError, "scan failed"):
            self.run_benchmark(frame)

    def test_missing_arguments(self):
        with self.assertRaises(SystemExit):
            self.run_benchmark(Mock(), args=())


if __name__ == "__main__":
    unittest.main()
