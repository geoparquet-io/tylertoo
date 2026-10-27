"""Surface parity between the CLI and the Python API (#779, track G2).

The CLI's arguments come from ``crates/cli/tests/parity/clap_surface.json``,
a dump of the clap definitions that a Rust test keeps current. The Python
side comes from ``inspect.signature`` of the public functions and from the
``_tylertoo.pyi`` stub.

The gates:

- (a) Every clap flag of ``tiles``, ``overview``, ``export-pmtiles``,
  ``pyramid`` and ``decode`` maps to a keyword argument of the matching
  Python function, and every keyword argument maps back to a flag. The rule
  is kebab-case to snake_case, plus ``RENAMES``.
- (b) The Python defaults equal the clap defaults, the ``.pyi`` defaults
  equal ``__init__.py``'s, and both equal ``tylertoo._defaults()``, the
  core defaults #779's F3 track exposes.

An allowed difference is an entry in ``parity_exemptions.toml`` with a
reason and an issue number. A known, unfixed difference is an entry with
``xfail = true``, which runs as a strict xfail: when the drift is fixed the
test XPASSes, fails, and the entry has to go. ``PINNED`` holds the number of
entries in each table.
"""

from __future__ import annotations

import ast
import inspect
import json
import re
import sys
from pathlib import Path
from typing import Any, NamedTuple

import pytest

import tylertoo

if sys.version_info >= (3, 11):
    import tomllib
else:  # pragma: no cover - exercised on the 3.9 and 3.10 wheels only
    import tomli as tomllib

HERE = Path(__file__).resolve().parent
SURFACE = HERE.parents[1] / "cli" / "tests" / "parity" / "clap_surface.json"
LEDGER = HERE / "parity_exemptions.toml"
STUB = HERE.parent / "python" / "tylertoo" / "_tylertoo.pyi"

# The CLI command each Python function mirrors. `tiles` is the one-shot
# command; Python spells it `convert` until F3 decides on `tiles`.
FUNCTIONS = {
    "tiles": "tiles" if hasattr(tylertoo, "tiles") else "convert",
    "overview": "overview",
    "export-pmtiles": "export_pmtiles",
    "pyramid": "pyramid",
    "decode": "decode",
}

# Flags whose keyword argument is not the snake_case spelling, by command
# ("*" for every command).
RENAMES: dict[str, dict[str, str]] = {
    "*": {
        "include-property": "include_properties",
        "exclude-property": "exclude_properties",
        "accumulate-attribute": "accumulate_attributes",
        "max-tile-size": "tile_size_limit",
        "class-rank": "class_ranks",
        # Negated flags: the keyword names the positive behaviour.
        "no-cascade": "cascade",
        "no-streaming": "streaming",
        "no-density-drop": "density_drop",
        "no-coalesce-lines": "coalesce_lines",
        "no-simple-clip-fastpath": "simple_clip_fastpath",
    },
    "overview": {"gsd": "gsds"},
    "pyramid": {"band": "bands"},
}
NEGATED = {
    "no-cascade",
    "no-streaming",
    "no-density-drop",
    "no-coalesce-lines",
    "no-simple-clip-fastpath",
}
# clap's `auto` is the Python 0 sentinel for these.
AUTO_IS_ZERO = {"partition-wave", "in-flight-batches", "read-workers"}

# Entries per ledger table and kind; see the module docstring.
PINNED = {
    ("flag", "exempt"): 12,
    ("flag", "xfail"): 60,
    ("kwarg", "exempt"): 2,
    ("kwarg", "xfail"): 2,
    ("default", "exempt"): 0,
    ("default", "xfail"): 2,
}


class Arg(NamedTuple):
    """One clap argument from the dump."""

    command: str
    name: str
    kind: str
    default: str | None
    positional: bool


class Entry(NamedTuple):
    """One ledger entry."""

    table: str
    command: str
    name: str
    reason: str
    issue: int
    xfail: bool


def _surface() -> dict[str, list[Arg]]:
    raw = json.loads(SURFACE.read_text(encoding="utf-8"))
    return {
        command: [
            Arg(
                command,
                a["long"] or a["id"],
                a["kind"],
                a["default"],
                a["positional"],
            )
            for a in args
            if not a["hidden"]
        ]
        for command, args in raw.items()
        if command in FUNCTIONS
    }


def _ledger() -> list[Entry]:
    raw = tomllib.loads(LEDGER.read_text(encoding="utf-8"))
    return [
        Entry(
            table,
            row["command"],
            row.get("flag") or row["kwarg"],
            row.get("reason", ""),
            row.get("issue", 0),
            row.get("xfail", False),
        )
        for table, rows in raw.items()
        for row in rows
    ]


