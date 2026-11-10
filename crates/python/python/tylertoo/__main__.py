"""The tylertoo CLI: `python -m tylertoo` and the `tylertoo` script.

Both run the same code as the `tylertoo` binary, with the same output,
logging, and exit status (see `tylertoo --help`).
"""

import signal
import sys

from tylertoo._tylertoo import _run_cli


def main() -> None:
    """Run the CLI on `sys.argv` and exit with its status."""
    # The run never returns to Python to act on a signal, so Ctrl-C gets
    # the default action, as for the binary. SIGTERM already has it.
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    sys.exit(_run_cli(["tylertoo", *sys.argv[1:]]))


if __name__ == "__main__":
    main()
