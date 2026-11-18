"""pytest plugin for the S3 compatibility job: run only the enabled tests.

s3-compat-enabled.txt lists the tests WarpDrive supports, one pytest node ID
per line (`#` starts a comment). Everything else is deselected, so the job
runs exactly the supported set. A feature PR enables its tests by adding
their IDs to the file.

Loaded with `-p s3tests_enabled_only` and this directory on PYTHONPATH.
Set S3COMPAT_RUN_ALL=1 to skip the filter (the discovery run).
"""
import os
from pathlib import Path

ENABLED_FILE = Path(__file__).parent / "s3-compat-enabled.txt"


def read_enabled():
    ids = set()
    for line in ENABLED_FILE.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            ids.add(line)
    return ids


def pytest_collection_modifyitems(config, items):
    if os.environ.get("S3COMPAT_RUN_ALL") == "1":
        return
    enabled = read_enabled()
    keep, drop = [], []
    for item in items:
        (keep if item.nodeid in enabled else drop).append(item)
    if drop:
        config.hook.pytest_deselected(items=drop)
    items[:] = keep
