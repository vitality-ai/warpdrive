# Warpdrive S3 demo

Warpdrive uses **one auth path**: Vitality Console + AWS SigV4.

### Important: bucket name in `test_user_auth.txt`

**Vitality Console** is the source of truth for bucket names. On each S3 request that targets a bucket (anything other than **ListBuckets** `GET /s3`), Warpdrive calls Console **`s3-credentials`** with the path bucket; Console returns **403** if that bucket is not registered for the key’s owner.

So **`bucket_name` in `test_user_auth.txt` must exist in the Console** (e.g. `default` or a bucket you created in the UI), or uploads will fail after authentication.

**ListBuckets** (`GET /s3`) does not send a bucket to Console and only lists buckets that already have metadata in Warpdrive for your user.

## Setup

1. **Warpdrive** (`server/.env`): set
   - `VITALITY_CONSOLE_URL` – e.g. `http://localhost:8000`
   - `WARPDRIVE_SERVICE_SECRET` – shared secret (must match Console)

2. **Vitality Console**: running with the same `WARPDRIVE_SERVICE_SECRET` in its `.env`.

3. **Credentials file**: create `demo/test_user_auth.txt` with an API key and a **bucket that exists in the Console**:
   ```
   access_key=<your access key from Console>
   secret_key=<your secret key from Console>
   bucket_name=<bucket from Console, e.g. default>
   user=<optional, for reference>
   ```

## Run

```bash
pip install boto3 requests
python3 s3_comprehensive_test.py
```

By default the script **deletes all uploaded objects** at the end. To leave them in the bucket (so they show in haystack and in Vitality Console storage usage), run with `--no-cleanup`:

```bash
python3 s3_comprehensive_test.py --no-cleanup
```

If you see **401 Unauthorized**: ensure Console is running, the credentials in `test_user_auth.txt` match a Console API key, and `WARPDRIVE_SERVICE_SECRET` matches in both `.env` files.

## Why don’t I see my user (e.g. mannat) in the haystack DB?

Rows with `user=testuser1` or `user=bucket_test_user` come from **Warpdrive’s Rust integration tests** (legacy `/put`/`/get` API with a `user` header). They are **not** from this Python demo.

This demo uses the **S3 API**. Uploads are stored under the **Console owner_id** (e.g. `mannat@gmail.com`), which Warpdrive gets from `POST /api/auth/s3-credentials` using the `access_key` from your request.

So:

1. **Run this demo** (with Console and Warpdrive running):  
   `python3 s3_comprehensive_test.py`  
   If you get 401, no row is written; fix Console + secret + API key and run again.

2. **Check the DB** after a successful run:  
   `sqlite3 metadata/metadata.sqlite "SELECT user, bucket, key FROM haystack WHERE user LIKE '%mannat%';"`  
   You should see `user=mannat@gmail.com` and `bucket=test` (or whatever `bucket_name` is in `test_user_auth.txt`).
