"""Make scripts/ci importable for the gate-script tests (#779)."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
