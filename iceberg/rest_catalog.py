#!/usr/bin/env python3
"""Iceberg REST Catalog served on top of WarpDrive (Layer 2).

A standalone REST catalog (the shape used by Lakekeeper / Polaris / Nessie /
the iceberg-rest-fixture) whose authoritative state lives ENTIRELY in WarpDrive
-- no external catalog DB:

  * namespace markers   -> s3://<wh>/__catalog__/namespaces/<ns>.json
  * table pointers      -> s3://<wh>/__catalog__/tables/<ns>/<tbl>.json
                           = {"metadata_location": "...", "version": N}
  * table metadata.json -> standard Iceberg layout under the table location

Commit atomicity is a compare-and-swap on the table-pointer object using
WarpDrive's S3 conditional writes:
  * create : PUT pointer  If-None-Match: *     (409 if it already exists)
  * commit : PUT pointer  If-Match: <etag>     (409 if another commit won)
This mirrors SqlCatalog's `UPDATE ... WHERE metadata_location = <current>` CAS,
but the "row" is an object in WarpDrive and the lock is the 412 the object store
returns -- so the catalog needs no database of its own.

Iceberg metadata semantics (building initial metadata, validating a commit's
requirements, applying its updates, serializing metadata.json) are reused from
pyiceberg itself, so this file is only protocol + storage + atomicity.

Run:  WD_S3=http://127.0.0.1:9760 venv/bin/python rest_catalog.py  (listens :8181)
Point a client at it with uri="http://127.0.0.1:8181".
"""
import json, os, sys, uuid, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote

import boto3
from botocore.config import Config
from botocore.exceptions import ClientError

from pyiceberg.partitioning import UNPARTITIONED_PARTITION_SPEC
from pyiceberg.table.sorting import UNSORTED_SORT_ORDER
from pyiceberg.table.metadata import new_table_metadata
from pyiceberg.table.update import update_table_metadata
from pyiceberg.serializers import FromInputFile, ToOutputFile
from pyiceberg.io.fsspec import FsspecFileIO
from pyiceberg.catalog.rest import CreateTableRequest, CommitTableRequest, TableResponse
from pyiceberg.exceptions import CommitFailedException

# ---- WarpDrive connection (server-side; the catalog is the only S3 writer of
# ---- its own pointer/namespace objects) -------------------------------------
WD_S3     = os.environ.get("WD_S3", "http://127.0.0.1:9760")
WD_KEY    = os.environ.get("WD_KEY", "adminkey")
WD_SECRET = os.environ.get("WD_SECRET", "adminsecret")
WH_BUCKET = os.environ.get("WD_WAREHOUSE_BUCKET", "warehouse")
WAREHOUSE = f"s3://{WH_BUCKET}"
LISTEN    = int(os.environ.get("REST_PORT", "8181"))

# S3 settings handed to CLIENTS via /v1/config so their FileIO targets WarpDrive
# with no client-side wiring -- the catalog drives it, the REST way.
S3_CLIENT_CONF = {
    "s3.endpoint": WD_S3, "s3.access-key-id": WD_KEY, "s3.secret-access-key": WD_SECRET,
    "s3.region": "us-east-1", "s3.path-style-access": "true",
}

_s3 = boto3.client("s3", endpoint_url=WD_S3, aws_access_key_id=WD_KEY,
                   aws_secret_access_key=WD_SECRET, region_name="us-east-1",
                   config=Config(s3={"addressing_style": "path"}))
_io = FsspecFileIO(properties=S3_CLIENT_CONF)
_lock = threading.Lock()  # serialize same-process metadata work; cross-process
                          # safety still rests on the WarpDrive CAS below.


# ---- low-level pointer storage with conditional writes ----------------------
def _cond_put(key, body, if_match=None, if_none_match=None):
    evt = "before-sign.s3.PutObject"
    def _inject(request, **kw):
        if if_match is not None:      request.headers["If-Match"] = if_match
        if if_none_match is not None: request.headers["If-None-Match"] = if_none_match
    _s3.meta.events.register(evt, _inject)
    try:
        return _s3.put_object(Bucket=WH_BUCKET, Key=key, Body=body)
    finally:
        _s3.meta.events.unregister(evt, _inject)

def _get(key):
    """Return (json_body, etag) or (None, None) if absent."""
    try:
        r = _s3.get_object(Bucket=WH_BUCKET, Key=key)
        return json.loads(r["Body"].read()), r["ETag"]
    except ClientError as e:
        if e.response["Error"].get("Code") in ("NoSuchKey", "404"):
            return None, None
        raise

def _is_precondition(e):
    return isinstance(e, ClientError) and (
        e.response["Error"].get("Code") == "PreconditionFailed"
        or e.response["ResponseMetadata"].get("HTTPStatusCode") == 412)