ARGS = _surface()
ENTRIES = _ledger()


def _entry(table: str, command: str, name: str) -> Entry | None:
    for e in ENTRIES:
        if (e.table, e.command, e.name) == (table, command, name):
            return e
    return None


def _kwarg_for(command: str, flag: str) -> str:
    renames = {**RENAMES["*"], **RENAMES.get(command, {})}
    return renames.get(flag, flag.replace("-", "_"))


def _signature(command: str) -> dict[str, inspect.Parameter]:
    func = getattr(tylertoo, FUNCTIONS[command])
    return dict(inspect.signature(func).parameters)


def _flags_by_kwarg(command: str) -> dict[str, Arg]:
    return {_kwarg_for(command, a.name): a for a in ARGS[command]}


def _params(table: str, pairs: list[tuple[str, str]]) -> list[Any]:
    """Pytest params, skipping exempt pairs and marking xfail ones."""
    params = []
    for command, name in pairs:
        e = _entry(table, command, name)
        if e is not None and not e.xfail:
            continue
        marks = (
            [pytest.mark.xfail(strict=True, reason=f"#{e.issue}: {e.reason}")]
            if e is not None
            else []
        )
        params.append(pytest.param(command, name, id=f"{command}:{name}", marks=marks))
    return params


FLAG_PAIRS = [(c, a.name) for c, args in ARGS.items() for a in args]
KWARG_PAIRS = [(c, k) for c in ARGS for k in _signature(c)]


# --- (a) flag <-> kwarg ------------------------------------------------------


@pytest.mark.parametrize(("command", "flag"), _params("flag", FLAG_PAIRS))
def test_every_flag_has_a_kwarg(command: str, flag: str) -> None:
    # Would have caught #443 (`--feature-id` on the CLI, not in Python) and
    # every one-sided fix of the kind #779 counts: 11 fix PRs edited both
    # main.rs and lib.rs.
    kwarg = _kwarg_for(command, flag)
    assert kwarg in _signature(command), (
        f"`tylertoo {command} --{flag}` has no `{FUNCTIONS[command]}({kwarg}=...)`: "
        "add the keyword argument, or list it in parity_exemptions.toml [[flag]]"
    )


@pytest.mark.parametrize(("command", "kwarg"), _params("kwarg", KWARG_PAIRS))
def test_every_kwarg_has_a_flag(command: str, kwarg: str) -> None:
    # The reverse of the above: a Python-only option is drift too (#709).
    assert kwarg in _flags_by_kwarg(command), (
        f"`{FUNCTIONS[command]}({kwarg}=...)` has no `tylertoo {command}` flag: "
        "add the flag, or list it in parity_exemptions.toml [[kwarg]]"
    )


# --- (b) defaults --------------------------------------------------------------

SIZE = re.compile(r"^(\d+)\s*([KMG]?)B?$", re.IGNORECASE)


def _number(text: str) -> float | None:
    """A clap default as a number: a byte size such as `2M`, or a float."""
    m = SIZE.match(text.strip())
    if m is not None:
        power = "_KMG".index((m.group(2) or "_").upper())
        return float(int(m.group(1)) << (10 * power))
    try:
        return float(text)
    except ValueError:
        return None


def _same_default(arg: Arg, python: object) -> bool:
    """Whether a Python default means the same as the clap default."""
    if arg.kind in {"flag", "negated-flag"}:
        # An unset flag is off. Its negated keyword is on, or None for a
        # tri-state keyword whose None means "not passed".
        if arg.name in NEGATED:
            return python is True or python is None
        return python is False
    if arg.default is None or python is None or isinstance(python, bool):
        return arg.default is None and python is None
    if arg.name in AUTO_IS_ZERO and arg.default == "auto":
        return python == 0
    if isinstance(python, (int, float)):
        return _number(arg.default) == python
    return arg.default == python


def _default_pairs() -> list[tuple[str, str]]:
    pairs = []
    for command, args in ARGS.items():
        sig = _signature(command)
        for a in args:
            p = sig.get(_kwarg_for(command, a.name))
            if a.positional or p is None or p.default is inspect.Parameter.empty:
                continue
            pairs.append((command, a.name))
    return pairs


