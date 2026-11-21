"""RFC 5 review scenarios, run live on a local 5-node RS(3,2) cluster.

Usage: review_validate.py <warp_drive binary> <run dir> <label>

Node 0 is reached only through a proxy (its WARPDRIVE_SELF_ADDR and its
entry in WARPDRIVE_PEERS point at the proxy), so the T2 scenario can fail
exactly one request: the coordinator's own packed-record tombstone.
"""
import http.server, json, os, shutil, signal, socket, socketserver, subprocess, sys, threading, time
import boto3, requests
from botocore.config import Config

BIN, RUN, LABEL = sys.argv[1], sys.argv[2], sys.argv[3]
BASE = 20010
PROXY = 20110  # node0's advertised port; its gRPC/TCP ports are PROXY+1000/+2000, forwarded raw
N, K, M = 5, 3, 2
KEY, SECRET = "adminkey", "adminsecretkey123456"
ADDR = [f"http://127.0.0.1:{PROXY}"] + [f"http://127.0.0.1:{BASE + i}" for i in range(1, N)]
PEERS = ",".join(ADDR)
procs, results = {}, []
DROP = {"content_tombstone": False}


# ---------------------------------------------------------------- proxy
class Fwd(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _go(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else None
        if DROP["content_tombstone"] and self.command == "DELETE" and self.path == "/cluster/_internal/content_location":
            self.send_response(503)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        hdrs = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "content-length", "connection")}
        try:
            r = requests.request(self.command, f"http://127.0.0.1:{BASE}{self.path}", data=body, headers=hdrs, timeout=30, allow_redirects=False)
            data = r.content
            self.send_response(r.status_code)
            for k, v in r.headers.items():
                if k.lower() not in ("content-length", "transfer-encoding", "connection", "content-encoding"):
                    self.send_header(k, v)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except requests.RequestException:
            self.send_response(502)
            self.send_header("Content-Length", "0")
            self.end_headers()

    do_GET = do_PUT = do_POST = do_DELETE = do_HEAD = _go


class TServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def raw_forward(listen, target):
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", listen))
    srv.listen(256)

    def pipe(a, b):
        try:
            while True:
                d = a.recv(65536)
                if not d:
                    break
                b.sendall(d)
        except OSError:
            pass
        for s in (a, b):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def loop():
        while True:
            c, _ = srv.accept()
            try:
                t = socket.create_connection(("127.0.0.1", target))
            except OSError:
                c.close()
                continue
            threading.Thread(target=pipe, args=(c, t), daemon=True).start()
            threading.Thread(target=pipe, args=(t, c), daemon=True).start()

    threading.Thread(target=loop, daemon=True).start()


threading.Thread(target=TServer(("127.0.0.1", PROXY), Fwd).serve_forever, daemon=True).start()
raw_forward(PROXY + 1000, BASE + 1000)
raw_forward(PROXY + 2000, BASE + 2000)


# ---------------------------------------------------------------- cluster
def start(i):
    d = os.path.join(RUN, f"node{i}")
    os.makedirs(d, exist_ok=True)
    shutil.copy(os.path.join(os.path.dirname(BIN), "../../server_log.yaml"), d)
    env = dict(os.environ, WARPDRIVE_PORT=str(BASE + i), WARPDRIVE_SELF_ADDR=ADDR[i], WARPDRIVE_PEERS=PEERS,
               WARPDRIVE_RS_K=str(K), WARPDRIVE_RS_M=str(M), WARPDRIVE_ADMIN_ACCESS_KEY=KEY, WARPDRIVE_ADMIN_SECRET_KEY=SECRET,
               WARPDRIVE_INLINE_MAX_BYTES="65536", WARPDRIVE_RECLAIM_SWEEP_SECS="0")
    procs[i] = subprocess.Popen([BIN], cwd=d, env=env, stdout=open(os.path.join(d, "node.out"), "a"), stderr=subprocess.STDOUT)
    for _ in range(150):
        try:
            if requests.get(f"http://127.0.0.1:{BASE + i}/cluster/_internal/timing", timeout=0.5).ok:
                time.sleep(0.3)
                return
        except requests.RequestException:
            pass
        time.sleep(0.1)
    raise SystemExit(f"node{i} did not start")


def stop(i):
    if procs.get(i) and procs[i].poll() is None:
        procs[i].send_signal(signal.SIGTERM)
        procs[i].wait(15)


def url(i):
    return f"http://127.0.0.1:{BASE + i}"


def s3(i):
    return boto3.client("s3", endpoint_url=f"{url(i)}/cluster/s3", aws_access_key_id=KEY, aws_secret_access_key=SECRET,
                        region_name="us-east-1", config=Config(s3={"addressing_style": "path"}, retries={"max_attempts": 1}))


def s3_get(i, bucket, key):
    try:
        return s3(i).get_object(Bucket=bucket, Key=key)["Body"].read()
    except Exception as e:
        return f"error: {type(e).__name__}: {str(e)[:80]}"


def raw_get(i, bucket, key):
    try:
        r = requests.get(f"{url(i)}/cluster/{bucket}/{key}", timeout=30)
        return r.content if r.status_code == 200 else f"HTTP {r.status_code}"
    except requests.RequestException as e:
        return f"error: {e}"


def record(scenario, criterion, passed, actual):
    results.append((scenario, criterion, passed, actual))
    print(f"[{LABEL}] {'PASS' if passed else 'FAIL'}  {scenario}: {criterion}  -> {actual}")


