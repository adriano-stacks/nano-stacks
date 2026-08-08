#!/usr/bin/env python3
"""Verify the offline deck with a headless browser and its printed PDF."""

from __future__ import annotations

import argparse
import html
import json
import re
import subprocess
import tempfile
from html.parser import HTMLParser
from pathlib import Path


VIEWPORTS = ((1280, 720), (1366, 768), (1920, 1080))


class LocalReferences(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.references: list[str] = []

    def handle_starttag(self, _tag: str, attrs: list[tuple[str, str | None]]) -> None:
        for name, value in attrs:
            if name in {"href", "src"} and value:
                self.references.append(value)


def command(*arguments: str) -> str:
    result = subprocess.run(arguments, check=True, text=True, capture_output=True)
    return result.stdout


def local_assets(deck: Path) -> None:
    parser = LocalReferences()
    parser.feed(deck.read_text(encoding="utf-8"))
    for reference in parser.references:
        if re.match(r"^[a-z][a-z0-9+.-]*:", reference, re.IGNORECASE):
            raise AssertionError(f"remote or absolute URI in deck: {reference}")
        target = (deck.parent / reference.split("#", 1)[0]).resolve()
        if not target.is_file():
            raise AssertionError(f"missing local deck asset: {reference}")
    for source in (deck, deck.parent / "style.css", deck.parent / "deck.js"):
        text = source.read_text(encoding="utf-8")
        if re.search(r"\b(?:https?|data):", text, re.IGNORECASE):
            raise AssertionError(f"network or embedded asset URI in {source}")


def dumped_report(browser: str, deck: Path, width: int, height: int) -> dict:
    output = command(
        browser,
        "--headless=new",
        "--disable-gpu",
        "--no-sandbox",
        "--disable-background-networking",
        "--disable-component-update",
        "--host-resolver-rules=MAP * ~NOTFOUND",
        f"--window-size={width},{height}",
        "--dump-dom",
        f"{deck.as_uri()}?selftest=1#1",
    )
    match = re.search(
        r'<pre id="deck-selftest" hidden="">(.*?)</pre>', output, re.DOTALL
    )
    if match is None:
        raise AssertionError(
            f"headless browser did not return the self-test at {width}x{height}"
        )
    return json.loads(html.unescape(match.group(1)))


def browser_report(browser: str, deck: Path, width: int, height: int) -> dict:
    report = dumped_report(browser, deck, width, height)
    actual_width, actual_height = report["viewport"]
    if [actual_width, actual_height] != [width, height]:
        report = dumped_report(
            browser,
            deck,
            width + width - actual_width,
            height + height - actual_height,
        )
    if report["viewport"] != [width, height]:
        raise AssertionError(
            f"headless browser produced viewport {report['viewport']}, not {width}x{height}"
        )
    if report["navigation_failures"] or report["overflow"]:
        raise AssertionError(f"deck failed at {width}x{height}: {report}")
    return report


def printed_deck(browser: str, pdfinfo: str, deck: Path, slides: int) -> dict:
    with tempfile.TemporaryDirectory(prefix="nano-stacks-deck-") as temporary:
        pdf = Path(temporary) / "deck.pdf"
        command(
            browser,
            "--headless=new",
            "--disable-gpu",
            "--no-sandbox",
            "--disable-background-networking",
            "--disable-component-update",
            "--host-resolver-rules=MAP * ~NOTFOUND",
            "--no-pdf-header-footer",
            f"--print-to-pdf={pdf}",
            deck.as_uri(),
        )
        information = command(pdfinfo, str(pdf))
        pages = re.search(r"^Pages:\s+(\d+)$", information, re.MULTILINE)
        size = re.search(
            r"^Page size:\s+([0-9.]+) x ([0-9.]+) pts", information, re.MULTILINE
        )
        if pages is None or int(pages.group(1)) != slides:
            raise AssertionError(
                f"printed deck has the wrong page count:\n{information}"
            )
        if size is None:
            raise AssertionError(f"printed deck has no page size:\n{information}")
        width, height = map(float, size.groups())
        if abs(width / height - 16 / 9) > 0.002:
            raise AssertionError(f"printed page is not 16:9: {width} x {height} points")
        return {
            "pages": slides,
            "page_points": [width, height],
            "bytes": pdf.stat().st_size,
        }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--browser", default="chromium")
    parser.add_argument("--pdfinfo", default="pdfinfo")
    parser.add_argument(
        "--deck",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "index.html",
    )
    args = parser.parse_args()
    deck = args.deck.resolve()
    local_assets(deck)
    reports = [browser_report(args.browser, deck, *viewport) for viewport in VIEWPORTS]
    slides = reports[0]["slides"]
    if any(report["slides"] != slides for report in reports):
        raise AssertionError("headless runs disagree on the slide count")
    result = {
        "offline_assets": "local files only",
        "viewports": reports,
        "print": printed_deck(args.browser, args.pdfinfo, deck, slides),
    }
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
