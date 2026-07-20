"""Generate the Python API reference (Markdown) for the tylertoo package.

Single source of truth is the ``///`` doc-comments on the ``#[pyfunction]`` s in
``crates/python/src/lib.rs`` — pyo3 compiles those into each function's
``__doc__``. This script imports the *built* extension module, reads the
runtime signatures (``inspect.signature``) and Google-style docstrings, and
renders them to Markdown on **stdout**. The committed copy lives at
``docs/reference/python.md`` and is diff-guarded by CI — never hand-edit it.

Regenerate::

    cd crates/python
    uv run --no-sync --with docstring-parser==0.18.0 \\
        python scripts/gen_reference.py > ../../docs/reference/python.md

The rendering mirrors the CLI reference (``reference/cli.md``): one ``##``
section per function, an ``###### **…:**`` sub-head per docstring section, and
``* `name` — description`` bullets.
"""

from __future__ import annotations

import inspect
import re
import sys

from docstring_parser import parse

import tylertoo

# Fixed, workflow-first order (deterministic output for the CI diff-guard).
# The two-step overview → export path first, then validation, then the
# one-shot facade last. Every name here must be public (in ``__all__``).
FUNCTION_ORDER = ["overview", "export_pmtiles", "validate", "convert"]

BANNER = (
    "<!-- GENERATED FILE — do not edit by hand.\n"
    "     Regenerate: cd crates/python && uv run --no-sync \\\n"
    "       --with docstring-parser==0.18.0 \\\n"
    "       python scripts/gen_reference.py > ../../docs/reference/python.md\n"
    "     CI fails if this file drifts from the lib.rs docstrings. -->\n"
)


def _rst_to_md(text: str) -> str:
    """Light RST → Markdown touch-ups for prose harvested from docstrings."""
    # RST inline literal ``x`` → Markdown `x`.
    text = re.sub(r"``([^`]+)``", r"`\1`", text)
    # RST deprecated directive → a bold lead-in.
    return text.replace(".. deprecated::", "**Deprecated.**")


def _reflow(text: str | None) -> str:
    """Collapse each blank-line-delimited paragraph to a single reflowed line.

    Docstring bodies are indented for readability in Rust source; left as-is,
    that indentation renders as stray Markdown code blocks. Joining each
    paragraph's lines with spaces yields clean prose paragraphs.
    """
    if not text:
        return ""
    paragraphs = re.split(r"\n\s*\n", text.strip())
    out = []
    for para in paragraphs:
        joined = " ".join(line.strip() for line in para.splitlines() if line.strip())
        if joined:
            out.append(_rst_to_md(joined))
    return "\n\n".join(out)


def _render_function(name: str) -> str:
    obj = getattr(tylertoo, name)
    signature = str(inspect.signature(obj))
    doc = parse(inspect.getdoc(obj) or "")

    lines: list[str] = [f"## `{name}`", ""]
    lines += ["```python", f"{name}{signature}", "```", ""]

    if doc.short_description:
        lines += [_rst_to_md(doc.short_description.strip()), ""]

    long = _reflow(doc.long_description)
    if long:
        lines += [long, ""]

    if doc.params:
        lines += ["###### **Parameters:**", ""]
        for p in doc.params:
            type_part = f" (`{p.type_name}`)" if p.type_name else ""
            desc = _rst_to_md(_reflow(p.description))
            lines.append(f"* `{p.arg_name}`{type_part} — {desc}")
        lines.append("")

    if doc.returns and (doc.returns.description or doc.returns.type_name):
        lines += ["###### **Returns:**", ""]
        type_part = f"`{doc.returns.type_name}` — " if doc.returns.type_name else ""
        lines += [f"{type_part}{_rst_to_md(_reflow(doc.returns.description))}", ""]

    if doc.raises:
        lines += ["###### **Raises:**", ""]
        for r in doc.raises:
            type_part = f"`{r.type_name}` — " if r.type_name else ""
            lines.append(f"* {type_part}{_rst_to_md(_reflow(r.description))}")
        lines.append("")

    examples = [e for e in doc.examples if (e.snippet or e.description)]
    if examples:
        lines += ["###### **Example:**", "", "```python"]
        for e in examples:
            lines.append((e.snippet or e.description or "").rstrip())
        lines += ["```", ""]

    return "\n".join(lines)


def main() -> None:
    public = set(getattr(tylertoo, "__all__", []))
    missing = [n for n in FUNCTION_ORDER if n not in public]
    if missing:
        raise SystemExit(f"FUNCTION_ORDER lists non-public names: {missing}")
    uncovered = sorted(public - set(FUNCTION_ORDER))
    if uncovered:
        raise SystemExit(
            f"Public functions missing from FUNCTION_ORDER: {uncovered}. "
            "Add them to keep the reference complete."
        )

    module_doc = _reflow(inspect.getdoc(tylertoo)) or (
        "The `tylertoo` Python package binds the tylertoo-core engine: build "
        "multi-resolution GeoParquet overview files and export them to PMTiles "
        "archives, with the same knobs and defaults as the CLI."
    )

    parts = [BANNER, "# Python reference", "", module_doc, ""]
    parts += [_render_function(name) for name in FUNCTION_ORDER]
    sys.stdout.write("\n".join(parts).rstrip() + "\n")


if __name__ == "__main__":
    main()
