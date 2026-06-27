# RFC 5: Warpdrive Distribution — APT Packaging & User Onboarding

**Status:** Draft  
**Date:** June 2026

---

## Overview

Warpdrive should be installable in one command on any Debian/Ubuntu machine.
The goal is zero friction from first download to first `s3.put_object` call —
no Docker, no cloud account, no configuration file required for a working
out-of-the-box setup.

This RFC covers:
1. How we build and publish a `.deb` package on every release
2. What the package installs (binary, systemd service, default config)
3. The user experience without the Vitality Console (direct S3 API use)
4. The user experience with the Vitality Console (managed multi-user setup)

---

## 1. Release Pipeline

### Trigger

Every git tag matching `v*` (e.g. `v0.3.1`) on the `main` branch triggers
the release workflow.

### Workflow: `.github/workflows/release.yml`

```
on:
  push:
    tags: ["v*"]

jobs:
  build-and-publish:
    runs-on: ubuntu-22.04
    steps:
      - checkout
      - install Rust stable toolchain
      - cargo build --release (warpdrive/server/)
      - cargo deb  →  produces warp_drive_<version>_amd64.deb
      - upload .deb to Fury.io APT repo via curl
      - create GitHub Release, attach .deb as asset
```

### Packaging tool

[`cargo-deb`](https://github.com/kornelski/cargo-deb) reads `[package.metadata.deb]`
in `Cargo.toml` and produces a standards-compliant `.deb`. We add:

```toml
[package.metadata.deb]
name = "warpdrive"
maintainer = "Vitality AI <hello@vitality.ai>"
license-file = ["LICENSE"]
extended-description = """\
Warpdrive is a self-hosted S3-compatible object storage server.
Drop-in replacement for AWS S3 — works with any S3 SDK out of the box."""
depends = "$auto"
section = "utils"
priority = "optional"
assets = [
  ["target/release/warp_drive", "usr/bin/warpdrive", "755"],
  ["packaging/warpdrive.service", "lib/systemd/system/warpdrive.service", "644"],
  ["packaging/warpdrive.env",     "etc/warpdrive/warpdrive.env",           "640"],
  ["packaging/warpdrive.yaml",    "etc/warpdrive/warpdrive.yaml",          "644"],
]
maintainer-scripts = "packaging/debian/"
systemd-units = { unit-name = "warpdrive", enable = true }
```

### APT repository

Hosted on **Fury.io** (free tier, no infrastructure to maintain).

```bash
# User setup — one time
curl -fsSL https://apt.fury.io/vitality-ai/gpg.key \
  | sudo gpg --dearmor -o /etc/apt/keyrings/vitality-ai.gpg
echo "deb [signed-by=/etc/apt/keyrings/vitality-ai.gpg] \
  https://apt.fury.io/vitality-ai/ /" \
  | sudo tee /etc/apt/sources.list.d/warpdrive.list
sudo apt update
sudo apt install warpdrive
```

The CI upload step:

```bash
curl -F package=@warpdrive_*.deb \
     https://${{ secrets.FURY_TOKEN }}@push.fury.io/vitality-ai/
```

---

## 2. What the Package Installs

| Path | Description |
|---|---|
| `/usr/bin/warpdrive` | The server binary |
| `/lib/systemd/system/warpdrive.service` | systemd unit (enabled on install) |
| `/etc/warpdrive/warpdrive.env` | Environment file (credentials, port) |
| `/etc/warpdrive/warpdrive.yaml` | Logging config |
| `/var/lib/warpdrive/` | Default data directory (metadata DB + blobs) |
| `/var/log/warpdrive/` | Log directory |

### Default `warpdrive.env`

```bash
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey
WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456
WARPDRIVE_PORT=9710
WARPDRIVE_DATA_DIR=/var/lib/warpdrive
```

The file is installed with mode `640`, owned by `root:warpdrive`, so the
service can read it but other users cannot. On first install the postinst
script creates the `warpdrive` system user and sets ownership of data and
log directories.

### Default `warpdrive.service`

```ini
[Unit]
Description=Warpdrive S3-compatible object storage
After=network.target

[Service]
Type=simple
User=warpdrive
Group=warpdrive
EnvironmentFile=/etc/warpdrive/warpdrive.env
ExecStart=/usr/bin/warpdrive
Restart=on-failure
RestartSec=5
WorkingDirectory=/var/lib/warpdrive

[Install]
WantedBy=multi-user.target
```

The service starts automatically on install and on boot.

---

## 3. Using Warpdrive Without the Console

This is the "just store stuff" path — no UI, no user management.
Everything is done through the S3 API using the default admin credentials.

### Installation

```bash
curl -fsSL https://apt.fury.io/vitality-ai/gpg.key \
  | sudo gpg --dearmor -o /etc/apt/keyrings/vitality-ai.gpg
echo "deb [signed-by=/etc/apt/keyrings/vitality-ai.gpg] \
  https://apt.fury.io/vitality-ai/ /" \
  | sudo tee /etc/apt/sources.list.d/warpdrive.list
sudo apt update && sudo apt install warpdrive
```

Warpdrive is now running on port 9710.

```bash
sudo systemctl status warpdrive   # verify
```

### Connecting with boto3

```python
import boto3

s3 = boto3.client(
    "s3",
    endpoint_url="http://localhost:9710",
    aws_access_key_id="adminkey",
    aws_secret_access_key="adminsecretkey123456",
    region_name="us-east-1",       # any value works
)
```

### Basic operations

```python
# Create a bucket
s3.create_bucket(Bucket="my-bucket")

# Upload an object
s3.put_object(Bucket="my-bucket", Key="hello.txt", Body=b"hello world")

# Download
response = s3.get_object(Bucket="my-bucket", Key="hello.txt")
print(response["Body"].read())  # b"hello world"

# List objects
for obj in s3.list_objects_v2(Bucket="my-bucket")["Contents"]:
    print(obj["Key"], obj["Size"])

# Delete
s3.delete_object(Bucket="my-bucket", Key="hello.txt")
```

### AWS CLI

```bash
export AWS_ACCESS_KEY_ID=adminkey
export AWS_SECRET_ACCESS_KEY=adminsecretkey123456
export AWS_DEFAULT_REGION=us-east-1

alias s3local="aws s3 --endpoint-url http://localhost:9710"

s3local mb s3://my-bucket
s3local cp ./file.txt s3://my-bucket/file.txt
s3local ls s3://my-bucket/
```

### Changing the admin credentials

Edit `/etc/warpdrive/warpdrive.env` and restart:

```bash
sudo nano /etc/warpdrive/warpdrive.env
# change WARPDRIVE_ADMIN_ACCESS_KEY and WARPDRIVE_ADMIN_SECRET_KEY

sudo systemctl restart warpdrive
```

### Changing the port

Set `WARPDRIVE_PORT` in `/etc/warpdrive/warpdrive.env` and restart.

### Data directory

Metadata and blobs are stored in `/var/lib/warpdrive/`. Back this directory
up to retain all data. To move it, set `WARPDRIVE_DATA_DIR` in the env file.

---

## 4. Using Warpdrive With the Vitality Console

The Console is the management UI for Warpdrive. It adds:
- Browser-based bucket browser (upload, download, delete, preview)
- Sub-user management (create per-app access keys with scoped permissions)
- Usage dashboard (storage consumed per bucket, request rates)
- Bucket configuration UI (versioning, lifecycle rules, object lock)

Warpdrive itself does not change — the Console talks to it over the same
S3 API that your application code uses.

### Installation order

Install Warpdrive first (see Section 3), then install the Console:

```bash
sudo apt install warpdrive vitality-console
```

The Console is a separate package that installs a web server on port **8080**
and connects to Warpdrive at `http://localhost:9710` by default.

### First login

Open `http://localhost:8080` in a browser.

Log in with the default admin credentials:

| Field | Value |
|---|---|
| Access Key | `adminkey` |
| Secret Key | `adminsecretkey123456` |

Change these immediately via **Settings → Admin Credentials** after first login.

### Creating sub-users (per-application keys)

Navigate to **Users → Create User**. Assign a name and the Console generates
an access key / secret key pair. The user inherits the permissions you set
(bucket-level read/write scoping is supported).

Application code then uses the sub-user's keys instead of the admin keys:

```python
s3 = boto3.client(
    "s3",
    endpoint_url="http://localhost:9710",
    aws_access_key_id="app1-accesskey",
    aws_secret_access_key="app1-secretkey",
    region_name="us-east-1",
)
```

This means the admin keys never leave the server — each application only
holds credentials scoped to what it needs.

### Remote access

By default Warpdrive binds to `0.0.0.0:9710`, so it is reachable from other
machines on the same network. For production use:

- Put Nginx or Caddy in front of Warpdrive on port 443 (TLS termination)
- Restrict `WARPDRIVE_PORT` to a non-public interface if the Console and
  application code run on the same host
- The Console can be configured with a remote Warpdrive endpoint via
  `/etc/vitality-console/console.env` (`WARPDRIVE_ENDPOINT=http://10.0.0.5:9710`)

---

## 5. Release Checklist

Before tagging a release:

1. All Ceph S3 tests for the batch pass locally
2. `TEST-COVERAGE.md` updated with newly-passing tests and running total
3. `Cargo.toml` version bumped (follows semver)
4. Tag pushed: `git tag v0.X.Y && git push origin v0.X.Y`
5. CI builds `.deb`, publishes to Fury, creates GitHub Release automatically

---

## 6. Open Questions

- **Fury.io free tier limit**: 250 MB storage, 10k downloads/month. If
  download volume exceeds this, migrate to self-hosted (S3 bucket +
  CloudFront) or a paid Fury plan.
- **ARM64 builds**: GitHub Actions `ubuntu-22.04` is x86_64. A separate
  `ubuntu-22.04-arm` runner (or cross-compilation via `cross`) is needed
  for Raspberry Pi / Ampere installs. Defer until there is demand.
- **`/etc/warpdrive/warpdrive.env` default credentials**: Shipping
  known-default admin credentials is a deliberate developer-experience
  tradeoff. The postinst script should print a prominent warning if the
  server is reachable on a non-loopback interface and the credentials have
  not been changed.