@pytest.mark.parametrize(("command", "flag"), _params("default", _default_pairs()))
def test_python_default_matches_the_clap_default(command: str, flag: str) -> None:
    # Would have caught the two hand-synced defaults #699 found in
    # `convert()`, and the size-cap default #761 changed on three surfaces.
    arg = next(a for a in ARGS[command] if a.name == flag)
    param = _signature(command)[_kwarg_for(command, flag)]
    assert _same_default(arg, param.default), (
        f"`tylertoo {command} --{flag}` defaults to {arg.default!r}, "
        f"`{FUNCTIONS[command]}({param.name}=...)` to {param.default!r}"
    )


def _stub_signatures() -> dict[str, dict[str, object]]:
    """Each stub function's keyword defaults (``...`` when none)."""
    tree = ast.parse(STUB.read_text(encoding="utf-8"))
    out: dict[str, dict[str, object]] = {}
    for node in tree.body:
        if not isinstance(node, ast.FunctionDef):
            continue
        a = node.args
        positional = a.posonlyargs + a.args
        defaults: list[ast.expr | None] = [None] * (
            len(positional) - len(a.defaults)
        ) + list(a.defaults)
        pairs = list(zip(positional, defaults)) + list(zip(a.kwonlyargs, a.kw_defaults))
        out[node.name] = {
            arg.arg: (... if d is None else ast.literal_eval(d)) for arg, d in pairs
        }
    return out


def _init_signature(name: str) -> dict[str, object]:
    params = inspect.signature(getattr(tylertoo, name)).parameters
    return {
        k: (... if p.default is inspect.Parameter.empty else p.default)
        for k, p in params.items()
    }


STUB_FUNCTIONS = sorted(_stub_signatures())


@pytest.mark.parametrize("name", STUB_FUNCTIONS)
def test_stub_matches_the_public_signature(name: str) -> None:
    # `__init__.py` passes every argument to the extension by keyword, and
    # stubtest only checks the stub against the extension. A default changed
    # in one of the three and not the others (#699) shows up here.
    assert _stub_signatures()[name] == _init_signature(name)


@pytest.mark.xfail(
    strict=True,
    raises=AttributeError,
    reason="#779 F3: core defaults not exposed",
)
@pytest.mark.parametrize("name", STUB_FUNCTIONS)
def test_defaults_match_the_core_defaults(name: str) -> None:
    # `tylertoo._defaults()` is built from the core `Default` impls (#779
    # F3): {function name: {keyword: default}}. Every keyword it names has
    # that default in `__init__.py` and in the stub.
    core: dict[str, dict[str, object]] = tylertoo._defaults()  # type: ignore[attr-defined]
    stub, public = _stub_signatures()[name], _init_signature(name)
    for kwarg, value in core.get(name, {}).items():
        assert public.get(kwarg) == value, f"{name}({kwarg}=...) in __init__.py"
        assert stub.get(kwarg) == value, f"{name}({kwarg}=...) in the stub"


# --- the ledger ------------------------------------------------------------------


def test_ledger_is_well_formed_and_pinned() -> None:
    seen = set()
    for e in ENTRIES:
        assert e.reason.strip(), f"{e} has no reason"
        assert e.issue > 0, f"{e} has no issue number"
        assert e.command in FUNCTIONS, f"{e} names an unknown command"
        key = (e.table, e.command, e.name)
        assert key not in seen, f"{e} is listed twice"
        seen.add(key)
    counts: dict[tuple[str, str], int] = {}
    for e in ENTRIES:
        kind = "xfail" if e.xfail else "exempt"
        counts[(e.table, kind)] = counts.get((e.table, kind), 0) + 1
    assert {k: counts.get(k, 0) for k in PINNED} == PINNED
    assert set(counts) <= set(PINNED), f"unpinned tables: {set(counts) - set(PINNED)}"


def test_exempt_entries_are_not_stale() -> None:
    # An exempt pair that has since gained its counterpart must leave the
    # ledger, or the next real difference hides behind it.
    stale = []
    for e in ENTRIES:
        if e.xfail:
            continue
        if e.table == "flag":
            known = {a.name for a in ARGS[e.command]}
            gone = e.name not in known or _kwarg_for(e.command, e.name) in _signature(
                e.command
            )
        elif e.table == "kwarg":
            gone = e.name not in _signature(e.command) or e.name in _flags_by_kwarg(
                e.command
            )
        else:
            gone = (e.command, e.name) not in _default_pairs() or _default_ok(
                e.command, e.name
            )
        if gone:
            stale.append(e)
    assert not stale, f"stale exemptions: {stale}"


def _default_ok(command: str, flag: str) -> bool:
    arg = next(a for a in ARGS[command] if a.name == flag)
    return _same_default(arg, _signature(command)[_kwarg_for(command, flag)].default)
