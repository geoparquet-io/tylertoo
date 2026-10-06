r"""Generate the Python API reference page (Markdown) on stdout.

The source of truth is `python/tylertoo/__init__.py`: its type
annotations, defaults, and Google-style docstrings. The script imports
the built package and renders each public function, then each report
type. The committed copy is `docs/reference/python.md`; CI fails when it
drifts, so never edit it by hand.

Regenerate:

    cd crates/python
    uv run --no-sync --with docstring-parser==0.18.0 \
        python scripts/gen_reference.py > ../../docs/reference/python.md
"""

from __future__ import annotations

import inspect
import sys
from typing import Any

from docstring_parser import parse

import tylertoo

# Workflow order, so the diff guard sees deterministic output: the
# overview -> export path, then validation, then the deprecated
# one-shot.
FUNCTION_ORDER = ["overview", "export_pmtiles", "validate", "convert"]

# Report types, outermost first.
TYPE_ORDER = [
    "OverviewReport",
    "LevelReport",
    "SkippedLevel",
    "OutOfRangeExemplar",
    "RemoteFetch",
    "ExportReport",
    "ZoomReport",
    "SkippedPropertyColumn",
    "ValidationReport",
    "ValidationCheck",
]

# Integer defaults at least this large print with `_` separators.
GROUP = 10_000

BANNER = (
    "<!-- GENERATED FILE — do not edit by hand.\n"
    "     Regenerate: cd crates/python && uv run --no-sync \\\n"
    "       --with docstring-parser==0.18.0 \\\n"
    "       python scripts/gen_reference.py > ../../docs/reference/python.md\n"
    "     CI fails if this file drifts from python/tylertoo/__init__.py. -->\n"
)


def _flat(text: str | None) -> str:
    """Join each paragraph's lines into one line.

    Args:
        text: Docstring text, possibly indented and wrapped.

    Returns:
        The paragraphs, one per line, separated by blank lines.
    """
    if not text:
        return ""
    paragraphs = inspect.cleandoc(text).split("\n\n")
    return "\n\n".join(" ".join(p.split()) for p in paragraphs if p.strip())


def _default(value: Any) -> str:
    """Render a default value the way the annotations are written.

    Args:
        value: The parameter's default.

    Returns:
        Python source for the value, with double-quoted strings.
    """
    if isinstance(value, str):
        return f'"{value}"'
    if isinstance(value, int) and not isinstance(value, bool) and value >= GROUP:
        return f"{value:_}"
    return repr(value)


def _signature(name: str, func: Any) -> str:
    """Render a function signature from its annotations' source text.

    Args:
        name: The function's public name.
        func: The function.

    Returns:
        The signature, one parameter per line.
    """
    notes = func.__annotations__
    lines = [f"{name}("]
    keyword_only_started = False
    for param in inspect.signature(func).parameters.values():
        if param.kind is param.KEYWORD_ONLY and not keyword_only_started:
            lines.append("    *,")
            keyword_only_started = True
        text = f"    {param.name}: {notes[param.name]}"
        if param.default is not param.empty:
            text += f" = {_default(param.default)}"
        lines.append(text + ",")
    lines.append(f") -> {notes['return']}")
    return "\n".join(lines)


def _link_type(type_name: str) -> str:
    """Link a report type name to its section.

    Args:
        type_name: A return annotation.

    Returns:
        A Markdown link when the type is documented here, else code.
    """
    if type_name in TYPE_ORDER:
        return f"[`{type_name}`](#{type_name.lower()})"
    return f"`{type_name}`"


def _render_function(name: str) -> str:
    """Render one public function.

    Args:
        name: The function's public name.

    Returns:
        The function's Markdown section.
    """
    func = getattr(tylertoo, name)
    doc = parse(inspect.getdoc(func) or "")
    lines = [f"## `{name}`", "", "```python", _signature(name, func), "```", ""]
    lines += [_flat(doc.short_description), ""]
    if doc.long_description:
        lines += [_flat(doc.long_description), ""]
    if doc.params:
        lines += ["###### **Parameters:**", ""]
        lines += [f"* `{p.arg_name}`: {_flat(p.description)}" for p in doc.params]
        lines.append("")
    if doc.returns and doc.returns.description:
        returned = _link_type(func.__annotations__["return"])
        lines += ["###### **Returns:**", ""]
        lines += [f"{returned}: {_flat(doc.returns.description)}", ""]
    if doc.raises:
        lines += ["###### **Raises:**", ""]
        lines += [f"* `{r.type_name}`: {_flat(r.description)}" for r in doc.raises]
        lines.append("")
    snippets = [e.description for e in doc.examples if e.description]
    if snippets:
        lines += ["###### **Example:**", "", "```python", *snippets, "```", ""]
    return "\n".join(lines)


def _render_type(name: str) -> str:
    """Render one report type as its class definition plus field notes.

    Args:
        name: The type's public name.

    Returns:
        The type's Markdown section.
    """
    cls = getattr(tylertoo, name)
    doc = parse(inspect.getdoc(cls) or "")
    # TypedDict wraps each annotation in a ForwardRef; print its source.
    fields = [
        f"    {key}: {getattr(value, '__forward_arg__', value)}"
        for key, value in cls.__annotations__.items()
    ]
    lines = [f"### `{name}`", "", _flat(doc.short_description), ""]
    lines += ["```python", f"class {name}(TypedDict):", *fields, "```", ""]
    if doc.params:
        lines += [f"* `{p.arg_name}`: {_flat(p.description)}" for p in doc.params]
        lines.append("")
    return "\n".join(lines)


def main() -> None:
    """Print the reference page, after checking that it covers the API.

    Raises:
        SystemExit: A public name is missing from the page, or a listed
            name is not public.
    """
    public = set(tylertoo.__all__)
    listed = FUNCTION_ORDER + TYPE_ORDER
    if missing := [n for n in listed if n not in public]:
        raise SystemExit(f"Listed but not public: {missing}")
    if uncovered := sorted(public - set(listed)):
        raise SystemExit(f"Public but not listed here: {uncovered}")

    parts = [BANNER, "# Python reference", "", _flat(tylertoo.__doc__), ""]
    parts += [_render_function(name) for name in FUNCTION_ORDER]
    parts += ["## Report types", ""]
    parts += [_render_type(name) for name in TYPE_ORDER]
    sys.stdout.write("\n".join(parts).rstrip() + "\n")


if __name__ == "__main__":
    main()
