"""The tutorials: every step script is shown verbatim and runs as shown.

Each directory under ``examples/`` is one tutorial. Its ``README.md`` is the
page the docs site includes; its numbered ``0*.sh`` / ``0*.py`` files are
the steps. The tests run the steps in order in one scratch directory, then
check that the README shows each step verbatim, followed by output the
run reproduces.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest
from doc_blocks import (
    EXAMPLES_DIR,
    example_steps,
    expected_output,
    fence_for,
    missing_lines,
    stage_input,
)

EXAMPLES = sorted(p for p in EXAMPLES_DIR.glob("*/") if p.is_dir())
STEPS = [(ex, step) for ex in EXAMPLES for step in example_steps(ex)]


def _step_id(param: tuple[Path, Path]) -> str:
    example, step = param
    return f"{example.name}/{step.name}"


def test_examples_exist() -> None:
    """Both tutorials ship, and each has steps to run."""
    assert {p.name for p in EXAMPLES} >= {"cli-madagascar", "python-madagascar"}
    for example in EXAMPLES:
        assert (example / "README.md").is_file(), f"{example.name} has no README"
        assert example_steps(example), f"{example.name} has no steps"


@pytest.mark.parametrize("param", STEPS, ids=_step_id)
def test_tutorial_shows_step_verbatim(param: tuple[Path, Path]) -> None:
    """The prose cannot drift from the code CI runs."""
    example, step = param
    tutorial = (example / "README.md").read_text(encoding="utf-8")
    assert fence_for(step) in tutorial, (
        f"{example.name}/README.md does not show {step.name} verbatim"
    )


@pytest.fixture(scope="module")
def step_outputs(
    tmp_path_factory: pytest.TempPathFactory,
    madagascar: Path,
    doc_env: dict[str, str],
) -> dict[Path, str]:
    """Run every example's steps in order; map each step to its output."""
    outputs: dict[Path, str] = {}
    for example in EXAMPLES:
        workdir = tmp_path_factory.mktemp(example.name)
        stage_input(madagascar, workdir)
        for step in example_steps(example):
            runner = "bash" if step.suffix == ".sh" else sys.executable
            proc = subprocess.run(
                [runner, str(step)],
                cwd=workdir,
                env=doc_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
            assert proc.returncode == 0, (
                f"{example.name}/{step.name} exited {proc.returncode}:\n{proc.stdout}"
            )
            outputs[step] = proc.stdout
    return outputs


@pytest.mark.parametrize("param", STEPS, ids=_step_id)
def test_step_output_matches_tutorial(
    param: tuple[Path, Path], step_outputs: dict[Path, str]
) -> None:
    """The output a tutorial shows is the output the step prints."""
    example, step = param
    tutorial = (example / "README.md").read_text(encoding="utf-8")
    shown = expected_output(tutorial, step)
    if shown is None:
        pytest.skip(f"{step.name} shows no output block")
    missing = missing_lines(shown, step_outputs[step])
    assert not missing, (
        f"{example.name}/README.md shows output that {step.name} did not "
        f"print:\n" + "\n".join(missing) + f"\n\nActual output:\n{step_outputs[step]}"
    )
