# Developer's Guide

## Local Setup

---

### 1. Prerequisites

- **Rust** — [install via rustup](https://rustup.rs/):
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- **libsqlite3-dev** and **FlatBuffers compiler (`flatc`)**

  Ubuntu/Debian:
  ```bash
  sudo apt-get update && sudo apt-get install -y libsqlite3-dev flatbuffers-compiler
  ```

  macOS (Homebrew):
  ```bash
  brew install sqlite3 flatbuffers
  ```

---

### 2. Clone the Repository

```bash
git clone https://github.com/vitality-ai/warpdrive.git
cd warpdrive
```

---

### 3. Build

```bash
cd warpdrive/server
cargo build --release
```

---

### 4. Run

Warpdrive must be started from the `server/` directory (it looks for `server_log.yaml` and needs writable `logs/` and `storage/` subdirectories).

```bash
cd warpdrive/server
WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  ./target/release/warp_drive
```

Listens on **port 9710**. Logs go to `logs/` and object data to `storage/`.

Replace the key values with credentials of your choice — these become the admin access key and secret used for S3 SigV4 authentication.

---

### 5. Verify with boto3

```bash
pip install boto3
```

```python
import boto3

s3 = boto3.client(
    's3',
    endpoint_url='http://localhost:9710',
    aws_access_key_id='adminkey',
    aws_secret_access_key='adminsecretkey123456',
    region_name='us-east-1',
    config=boto3.session.Config(s3={'addressing_style': 'path'}),
)

s3.create_bucket(Bucket='test-bucket')
s3.put_object(Bucket='test-bucket', Key='hello.txt', Body=b'Hello, Warpdrive!')
print(s3.get_object(Bucket='test-bucket', Key='hello.txt')['Body'].read())
```

---

## Docker (Optional)

**Build:**
```bash
docker build -t warpdrive .
```

**Run:**
```bash
docker run -p 9710:9710 \
  -e WARPDRIVE_ADMIN_ACCESS_KEY=adminkey \
  -e WARPDRIVE_ADMIN_SECRET_KEY=adminsecretkey123456 \
  warpdrive
```

---

## Troubleshooting

- Ensure all prerequisites are installed and available in your PATH.
- For advanced help, see [Rust docs](https://doc.rust-lang.org/book/) or [FlatBuffers docs](https://google.github.io/flatbuffers/).
