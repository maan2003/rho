"""Pinned ghapi 2.1.5 REST metadata plus Octo's review-decision endpoint (Apache-2.0)."""
import json
from pathlib import Path

spec = json.loads(Path(__file__).with_suffix(".json").read_text(encoding="utf-8"))