def last_record(i, key):
    """The newest record line for `key` in node i's location log, in either log format."""
    line = None
    for l in open(os.path.join(RUN, f"node{i}", "cluster_location.log")):
        rk, e = json.loads(l)
        if rk.endswith(":" + key):
            line = e
    if isinstance(line, dict) and "Live" in line:
        return line["Live"]
    return line


def describe(bodies, expected):
    return ", ".join(f"node{i}: {'new' if b == expected[0] else ('old' if len(expected) > 1 and b == expected[1] else (b if isinstance(b, str) else f'{len(b)} bytes'))}"
                     for i, b in enumerate(bodies))


shutil.rmtree(RUN, ignore_errors=True)
for i in range(N):
    start(i)
try:
    B = "review"
    s3(1).create_bucket(Bucket=B)

    # ------------------------------------------------ healthy-path controls
    small, big = os.urandom(4096), os.urandom(1 << 20)
    s3(0).put_object(Bucket=B, Key="h-inline", Body=small)
    s3(0).put_object(Bucket=B, Key="h-sharded", Body=big)
    got = [(s3_get(i, B, "h-inline"), s3_get(i, B, "h-sharded")) for i in range(N)]
    ok = all(a == small and b == big for a, b in got)
    record("Healthy path", "inline and sharded objects read back exactly from all 5 nodes", ok,
           "all 5 exact" if ok else describe([a for a, _ in got], [small]) + " / " + describe([b for _, b in got], [big]))

    # ------------------------------------------------ T2 stale read after overwrite
    PB = "packedbkt"
    r = requests.put(f"{url(1)}/cluster/_admin/bucket_config/{PB}", json={"packer_name": "fac", "overhead_threshold_pct": 400.0})
    assert r.ok, r.text
    old = os.urandom(6 * 2000)
    units = json.dumps([[j * 2000, 2000] for j in range(6)])
    r = requests.put(f"{url(0)}/cluster/{PB}/t2key", data=old, headers={"x-warpd-computable-units": units})
    placement = r.headers.get("x-warpd-placement")
    assert r.ok and placement == "packed", (r.status_code, r.text, placement)
    new = os.urandom(300_000)
    DROP["content_tombstone"] = True
    r = requests.put(f"{url(0)}/cluster/{PB}/t2key", data=new, headers={"x-warpd-computable-units": "false"})
    DROP["content_tombstone"] = False
    put_status = r.status_code
    bodies = [raw_get(i, PB, "t2key") for i in range(N)]
    coord = bodies[0]
    if put_status == 200:
        record("T2 stale read", "PUT succeeded, so the coordinator returns the new bytes", coord == new,
               f"PUT {put_status}; " + describe(bodies, [new, old]))
    else:
        record("T2 stale read", "PUT succeeded, so the coordinator returns the new bytes", True,
               f"PUT failed ({put_status}), so no success was acknowledged; " + describe(bodies, [new, old]))

    # ------------------------------------------------ concurrent-write disagreement
    v1, v2 = os.urandom(200_000), os.urandom(200_000)
    s3(1).put_object(Bucket=B, Key="race", Body=v1)
    r1 = last_record(1, "race")
    s3(2).put_object(Bucket=B, Key="race", Body=v2)
    r2 = last_record(2, "race")
    # Deliver the two writes in opposite orders: nodes 0 and 1 see v2 then v1
    # (v1's message arrives late); nodes 2-4 already end on v2.
    late = [requests.post(f"{ADDR[i]}/cluster/_internal/location", json=r1).status_code for i in (0, 1)]
    bodies = [s3_get(i, B, "race") for i in range(N)]
    agree = len({b if isinstance(b, bytes) else b for b in bodies}) == 1
    record("Concurrent writes", "every node returns the same complete value", agree,
           f"late v1 delivery answered {late}; " + describe(bodies, [v2, v1]))
    for i in range(N):
        stop(i)
    for i in range(N):
        start(i)
    bodies2 = [s3_get(i, B, "race") for i in range(N)]
    agree2 = len(set(bodies2)) == 1
    record("Concurrent writes", "agreement holds after a full restart", agree2, describe(bodies2, [v2, v1]))

    # ------------------------------------------------ degraded-write durability
    stop(3)
    stop(4)
    di, ds = os.urandom(4096), os.urandom(1 << 20)
    pi = s3(0).put_object(Bucket=B, Key="d-inline", Body=di)["ResponseMetadata"]["HTTPStatusCode"]
    ps = s3(0).put_object(Bucket=B, Key="d-sharded", Body=ds)["ResponseMetadata"]["HTTPStatusCode"]
    stop(1)
    stop(2)
    start(3)
    start(4)
    gi = [s3_get(i, B, "d-inline") for i in (0, 3, 4)]
    gs = [s3_get(i, B, "d-sharded") for i in (0, 3, 4)]
    fmt = lambda xs, exp: ", ".join(f"node{n}: {'exact' if x == exp else x}" for n, x in zip((0, 3, 4), xs))
    record("Degraded write", "inline object acked with nodes 3,4 down survives losing nodes 1,2",
           any(x == di for x in gi), f"PUT {pi}; " + fmt(gi, di))
    record("Degraded write", "sharded object acked with nodes 3,4 down survives losing nodes 1,2",
           any(x == ds for x in gs), f"PUT {ps}; " + fmt(gs, ds))
finally:
    for i in list(procs):
        stop(i)

with open(os.path.join(os.path.dirname(RUN), f"review-{LABEL}.json"), "w") as f:
    json.dump(results, f, indent=1)
