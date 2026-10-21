#!/usr/bin/env python3
"""Render the manual TUI gallery and optionally screenshot it with a headless browser.

The gallery itself is an ignored Rust test (`client::shell::tests::gallery::ui_gallery`)
that writes one HTML file per composed frame into `target/ui-gallery/`. This script
runs that test and, when Chrome or Edge is available, converts every frame into a PNG
next to its HTML. Everything stays inside the project's `target/` directory.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CELL_WIDTH_PX = 7.8
CELL_HEIGHT_PX = 17
PAGE_PADDING_X = 48
PAGE_PADDING_Y = 96
SIZE_PATTERN = re.compile(r"(\d+)x(\d+)")

BROWSER_CANDIDATES = (
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
)
BROWSER_COMMANDS = ("google-chrome", "chromium", "chromium-browser", "microsoft-edge", "msedge", "chrome")


def gallery_dir() -> Path:
    override = os.environ.get("HERDR_UI_GALLERY_DIR")
    return Path(override) if override else ROOT / "target" / "ui-gallery"


def find_browser() -> str | None:
    for candidate in BROWSER_CANDIDATES:
        if Path(candidate).is_file():
            return candidate
    for command in BROWSER_COMMANDS:
        found = shutil.which(command)
        if found:
            return found
    return None


def window_size(html: Path) -> tuple[int, int]:
    """Size the browser window from the frame dimensions recorded in the page heading."""
    text = html.read_text(encoding="utf-8", errors="replace")
    heading = re.search(r"<h2[^>]*>.*?(\d+)×(\d+)</h2>", text)
    if heading:
        cols, rows = int(heading.group(1)), int(heading.group(2))
    else:
        match = SIZE_PATTERN.search(html.stem)
        cols, rows = (int(match.group(1)), int(match.group(2))) if match else (160, 45)
    return int(cols * CELL_WIDTH_PX) + PAGE_PADDING_X, rows * CELL_HEIGHT_PX + PAGE_PADDING_Y


def run_gallery_test() -> int:
    command = ["cargo", "test", "--bin", "herdr", "ui_gallery", "--", "--ignored"]
    print("+ " + " ".join(command), flush=True)
    return subprocess.run(command, cwd=ROOT, check=False).returncode


def screenshot(browser: str, html: Path, profile: Path) -> bool:
    width, height = window_size(html)
    png = html.with_suffix(".png")
    # An old PNG must not make a browser that exits without capturing look successful.
    before = png.stat().st_mtime_ns if png.is_file() else None
    command = [
        browser,
        "--headless=new",
        "--disable-gpu",
        "--hide-scrollbars",
        f"--user-data-dir={profile}",
        f"--screenshot={png}",
        f"--window-size={width},{height}",
        html.resolve().as_uri(),
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False, timeout=120)
    return (
        result.returncode == 0
        and png.is_file()
        and (before is None or png.stat().st_mtime_ns != before)
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-test", action="store_true", help="reuse existing HTML frames")
    parser.add_argument("--no-png", action="store_true", help="only render HTML frames")
    parser.add_argument("--filter", default="", help="only screenshot frames whose name contains this text")
    parser.add_argument("--browser", help="use an explicit Chrome/Chromium/Edge executable")
    args = parser.parse_args(argv)

    if not args.skip_test:
        status = run_gallery_test()
        if status != 0:
            return status
    directory = gallery_dir()
    frames = sorted(
        path for path in directory.glob("*.html") if path.name != "gallery.html" and args.filter in path.stem
    )
    if not frames:
        print(f"no gallery frames in {directory}", file=sys.stderr)
        return 1
    print(f"gallery: {directory / 'gallery.html'} ({len(frames)} frames)")
    if args.no_png:
        return 0
    browser = args.browser or find_browser()
    if browser is None:
        print("no Chrome/Edge found; HTML frames only", file=sys.stderr)
        return 0
    profile_root = ROOT / "target" / "ui-gallery-browser-profile"
    profile_root.mkdir(parents=True, exist_ok=True)
    # Independent runs must not reuse a profile held by an interrupted browser.
    with tempfile.TemporaryDirectory(prefix="run-", dir=profile_root) as raw_profile:
        profile = Path(raw_profile)
        failed = [frame.name for frame in frames if not screenshot(browser, frame, profile)]
    for name in failed:
        print(f"screenshot failed: {name}", file=sys.stderr)
    print(f"png: {len(frames) - len(failed)}/{len(frames)}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
