"""Tests for scripts/ui_gallery.py (the manual TUI gallery helper)."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import ui_gallery


class WindowSizeTests(unittest.TestCase):
    def write(self, directory: Path, name: str, text: str) -> Path:
        path = directory / name
        path.write_text(text, encoding="utf-8")
        return path

    def test_size_comes_from_the_frame_heading(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            html = self.write(Path(raw), "gallery-overlay-palette.html", "<h2 id=\"x\">overlay palette — 160×45</h2>")
            width, height = ui_gallery.window_size(html)
        self.assertEqual(width, int(160 * ui_gallery.CELL_WIDTH_PX) + ui_gallery.PAGE_PADDING_X)
        self.assertEqual(height, 45 * ui_gallery.CELL_HEIGHT_PX + ui_gallery.PAGE_PADDING_Y)

    def test_size_falls_back_to_the_file_name_then_a_default(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            named = self.write(Path(raw), "gallery-workbench-80x24.html", "<p>no heading</p>")
            plain = self.write(Path(raw), "gallery-overlay-help.html", "<p>no heading</p>")
            named_size = ui_gallery.window_size(named)
            plain_size = ui_gallery.window_size(plain)
        self.assertEqual(named_size[1], 24 * ui_gallery.CELL_HEIGHT_PX + ui_gallery.PAGE_PADDING_Y)
        self.assertEqual(plain_size[1], 45 * ui_gallery.CELL_HEIGHT_PX + ui_gallery.PAGE_PADDING_Y)


class BrowserDiscoveryTests(unittest.TestCase):
    def test_no_browser_returns_none(self) -> None:
        with mock.patch.object(ui_gallery, "BROWSER_CANDIDATES", ()), mock.patch.object(
            ui_gallery.shutil, "which", return_value=None
        ):
            self.assertIsNone(ui_gallery.find_browser())

    def test_command_on_path_is_used(self) -> None:
        with mock.patch.object(ui_gallery, "BROWSER_CANDIDATES", ()), mock.patch.object(
            ui_gallery.shutil, "which", side_effect=lambda name: "/usr/bin/chromium" if name == "chromium" else None
        ):
            self.assertEqual(ui_gallery.find_browser(), "/usr/bin/chromium")


class ScreenshotTests(unittest.TestCase):
    def test_old_image_does_not_count_as_a_successful_capture(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            html = Path(raw, "frame-80x24.html")
            html.write_text("<h2>frame — 80×24</h2>", encoding="utf-8")
            html.with_suffix(".png").write_bytes(b"old screenshot")
            with mock.patch.object(ui_gallery.subprocess, "run", return_value=mock.Mock(returncode=0)):
                self.assertFalse(ui_gallery.screenshot("chromium", html, Path(raw, "profile")))

    def test_new_capture_counts_as_success(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            html = Path(raw, "frame-80x24.html")
            html.write_text("<h2>frame — 80×24</h2>", encoding="utf-8")

            def capture(*args: object, **kwargs: object) -> object:
                html.with_suffix(".png").write_bytes(b"new screenshot")
                return mock.Mock(returncode=0)

            with mock.patch.object(ui_gallery.subprocess, "run", side_effect=capture):
                self.assertTrue(ui_gallery.screenshot("chromium", html, Path(raw, "profile")))


class MainTests(unittest.TestCase):
    def test_missing_frames_fail_without_running_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as raw, mock.patch.dict(
            "os.environ", {"HERDR_UI_GALLERY_DIR": raw}
        ), mock.patch.object(ui_gallery, "run_gallery_test") as run_test:
            self.assertEqual(ui_gallery.main(["--skip-test", "--no-png"]), 1)
        run_test.assert_not_called()

    def test_html_only_mode_lists_frames(self) -> None:
        with tempfile.TemporaryDirectory() as raw, mock.patch.dict("os.environ", {"HERDR_UI_GALLERY_DIR": raw}):
            Path(raw, "gallery.html").write_text("index", encoding="utf-8")
            Path(raw, "gallery-workbench-80x24.html").write_text("frame", encoding="utf-8")
            self.assertEqual(ui_gallery.main(["--skip-test", "--no-png"]), 0)


if __name__ == "__main__":
    unittest.main()