# ---- key helpers ------------------------------------------------------------
def _ns_key(ns):    return f"__catalog__/namespaces/{ns}.json"
def _tbl_key(ns, t): return f"__catalog__/tables/{ns}/{t}.json"
def _tbl_location(ns, t): return f"{WAREHOUSE}/{ns.replace(chr(0x1f), '/')}/{t}"
def _meta_path(location, version): return f"{location}/metadata/{version:05d}-{uuid.uuid4()}.metadata.json"
def _ns_str(ns_list): return chr(0x1f).join(ns_list)


# ---- Iceberg operations -----------------------------------------------------
def load_table_response(ns, t):
    ptr, _etag = _get(_tbl_key(ns, t))
    if ptr is None:
        return None
    loc = ptr["metadata_location"]
    md = FromInputFile.table_metadata(_io.new_input(loc))
    return TableResponse(metadata=md, metadata_location=loc, config=S3_CLIENT_CONF)

def create_table(ns, body):
    raw = json.loads(body)
    t = raw["name"]
    # The client leaves `location` to the catalog; this pyiceberg's
    # CreateTableRequest requires it, so assign it before validating (reusing
    # the model's schema / partition-spec / sort-order parsing).
    location = raw.get("location") or _tbl_location(ns, t)
    raw["location"] = location
    req = CreateTableRequest.model_validate(raw)
    md = new_table_metadata(
        schema=req.table_schema,
        partition_spec=req.partition_spec or UNPARTITIONED_PARTITION_SPEC,
        sort_order=req.write_order or UNSORTED_SORT_ORDER,
        location=location,
        properties=req.properties or {},
    )
    meta_loc = _meta_path(location, 0)
    if not req.stage_create:
        ToOutputFile.table_metadata(md, _io.new_output(meta_loc), overwrite=True)
        try:
            _cond_put(_tbl_key(ns, t), json.dumps({"metadata_location": meta_loc, "version": 0}),
                      if_none_match="*")
        except ClientError as e:
            if _is_precondition(e):
                return ("already_exists", None)
            raise
    return ("ok", TableResponse(metadata=md, metadata_location=meta_loc, config=S3_CLIENT_CONF))

def commit_table(ns, t, body):
    # The table identifier is in the URL path; pyiceberg also repeats it in the
    # body but Spark/Trino's REST client does not, and this pyiceberg's
    # CommitTableRequest marks it required -- so inject it from the path.
    raw = json.loads(body)
    raw.setdefault("identifier", {"namespace": ns.split(chr(0x1f)), "name": t})
    req = CommitTableRequest.model_validate(raw)
    with _lock:
        ptr, etag = _get(_tbl_key(ns, t))
        current_md = None
        if ptr is not None:
            current_md = FromInputFile.table_metadata(_io.new_input(ptr["metadata_location"]))
        # validate requirements against the CURRENT metadata (AssertCreate,
        # AssertRefSnapshotId, assert-table-uuid, ...). Any failure -> 409.
        for r in req.requirements:
            r.validate(current_md)
        if current_md is None:
            # A commit against a non-existent pointer is a staged-create
            # transaction (create_table_transaction). Not needed by the
            # evaluated path (plain create_table + append), so reject clearly
            # rather than reconstruct base metadata from the update list.
            raise CommitFailedException(f"Table {ns}.{t} does not exist (staged create not supported)")
        new_md = update_table_metadata(current_md, req.updates)
        if current_md is not None and new_md == current_md:
            return ("ok", TableResponse(metadata=current_md,
                                        metadata_location=ptr["metadata_location"], config=S3_CLIENT_CONF))
        location = new_md.location
        version = (ptr["version"] + 1) if ptr else 0
        meta_loc = _meta_path(location, version)
        ToOutputFile.table_metadata(new_md, _io.new_output(meta_loc), overwrite=True)
        # CAS the pointer: create (If-None-Match:*) or swap (If-Match:<etag>).
        try:
            if ptr is None:
                _cond_put(_tbl_key(ns, t), json.dumps({"metadata_location": meta_loc, "version": 0}),
                          if_none_match="*")
            else:
                _cond_put(_tbl_key(ns, t), json.dumps({"metadata_location": meta_loc, "version": version}),
                          if_match=etag)
        except ClientError as e:
            if _is_precondition(e):
                raise CommitFailedException(f"Table {ns}.{t} updated concurrently")
            raise
        return ("ok", TableResponse(metadata=new_md, metadata_location=meta_loc, config=S3_CLIENT_CONF))


