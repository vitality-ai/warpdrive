"""
Adversarial inputs to WarpDial's placement interface, run against the
same live 9-node WarpDrive cluster (RS(6,3)) used for the Performance
section -- not a separate, smaller cluster, so the safety numbers and
the performance numbers come from the same configuration. For the
camera-ready revision: HiPC 2026 reviewers asked for the safety/
explainability claims to be backed by an experiment rather than
asserted.

This script covers the client/config-facing surface: a bad per-object
header (Case A) and a bad bucket configuration (Case B). The much larger
surface -- a misbehaving *implementation* of the StripePacker interface
itself, not a bad caller -- is covered by a fast, deterministic,
in-process simulation instead (server/src/cluster/packed.rs's
adversarial_packer_sim test module: dropped/duplicated/hallucinated
units, wrong bin count, and a capacity-lying packer, all run through the
real validation path with no live cluster needed). Case C here is the
one live, end-to-end confirmation that complements that simulation: the
same capacity-lying packer, registered for real (via the adversarial_demo
Cargo feature, never on by default) and PUT against over real HTTP.

Case A: a computable-units header that describes the whole object as one
unit, on a bucket with a tight (2%, the project's own default) overhead
threshold. Under RS(k=6,m=3), a single-unit pack always leaves k-1=5
bins empty, padding to the seed's own size -- a bound, not a guess (see
packing.rs's own comment on why this case always exceeds any sane
threshold). This is the shape of an adversarial or buggy computable-units
header: a client that lies about how to split the object, in the worst
possible direction.

Case B: a bucket configured with an unregistered packer_name -- the
shape of an adversarial or buggy bucket configuration rather than a bad
per-object header.

Case C: a bucket configured with the capacity_lying_demo packer (only
present when the server was built with --features adversarial_demo).
Packs units honestly but claims a capacity far smaller than it actually
uses. Before the validation fix added this session, this would have
PUT successfully and silently truncated the stored bytes; now it must
be rejected outright, with no object left reachable.

Control: a normal PUT with a sane header, same bucket/threshold --
must take the packed path, not fall back.

Usage: python3 adversarial_placement_test.py <cluster_base_url>
"""
import json
import sys
import urllib.error
import urllib.request

from real_workload_driver import configure_bucket_for_content_dependent_placement


def http_put_capture_headers(url: str, body: bytes, headers: dict) -> dict:
    req = urllib.request.Request(url, data=body, headers=headers, method="PUT")
    with urllib.request.urlopen(req, timeout=60) as resp:
        return {
            "status": resp.status,
            "x-warpd-placement": resp.headers.get("x-warpd-placement"),
            "x-warpd-pack-overhead-pct": resp.headers.get("x-warpd-pack-overhead-pct"),
        }


def http_put_expect_rejection(url: str, body: bytes, headers: dict) -> dict:
    """For a case that must fail the write outright (a validated-bad packer
    output), not fall back to another placement. Captures the rejection
    instead of treating it as an error in the test itself."""
    req = urllib.request.Request(url, data=body, headers=headers, method="PUT")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return {"rejected": False, "status": resp.status}
    except urllib.error.HTTPError as e:
        return {"rejected": True, "status": e.code, "body": e.read().decode(errors="replace")[:200]}


def http_get(url: str) -> bytes:
    with urllib.request.urlopen(url, timeout=60) as resp:
        return resp.read()


