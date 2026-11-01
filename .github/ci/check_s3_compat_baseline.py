#!/usr/bin/env python3
"""Gate the S3 compatibility test suite job on a regression, not on 100% pass.

Large, deliberately out-of-scope chunks of this suite are expected to
fail -- SSE, bucket policy, bucket logging, lifecycle, deprecated SigV2,
bucket ownership controls (see docs/TEST-COVERAGE.md for the full
breakdown). A 100%-pass gate would never be satisfiable and would hide
the one thing that actually matters here: whether something that used to
pass stopped passing.
"""
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

# Plain integer, one line -- the pass count below -- not the full test
# listing: a per-test allowlist would need updating every time a new gap
# gets fixed or a new one is found, and would silently stop catching
# anything not already on it. This only tracks the one number that
# matters: did something that used to pass stop passing.
BASELINE_FILE = Path(__file__).parent / "s3-compat-baseline.txt"


def main():
    results_path = sys.argv[1] if len(sys.argv) > 1 else "results.xml"
    root = ET.parse(results_path).getroot()
    suite = root if root.tag == "testsuite" else root.find("testsuite")

    total = int(suite.get("tests", 0))
    failures = int(suite.get("failures", 0))
    errors = int(suite.get("errors", 0))
    skipped = int(suite.get("skipped", 0))
    passed = total - failures - errors - skipped

    baseline = int(BASELINE_FILE.read_text().strip())
    print(f"S3 compatibility test suite: {passed} passed, {failures} failed, {errors} errors, "
          f"{skipped} skipped, {total} total")
    print(f"Baseline: {baseline} passed")

    if passed < baseline:
        print(f"REGRESSION: {baseline - passed} fewer tests pass than the recorded baseline.")
        sys.exit(1)
    if passed > baseline:
        print(f"{passed - baseline} more tests pass than the recorded baseline -- "
              f"update {BASELINE_FILE.name} to {passed}.")
    print("OK")


if __name__ == "__main__":
    main()
