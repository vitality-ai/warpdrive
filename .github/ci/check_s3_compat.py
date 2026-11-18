#!/usr/bin/env python3
"""Gate the S3 compatibility job: every enabled test must pass.

s3-compat-enabled.txt lists the tests WarpDrive supports (one pytest node ID
per line). The job runs only those (see s3tests_enabled_only.py), and this
script decides red or green:
- red: an enabled test failed, errored, was skipped, or didn't run at all
  (for example a renamed test or a typo in the list)
- green: every enabled test passed

Usage:
  check_s3_compat.py results.xml
      gate a normal run
  check_s3_compat.py results.xml --discover
      report on a full-suite run: passing tests that aren't enabled yet, and
      enabled tests that don't pass. Never fails.
  check_s3_compat.py run1.xml run2.xml ... --write
      replace the enabled list with the tests that passed in every run given
"""
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

ENABLED_FILE = Path(__file__).parent / "s3-compat-enabled.txt"
HEADER = """\
# S3 compatibility tests WarpDrive supports. CI runs only these, and every
# one must pass. One pytest node ID per line. To enable a feature's tests,
# add their IDs here in the PR that implements it.
"""


def node_id(case):
    """junit classname s3tests.functional.test_s3 -> s3tests/functional/test_s3.py"""
    path = case.get("classname").replace(".", "/") + ".py"
    return f"{path}::{case.get('name')}"


def read_results(path):
    status = {}
    for case in ET.parse(path).getroot().iter("testcase"):
        if case.find("failure") is not None:
            status[node_id(case)] = "failed"
        elif case.find("error") is not None:
            status[node_id(case)] = "error"
        elif case.find("skipped") is not None:
            status[node_id(case)] = "skipped"
        else:
            status[node_id(case)] = "passed"
    return status


def read_enabled():
    ids = []
    for line in ENABLED_FILE.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            ids.append(line)
    return ids


def main():
    paths = [a for a in sys.argv[1:] if not a.startswith("--")] or ["results.xml"]
    runs = [read_results(p) for p in paths]

    if "--write" in sys.argv:
        passed_everywhere = set.intersection(
            *({t for t, s in run.items() if s == "passed"} for run in runs))
        ENABLED_FILE.write_text(HEADER + "".join(f"{t}\n" for t in sorted(passed_everywhere)))
        print(f"Wrote {len(passed_everywhere)} tests that passed in all {len(runs)} runs "
              f"to {ENABLED_FILE.name}")
        return

    status = runs[0]
    enabled = read_enabled()

    if "--discover" in sys.argv:
        enabled_set = set(enabled)
        ready = sorted(t for t, s in status.items() if s == "passed" and t not in enabled_set)
        broken = sorted(t for t in enabled if status.get(t) != "passed")
        print(f"{len(ready)} tests pass but aren't enabled yet:")
        for t in ready:
            print(f"  + {t}")
        print(f"\n{len(broken)} enabled tests don't pass:")
        for t in broken:
            print(f"  - {t} ({status.get(t, 'not run')})")
        return

    problems = [(t, status.get(t, "not run")) for t in enabled if status.get(t) != "passed"]
    passed = len(enabled) - len(problems)
    print(f"S3 compatibility suite: {passed}/{len(enabled)} enabled tests passed.")
    if problems:
        print(f"\nFAIL: {len(problems)} enabled tests didn't pass:")
        for t, s in problems:
            print(f"  - {t} ({s})")
        sys.exit(1)
    print("PASS.")


if __name__ == "__main__":
    main()
