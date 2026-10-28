# Remote backups

Nenia pushes immutable compressed objects and self-contained snapshot manifests.
It uploads every new pack before publishing any new manifest. Pull follows the same
order, then the CLI reconstructs the local catalog. SQLite, local configuration,
credentials, lock files, and unfinished temporary files are never copied.

## Self-hosted server

Run the server with a persistent data directory and a random bearer token of at
least 24 characters. Terminate HTTPS at your reverse proxy; the built-in server
listens on HTTP.

```sh
export NENIA_TOKEN='your-long-random-token'
nenia --vault /srv/nenia serve --listen 127.0.0.1:7433
```

On each laptop or workstation:

```sh
export NENIA_TOKEN='your-long-random-token'
nenia connect https://traces.example.com
nenia backup
nenia watch
```

`connect` saves the connection in the local configuration; `NENIA_TOKEN` overrides
the saved token. Each client retains its local spool when the server is offline.
Run `backup` or `push` again to resume, or use `watch` for repeated attempts. There
is no server-side delete operation in the backup protocol. This is a single-tenant
service: clients sharing the token can read all archived sessions.

The client supports a base path such as `https://example.com/nenia/`, appending
`v1/objects`, `v1/packs/<hash>`, and `v1/manifests/<hash>`. Configure your reverse
proxy to strip that base path. Redirects are rejected rather than forwarded with
credentials. HTTP is accepted when explicitly requested for localhost or trusted
private transport; use HTTPS over public networks. Archives have no client-side
encryption: the server and object-store account can read their contents.

## File and S3 destinations

A second disk or mounted filesystem requires no server:

```sh
nenia push --remote file:///mnt/backup/nenia
nenia --vault /tmp/recovered pull --remote file:///mnt/backup/nenia
```

On Windows use a file URL such as `file:///D:/backups/nenia`. New objects are
written to temporary files, flushed, and published without replacing existing
files. On Unix the parent directory is also flushed. Archive subdirectories and
addressed objects must not be symlinks. Durability depends on the underlying disk
and filesystem; this transport does not add replication.

On Unix a completed sync also flushes the pack and manifest directories, including
when all objects were already present, and then the archive root. A retry therefore
repeats directory flushes that may have failed after an earlier file publication.

S3 uses the Rust `object_store` AWS backend. Create the bucket first, supply
credentials through the environment or supported AWS credential providers, and
grant list, get, and put access to the archive prefix. Nenia does not require delete
access or bucket administration.

```sh
export AWS_ACCESS_KEY_ID='your-access-key'
export AWS_SECRET_ACCESS_KEY='your-secret-key'
export AWS_REGION='us-east-1'
nenia push --remote s3://my-traces/nenia
nenia --vault /tmp/recovered pull --remote s3://my-traces/nenia
```

For Cloudflare R2, configure `AWS_ENDPOINT` to the account's S3 endpoint and set
`AWS_REGION=auto`, then use the same `s3://bucket/prefix` URL. R2 documents these
settings in its [S3 setup guide](https://developers.cloudflare.com/r2/get-started/s3/).
For example:

```sh
export AWS_ENDPOINT='https://ACCOUNT_ID.r2.cloudflarestorage.com'
export AWS_REGION='auto'
nenia push --remote s3://my-traces/nenia
```

`AWS_SESSION_TOKEN` supports temporary credentials. Set `AWS_ALLOW_HTTP=true`
only for an explicitly selected HTTP S3 endpoint, such as a localhost emulator.
See the backend's [AWS configuration](https://docs.rs/object_store/0.12.5/object_store/aws/struct.AmazonS3Builder.html).
S3 prefixes support ASCII letters, digits, `/`, `_`, `-`, and `.`, with no traversal
segments or percent-encoded paths. Conditional create must be supported by the
provider; Nenia does not fall back to overwriting existing objects.

## Fast repeats and integrity checks

Normal CLI backup trusts existing destination object names from one inventory.
It skips their local payloads and remote downloads, uploads only missing objects,
and reports how many existing objects were actually hash-verified. This avoids
reading years of old data on each backup. It does **not** audit those old bytes:
same-length corruption at an existing name can go undetected.

Run an integrity pass when you want that audit:

```sh
nenia push --verify-existing
nenia verify
```

`push --verify-existing` reads and hashes both the existing local objects and their
remote counterparts. This adds remote GET requests and download traffic. `verify`
checks local archive contents, including decompression and referenced chunk data.
The CLI always verifies existing local objects on pull. The library's
`SyncOptions::default()` also verifies existing objects; callers must explicitly
disable that option to use the CLI's fast upload behavior.

Newly uploaded and downloaded objects always undergo BLAKE3 verification. New
manifests are decoded with bounds and must reference packs present in the source
inventory. Transfers never delete or overwrite a corrupt existing object: they
fail with its identity so it can be investigated. An interrupted sync may leave
unreferenced packs, which are harmless and reused on retry. Completed snapshots
are published only after their dependencies finish.

## Limits and validation

Concurrency defaults to four objects and can be set from 1 to 32. Each encoded pack
is capped at 8 MiB + 64 KiB; each encoded manifest and HTTP inventory response is
capped at 64 MiB. Body limits apply while streaming even if the server supplies an
incorrect or absent Content-Length. Memory scales with in-flight objects, manifest
decoding, and the inventory of object names; inventory metadata remains O(number
of objects). This implementation does not paginate its HTTP inventory.

S3 operations use the backend's retry policy. HTTP connection failures, timeouts,
408, 429, and server errors retry up to three times with exponential backoff;
authentication errors and redirects fail immediately. HTTP requests have a
15-second connection timeout and a five-minute per-attempt timeout. A failed body
stream or exhausted retries leave the spool available for the next backup.

`transferred_bytes` counts newly published encoded object bytes, not HTTP overhead,
verification downloads, retry traffic, or total bandwidth.

The automated suite exercises file round trips, idempotency, corrupt sources and
destinations, missing dependencies, oversize rejection, symlink rejection, HTTP
authentication, redirects, retries, and restore through the actual Nenia server.
An additional S3 protocol test passed against Moto 5.2.5 on localhost: concurrent
uploads, idempotency, exact restore, corrupt objects, and missing dependencies.
Real AWS and R2 accounts have not been exercised in this environment. Provider
permissions, credential refresh, TLS/proxy setup, cold-storage retrieval, and
provider-specific conditional-write behavior still need validation in the target
deployment. No cloud resources were created by development tests.

To rerun the S3 test, install `moto[server]` in a Python virtual environment and
start `moto_server -H 127.0.0.1 -p 5001`. In another terminal using that environment:

```sh
export AWS_ENDPOINT=http://127.0.0.1:5001
export AWS_ALLOW_HTTP=true
export AWS_ACCESS_KEY_ID=testing
export AWS_SECRET_ACCESS_KEY=testing
export AWS_REGION=us-east-1
export NENIA_TEST_S3_REMOTE=s3://nenia-test/tests
python -c 'import boto3; boto3.client("s3", endpoint_url="http://127.0.0.1:5001").create_bucket(Bucket="nenia-test")'
cargo test --test remote s3_emulator -- --ignored
```

The test creates a unique prefix and deliberately corrupts and removes its own
objects. It refuses non-loopback endpoints and is ignored by normal `cargo test`.