def case_control(cluster_url: str, bucket: str, body: bytes, k: int = 6) -> dict:
    # k equal-size units under k bins: FAC seeds bin 0 with the first unit,
    # then each remaining equal-size unit exactly fills one more empty bin
    # at capacity -- one full stripe, no padding waste, 0% overhead by
    # construction, not a cherry-picked number. This is the genuine
    # happy-path baseline the adversarial cases are contrasted against.
    # (With a unit count that doesn't divide evenly across k bins -- e.g.
    # the 9-unit/k=3 case this used before the cluster moved to RS(6,3) --
    # FAC's greedy packing leaves bins unevenly filled and this stops
    # being a clean 0% control.)
    n = len(body) // k
    units_header = json.dumps([[i * n, n] for i in range(k)])
    result = http_put_capture_headers(
        f"{cluster_url}/cluster/{bucket}/control-normal-units",
        body,
        {"x-warpd-computable-units": units_header, "Content-Type": "application/octet-stream"},
    )
    got = http_get(f"{cluster_url}/cluster/{bucket}/control-normal-units")
    result["round_trip_ok"] = got == body
    return result


def case_a_single_giant_unit(cluster_url: str, bucket: str, body: bytes) -> dict:
    units_header = json.dumps([[0, len(body)]])
    result = http_put_capture_headers(
        f"{cluster_url}/cluster/{bucket}/case-a-single-unit",
        body,
        {"x-warpd-computable-units": units_header, "Content-Type": "application/octet-stream"},
    )
    got = http_get(f"{cluster_url}/cluster/{bucket}/case-a-single-unit")
    result["round_trip_ok"] = got == body
    return result


def case_b_unknown_packer(cluster_url: str, bucket: str, body: bytes) -> dict:
    configure_bucket_for_content_dependent_placement(
        cluster_url, bucket, overhead_threshold_pct=2.0, packer_name="not_a_real_packer"
    )
    units_header = json.dumps([[0, 300], [300, 300], [600, 400]])
    result = http_put_capture_headers(
        f"{cluster_url}/cluster/{bucket}/case-b-unknown-packer",
        body,
        {"x-warpd-computable-units": units_header, "Content-Type": "application/octet-stream"},
    )
    got = http_get(f"{cluster_url}/cluster/{bucket}/case-b-unknown-packer")
    result["round_trip_ok"] = got == body
    return result


def case_c_capacity_lying_packer(cluster_url: str, bucket: str, body: bytes) -> dict:
    configure_bucket_for_content_dependent_placement(
        cluster_url, bucket, overhead_threshold_pct=2.0, packer_name="capacity_lying_demo"
    )
    n = len(body) // 9
    units_header = json.dumps([[i * n, n] for i in range(9)])
    key = "case-c-capacity-lying"
    result = http_put_expect_rejection(
        f"{cluster_url}/cluster/{bucket}/{key}",
        body,
        {"x-warpd-computable-units": units_header, "Content-Type": "application/octet-stream"},
    )
    # The write must have been rejected outright -- confirm nothing is
    # reachable under this key (not a corrupted object, not a stale one).
    try:
        http_get(f"{cluster_url}/cluster/{bucket}/{key}")
        result["object_reachable_after_rejection"] = True
    except urllib.error.HTTPError as e:
        result["object_reachable_after_rejection"] = False
        result["get_status_after_rejection"] = e.code
    return result


if __name__ == "__main__":
    cluster_url = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:9710"
    body = bytes([i % 251 for i in range(900)])

    bucket = "adversarialtest"
    configure_bucket_for_content_dependent_placement(cluster_url, bucket, overhead_threshold_pct=2.0, packer_name="fac")

    print("=== Control: sane computable-units header, 2% threshold, fac packer ===")
    print(json.dumps(case_control(cluster_url, bucket, body), indent=2))

    print("\n=== Case A: whole object declared as a single computable unit (adversarial/buggy header) ===")
    print(json.dumps(case_a_single_giant_unit(cluster_url, bucket, body), indent=2))

    print("\n=== Case B: bucket configured with an unregistered packer name (adversarial/buggy bucket config) ===")
    print(json.dumps(case_b_unknown_packer(cluster_url, "adversarialtest-caseb", body), indent=2))

    print("\n=== Case C: bucket configured with a capacity-lying packer (adversarial/buggy packer implementation) ===")
    print(json.dumps(case_c_capacity_lying_packer(cluster_url, "adversarialtest-casec", body), indent=2))