# ---- HTTP routing -----------------------------------------------------------
class Handler(BaseHTTPRequestHandler):
    # Speak HTTP/1.1 so JVM clients (Spark/Trino via Apache HttpClient 5) can
    # reuse keep-alive connections; with HTTP/1.0 the socket closes after each
    # response and a reused connection sees "failed to respond". Every response
    # carries an explicit Content-Length (see _send), required for keep-alive.
    protocol_version = "HTTP/1.1"

    def log_message(self, *a): pass

    def handle_one_request(self):
        # Never let a handler exception silently drop the connection (which a
        # JVM client reports as "failed to respond"): map it to a 500 so the
        # client gets a real error it can surface.
        try:
            super().handle_one_request()
        except Exception as e:  # noqa: BLE001
            try:
                self._err(500, "InternalError", f"{type(e).__name__}: {e}")
            except Exception:
                pass

    def _send(self, code, obj=None):
        data = b"" if obj is None else (obj if isinstance(obj, (bytes, bytearray)) else json.dumps(obj).encode())
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if data:
            self.wfile.write(data)

    def _err(self, code, typ, msg):
        self._send(code, {"error": {"message": msg, "type": typ, "code": code}})

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    # path after optional /v1
    def _parts(self):
        p = self.path.split("?", 1)[0].strip("/").split("/")
        if p and p[0] == "v1":
            p = p[1:]
        return [unquote(x) for x in p if x != ""]

    def do_GET(self):
        p = self._parts()
        if p == ["config"]:
            return self._send(200, {"defaults": {}, "overrides": S3_CLIENT_CONF})
        if p == ["namespaces"]:
            res = _s3.list_objects_v2(Bucket=WH_BUCKET, Prefix="__catalog__/namespaces/")
            nss = [[o["Key"].split("/")[-1][:-5]] for o in res.get("Contents", [])]
            return self._send(200, {"namespaces": nss})
        if len(p) == 2 and p[0] == "namespaces":              # load namespace metadata
            obj, _ = _get(_ns_key(p[1]))
            if obj is None:
                return self._err(404, "NoSuchNamespaceException", f"Namespace does not exist: {p[1]}")
            return self._send(200, {"namespace": p[1].split(chr(0x1f)), "properties": obj.get("properties", {})})
        if len(p) == 3 and p[0] == "namespaces" and p[2] == "tables":  # list tables
            res = _s3.list_objects_v2(Bucket=WH_BUCKET, Prefix=f"__catalog__/tables/{p[1]}/")
            ids = [{"namespace": p[1].split(chr(0x1f)), "name": o["Key"].split("/")[-1][:-5]}
                   for o in res.get("Contents", [])]
            return self._send(200, {"identifiers": ids})
        if len(p) == 4 and p[0] == "namespaces" and p[2] == "tables":  # load table
            tr = load_table_response(p[1], p[3])
            if tr is None:
                return self._err(404, "NoSuchTableException", f"Table does not exist: {p[1]}.{p[3]}")
            return self._send(200, tr.model_dump_json(by_alias=True, exclude_none=True).encode())
        return self._err(404, "NotFound", f"GET {self.path}")

    def do_HEAD(self):
        p = self._parts()
        if len(p) == 2 and p[0] == "namespaces":
            obj, _ = _get(_ns_key(p[1]))
            return self._send(204 if obj is not None else 404)
        if len(p) == 4 and p[0] == "namespaces" and p[2] == "tables":
            ptr, _ = _get(_tbl_key(p[1], p[3]))
            return self._send(204 if ptr is not None else 404)
        return self._send(404)

    def do_POST(self):
        p = self._parts()
        body = self._body()
        if p == ["namespaces"]:                                # create namespace
            req = json.loads(body)
            ns = _ns_str(req["namespace"])
            try:
                _cond_put(_ns_key(ns), json.dumps({"properties": req.get("properties", {})}), if_none_match="*")
            except ClientError as e:
                if _is_precondition(e):
                    return self._err(409, "AlreadyExistsException", f"Namespace already exists: {ns}")
                raise
            return self._send(200, {"namespace": req["namespace"], "properties": req.get("properties", {})})
        if len(p) == 3 and p[0] == "namespaces" and p[2] == "tables":   # create table
            status, tr = create_table(p[1], body)
            if status == "already_exists":
                return self._err(409, "AlreadyExistsException", f"Table already exists in {p[1]}")
            return self._send(200, tr.model_dump_json(by_alias=True, exclude_none=True).encode())
        if len(p) == 4 and p[0] == "namespaces" and p[2] == "tables":   # commit (update table)
            try:
                status, tr = commit_table(p[1], p[3], body)
            except CommitFailedException as e:
                return self._err(409, "CommitFailedException", str(e))
            return self._send(200, tr.model_dump_json(by_alias=True, exclude_none=True).encode())
        return self._err(404, "NotFound", f"POST {self.path}")

    def do_DELETE(self):
        p = self._parts()
        if len(p) == 2 and p[0] == "namespaces":
            try: _s3.delete_object(Bucket=WH_BUCKET, Key=_ns_key(p[1]))
            except ClientError: pass
            return self._send(204)
        if len(p) == 4 and p[0] == "namespaces" and p[2] == "tables":
            try: _s3.delete_object(Bucket=WH_BUCKET, Key=_tbl_key(p[1], p[3]))
            except ClientError: pass
            return self._send(204)
        return self._send(404)


if __name__ == "__main__":
    # warehouse bucket must exist
    try: _s3.create_bucket(Bucket=WH_BUCKET)
    except ClientError: pass
    srv = ThreadingHTTPServer(("127.0.0.1", LISTEN), Handler)
    print(f"Iceberg REST catalog on http://127.0.0.1:{LISTEN}  warehouse={WAREHOUSE}  WarpDrive={WD_S3}", flush=True)
    srv.serve_forever()
