r"""Generate the Python API reference page (Markdown) on stdout.

The native functions provide generated defaults and Google-style
docstrings; their checked stubs provide type annotations. The script
imports the built package and renders each public function and report
type. The committed copy is `docs/reference/python.md`; CI fails when it
drifts, so never edit it by hand.

Regenerate:

    cd crates/python
    uv run --no-sync --with docstring-parser==0.18.0 \
        python scripts/gen_reference.py > ../../docs/reference/python.md
"""

from __future__ import annotations

import ast
import inspect
import sys
from pathlib import Path
from typing import Any

from docstring_parser import parse

import tylertoo

# Workflow order, so the diff guard sees deterministic output: the
# overview -> export path, multi-band pyramids, then decoding,
# validation, and the deprecated one-shot.
FUNCTION_ORDER = [
    "overview",
    "export_pmtiles",
    "pyramid",
    "decode",
    "validate",
    "convert",
]

# Start groups at named arguments, retaining docstring order.
OVERVIEW_GROUPS = {
    "input": "Input and levels",
    "sort_key": "Feature priority",
    "verbatim": "Geometry and thinning",
    "cluster": "Clustering",
    "coalesce_lines": "Line coalescing",
    "include_properties": "Properties",
    "cogp_compat": "Output, filtering, and memory",
}

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
    "PyramidReport",
    "PyramidBandReport",
    "PublicationMetadata",
    "DecodeReport",
    "ValidationReport",
    "ValidationCheck",
    "ConvertReport",
]

# Exception classes, after the report types.
EXCEPTION_ORDER = ["ConfigError"]

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


def _annotations(name: str) -> dict[str, str]:
    """Read annotations from the checked native API stub.

    Args:
        name: Public function name.

    Returns:
        Parameter and return annotations as source text.
    """
    source = Path(tylertoo.__file__).with_name("_tylertoo.pyi").read_text()
    function = next(
        node
        for node in ast.parse(source).body
        if isinstance(node, ast.FunctionDef) and node.name == name
    )
    notes = {
        arg.arg: ast.unparse(arg.annotation)
        for arg in [*function.args.args, *function.args.kwonlyargs]
        if arg.annotation is not None
    }
    if function.returns is not None:
        notes["return"] = ast.unparse(function.returns)
    return notes


def _signature(name: str, func: Any) -> str:
    """Render a function signature from its annotations' source text.

    Args:
        name: The function's public name.
        func: The function.

    Returns:
        The signature, one parameter per line.
    """
    notes = _annotations(name)
    lines = [f"{name}("]
    keyword_only_started = False
    parameters = inspect.signature(func).parameters
    order = [
        entry.arg_name
        for entry in parse(inspect.getdoc(func) or "").params
        if entry.arg_name in parameters
    ]
    order.extend(name for name in parameters if name not in order)
    for parameter_name in order:
        param = parameters[parameter_name]
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
        lines += ["### Parameters", ""]
        for param in doc.params:
            if name == "overview" and param.arg_name in OVERVIEW_GROUPS:
                if lines[-1]:
                    lines.append("")
                lines += [f"#### {OVERVIEW_GROUPS[param.arg_name]}", ""]
            lines += [f"* `{param.arg_name}`: {_flat(param.description)}"]
        lines.append("")
    if doc.returns and doc.returns.description:
        returned = _link_type(_annotations(name)["return"])
        lines += ["### Returns", ""]
        lines += [f"{returned}: {_flat(doc.returns.description)}", ""]
    if doc.raises:
        lines += ["### Raises", ""]
        lines += [f"* `{r.type_name}`: {_flat(r.description)}" for r in doc.raises]
        lines.append("")
    snippets = [e.description for e in doc.examples if e.description]
    if snippets:
        lines += ["### Example", "", "```python", *snippets, "```", ""]
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


def _render_exception(name: str) -> str:
    """Render one exception class: its base and its docstring.

    Args:
        name: The exception's public name.

    Returns:
        The exception's Markdown section.
    """
    cls = getattr(tylertoo, name)
    base = cls.__mro__[1].__name__
    lines = [f"### `{name}`", "", "```python", f"class {name}({base})", "```", ""]
    lines += [_flat(inspect.getdoc(cls)), ""]
    return "\n".join(lines)


def main() -> None:
    """Print the reference page, after checking that it covers the API.

    Raises:
        SystemExit: A public name is missing from the page, or a listed
            name is not public.
    """
    public = set(tylertoo.__all__)
    listed = FUNCTION_ORDER + TYPE_ORDER + EXCEPTION_ORDER
    if missing := [n for n in listed if n not in public]:
        raise SystemExit(f"Listed but not public: {missing}")
    if uncovered := sorted(public - set(listed)):
        raise SystemExit(f"Public but not listed here: {uncovered}")

    parts = [BANNER, "# Python reference", "", _flat(tylertoo.__doc__), ""]
    parts += [_render_function(name) for name in FUNCTION_ORDER]
    parts += ["## Report types", ""]
    parts += [_render_type(name) for name in TYPE_ORDER]
    parts += ["## Errors", ""]
    parts += [_render_exception(name) for name in EXCEPTION_ORDER]
    sys.stdout.write("\n".join(parts).rstrip() + "\n")


if __name__ == "__main__":
    main()
