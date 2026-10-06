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
        ("tutorials/brazil/index.html", "03-shard.sh"),
        ("changelog/index.html", "Changelog"),
        ("reference/cli/index.html", "export-pmtiles"),
        ("reference/python/index.html", "export_pmtiles"),
    ],
)
def test_page_renders(site: Path, page: str, marker: str) -> None:
    html = (site / page).read_text(encoding="utf-8")
    assert marker in html, f"{page} lacks {marker!r}"
    assert "--8&lt;--" not in html, f"{page} shows an unexpanded include"


def test_absolute_site_links_resolve(site: Path) -> None:
    """Absolute links to the site (README, tutorials) point at real pages.

    The README renders on GitHub, crates.io, and PyPI, so it links to the
    site by full URL, which the strict build does not check.
    """
    broken = []
    for page in site_pages():
        text = page.read_text(encoding="utf-8")
        for path, anchor in SITE_LINK.findall(text):
            target = site / path
            if not path.endswith(".html"):
                target = target / "index.html"
            if not target.is_file():
                broken.append(f"{page.name}: {SITE_URL}{path} (no page)")
            elif anchor and f'id="{anchor}"' not in target.read_text(encoding="utf-8"):
                broken.append(f"{page.name}: {SITE_URL}{path}#{anchor} (no anchor)")
    assert not broken, "Broken links to the docs site:\n" + "\n".join(broken)
