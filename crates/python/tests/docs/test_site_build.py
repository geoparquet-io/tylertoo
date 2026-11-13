"""The docs site builds strictly, and the includes land where expected.

``zensical build --strict`` fails on a broken link, a missing anchor, or a
missing snippet include. The checks below confirm the pages that are pure
includes (home page, tutorials, changelog) render real content.
"""

from __future__ import annotations

import re
import shutil
import subprocess
from pathlib import Path

import pytest
from doc_blocks import REPO_ROOT, site_pages

SITE = REPO_ROOT / "site"
SITE_URL = "https://geoparquet-io.github.io/tylertoo/"
SITE_LINK = re.compile(re.escape(SITE_URL) + r"([^)\s#>\"]*?)/?(?:#([\w-]+))?[)\s>\"]")


@pytest.fixture(scope="module")
def site() -> Path:
    zensical = shutil.which("zensical")
    if zensical is None:
        pytest.fail("zensical not found; run `uv sync --group docs`")
    proc = subprocess.run(
        [zensical, "build", "--strict", "--clean"],
        cwd=REPO_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    assert proc.returncode == 0, f"zensical build failed:\n{proc.stdout}"
    return SITE


@pytest.mark.parametrize(
    ("page", "marker"),
    [
        ("index.html", "Quickstart"),
        ("tutorials/madagascar/index.html", "02-preview.sh"),
        ("tutorials/brazil/index.html", "02-shard.sh"),
        ("changelog/index.html", "Changelog"),
        ("reference/cli/index.html", "export-pmtiles"),
        ("reference/python/index.html", "export_pmtiles"),
        ("spec/overviews/index.html", "canonical_level"),
    ],
)
def test_page_renders(site: Path, page: str, marker: str) -> None:
    html = (site / page).read_text(encoding="utf-8")
    assert marker in html, f"{page} lacks {marker!r}"
    assert "--8&lt;--" not in html, f"{page} shows an unexpanded include"


def _link_problem(site: Path, path: str, anchor: str) -> str | None:
    """Why `SITE_URL + path#anchor` does not resolve in the built site."""
    target = site / path
    if not path.endswith(".html"):
        target = target / "index.html"
    if not target.is_file():
        return "no page"
    if anchor and f'id="{anchor}"' not in target.read_text(encoding="utf-8"):
        return "no anchor"
    return None


def test_absolute_site_links_resolve(site: Path) -> None:
    """Absolute links to the site (README, tutorials) point at real pages.

    The README renders on GitHub, crates.io, and PyPI, so it links to the
    site by full URL, which the strict build does not check.
    """
    broken = [
        f"{page.name}: {SITE_URL}{path}{'#' + anchor if anchor else ''} ({problem})"
        for page in site_pages()
        for path, anchor in SITE_LINK.findall(page.read_text(encoding="utf-8"))
        if (problem := _link_problem(site, path, anchor))
    ]
    assert not broken, "Broken links to the docs site:\n" + "\n".join(broken)


def _script_path(site: Path, html: Path, src: str) -> Path:
    """Where a page's ``<script src>`` points, as a resolved filesystem path."""
    url = src.split("?", maxsplit=1)[0]
    if not url.startswith("/"):
        return (html.parent / url).resolve()
    # Root-absolute: valid only under the site's /tylertoo/ path.
    if src.startswith("/tylertoo/"):
        return (site / src.removeprefix("/tylertoo/")).resolve()
    return Path("/").resolve()


def test_local_scripts_resolve(site: Path) -> None:
    """Every local ``<script src>`` resolves, from its page, to a built file.

    Strict builds do not check raw HTML: a tutorial map's script one ``../``
    too deep resolved above the site's ``/tylertoo/`` root and 404ed.
    """
    broken = []
    for html in site.rglob("*.html"):
        for src in re.findall(r'<script[^>]*\bsrc="([^"]+)"', html.read_text()):
            if re.match(r"(?:[a-z]+:)?//", src):
                continue
            path = _script_path(site, html, src)
            if not path.is_relative_to(site.resolve()) or not path.is_file():
                broken.append(f"{html.relative_to(site)}: {src}")
    assert not broken, broken
