"""Check the source that tutorial snippet includes expose to readers."""

from pathlib import Path

import doc_blocks
import pytest


@pytest.fixture
def example(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setattr(doc_blocks, "REPO_ROOT", tmp_path)
    example = tmp_path / "examples" / "demo"
    example.mkdir(parents=True)
    (tmp_path / "docs" / "tutorials").mkdir(parents=True)
    return example


@pytest.mark.parametrize("sectioned", [False, True])
def test_rendered_source(example: Path, sectioned: bool) -> None:
    step = example / "01-run.sh"
    step.write_text("echo hello\n", encoding="utf-8")
    fence = doc_blocks.fence_for(step)
    readme = (
        "Hidden introduction\n<!-- --8<-- [start:build] -->\n"
        f"{fence}\n<!-- --8<-- [end:build] -->\n"
        "Hidden middle\n<!-- --8<-- [start:run] -->\n"
        "Run the example.\n<!-- --8<-- [end:run] -->\nHidden ending\n"
    )
    (example / "README.md").write_text(readme, encoding="utf-8")
    source = "examples/demo/README.md"
    page = doc_blocks.REPO_ROOT / "docs" / "tutorials" / "demo.md"
    page.write_text(
        f'--8<-- "{source}:build"\nPage prose\n--8<-- "{source}:run"\n'
        if sectioned
        else f'--8<-- "{source}"\n',
        encoding="utf-8",
    )

    rendered = doc_blocks.rendered_source(example)

    assert fence in rendered
    assert "Run the example." in rendered
    if sectioned:
        assert "Page prose" in rendered
        assert "Hidden" not in rendered
        assert "start:build" not in rendered
    else:
        assert rendered == readme + "\n"


def test_rendered_source_excludes_steps_outside_section(example: Path) -> None:
    step = example / "01-run.sh"
    step.write_text("echo hidden\n", encoding="utf-8")
    (example / "README.md").write_text(
        doc_blocks.fence_for(step) + "\n<!-- --8<-- [start:build] -->\nIncluded prose\n"
        "<!-- --8<-- [end:build] -->\n",
        encoding="utf-8",
    )
    page = doc_blocks.REPO_ROOT / "docs" / "tutorials" / "demo.md"
    page.write_text('--8<-- "examples/demo/README.md:build"\n', encoding="utf-8")

    assert doc_blocks.fence_for(step) not in doc_blocks.rendered_source(example)


def test_rendered_source_rejects_missing_section(example: Path) -> None:
    (example / "README.md").write_text("No snippet markers\n", encoding="utf-8")
    page = doc_blocks.REPO_ROOT / "docs" / "tutorials" / "demo.md"
    page.write_text('--8<-- "examples/demo/README.md:build"\n', encoding="utf-8")

    with pytest.raises(ValueError, match="build"):
        doc_blocks.rendered_source(example)


def test_rendered_source_requires_tutorial_page(example: Path) -> None:
    (example / "README.md").write_text("Unpublished tutorial\n", encoding="utf-8")

    with pytest.raises(ValueError, match="tutorial"):
        doc_blocks.rendered_source(example)
