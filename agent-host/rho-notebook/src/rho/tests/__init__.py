"""Tests of the rho package against a fake `_rho` (`fake_rho`).

Run from the package directory: `python3 -m unittest`. The package's
parent directory goes first on sys.path, so these test the source tree.
"""

import os
import sys

_SRC = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, _SRC)

from . import fake_rho  # noqa: E402

sys.modules["_rho"] = fake_rho
