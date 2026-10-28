# Nenia

Small, lossless backups for agent traces. One Rust binary runs on your devices and
your server. Sessions collect in an offline spool, upload to your own endpoint,
and restore byte for byte. No database service, queue, or cloud account required.

Named for [Nenia](https://logeion.org/nenia), the Roman personification of the
funeral song: a place for sessions after they end.

## Start a server

Build with Rust 1.88 or newer:

```sh
cargo build --release --locked
# Put target/release/nenia on your PATH (nenia.exe on Windows).
export NENIA_TOKEN="$(openssl rand -hex 32)"
nenia --vault ./data serve --listen 127.0.0.1:7433
```

Or build the included container:

```sh
export NENIA_TOKEN="$(openssl rand -hex 32)"
docker compose up -d --build
```

Keep the token for your devices. Put the HTTP endpoint behind your existing HTTPS
proxy or private network. The default listener/Compose port is loopback. See
[deployment](docs/deployment.md) for Windows, macOS, Linux, TLS, and scheduling.

## Connect each device

Use the same server token on each device:

```sh
export NENIA_TOKEN="your-server-token-at-least-24-characters"
nenia connect https://traces.example.com
nenia backup
nenia watch
```

`connect` saves the URL and token in your private spool. `backup` discovers Codex,
Claude Code, and Pi without source configuration. `watch` repeats every 60 seconds;
offline uploads retry on the next cycle. `nenia run -- codex` runs your agent with
its usual terminal and backs up afterward, preserving its exit status. It reports
pending uploads without changing a successful agent's exit code.

For Plaud exports or other files:

```sh
nenia backup ~/exports --source plaud
nenia backup ./traces ./another-session.jsonl
nenia ingest ./traces                  # capture offline without uploading
nenia push                            # upload the existing spool later
```

Automatic roots and overrides are documented in [adapters](docs/adapters.md).
Plaud support is for exported files; it does not connect to a Plaud account.
Source files are never modified or removed.

## Object storage

The server can keep an additive off-site mirror using its own S3 credentials:

```sh
nenia --vault ./data serve --mirror s3://my-bucket/nenia
```

It mirrors every 5 minutes by default and retries failures. Upload acknowledgment
means the server's local copy is durable; the off-site mirror is asynchronous.
Watch server logs for `Mirror complete` / `Mirror pending`.

Clients can also back up directly to S3-compatible storage or a mounted directory:

```sh
nenia connect s3://my-bucket/nenia
nenia backup
nenia push --remote file:///mnt/backup/nenia
```

AWS, R2, MinIO configuration, verification costs, and protocol details are in
[remote storage](docs/remote.md). No external AWS CLI is needed.

## Find and restore

On a fresh device, connect and pull. A new catalog is reconstructed from the
archive; no original device database is needed.

```sh
nenia pull
nenia list
nenia list --all-versions
nenia show <snapshot-prefix>
nenia restore <snapshot-prefix> --output recovered.jsonl
nenia cat <snapshot-prefix>            # exact source bytes to stdout
```

`restore` checks hashes before publishing the output and refuses to overwrite an
existing file. Snapshots preserve their originating device, source path, and
session metadata. Matching paths on different devices remain separate.

Metadata search works immediately. Full-text search is optional:

```sh
nenia index
nenia search "sqlite migration"
nenia --vault ./data serve --index     # optional server-side full-text search
```

The index stores token postings without a second plaintext copy of the traces.
Queries match all words; phrase search, snippets, and semantic embeddings are not
implemented. Default indexing keeps at most 8 MiB of extracted text per snapshot;
the archive always preserves the complete original bytes. Binary/media payloads
are omitted from text indexing. Indexes can still be larger than compressed data.

## Why it stays small

1. Split the original byte stream at content-defined boundaries, averaging roughly
   64 KiB (16–256 KiB bounds).
2. Store repeated chunks once in a device's catalog, including unchanged prefixes
   of growing sessions and repeated content across files.
3. Compress up to 8 MiB of unique chunks together with checksummed Zstd.
4. Store small, independently readable snapshot manifests with exact chunk offsets.

Immutable object names are BLAKE3 hashes. Whole identical packs are shared by the
server across devices. Independently packed overlapping chunks from different
devices are not automatically repacked into global chunk deduplication.

This is a focused composition of established backup techniques, not a new
compression algorithm. [Design and prior art](docs/design.md) explains the choices
relative to restic, rustic, Kopia, and ClickHouse. [Reproducible benchmarks](docs/benchmarks.md)
compare Nenia with per-file and solid Zstd, including manifests, SQLite, and optional
index overhead. The advantage is strongest when retaining overlapping snapshots;
independent files can be slightly larger than plain Zstd.

## Operate it

```sh
nenia stats                           # payload, manifests, catalog, other, total bytes
nenia verify                          # hash and reconstruction audit
nenia push --verify-existing          # also download/check objects already remote
nenia backup --full-scan              # bypass unchanged-file size/mtime cache
nenia rebuild                         # verify archive, preserve old DB, rebuild catalog
```

Normal backup trusts the remote inventory for objects already present, avoiding
downloads of old payloads. New objects are checked. `pull` checks both downloaded
and existing local objects. Routine capture uses size and modification time to
skip unchanged sources; use `--full-scan` after edits that preserve timestamps.

`rebuild` also works with a corrupt SQLite file. It verifies the archive before
moving old catalog files into a `catalog-before-rebuild-*` directory. Full-text
indexes and the change cache are disposable and can be rebuilt. Missing archive
objects must be restored from another copy; hashes detect damage, not repair it.

The format is documented in [FORMAT.md](docs/FORMAT.md). All data can be restored
with the library or CLI; no account or proprietary service is needed.

## Scope of v0.1

- One tenant, one server process, one writer per local spool. Clients may upload
  concurrently. The server serializes catalog operations.
- Archives are compressed, not encrypted by Nenia. Use HTTPS and private encrypted
  storage when needed. The operator of the server can read the archive.
- No automatic deletion, retention pruning, cold-tier retrieval, or local cache
  eviction. The spool retains captured objects. Shared packs must not be expired
  independently based on their age.
- No filesystem snapshot guarantee for arbitrary files modified in place during
  capture. Agent append-only JSONL is the intended source. Parsing is best effort;
  malformed JSON and partial final records are still preserved.
- Native Linux execution and local S3 emulation are tested here. CI is configured
  for native Windows/macOS/Linux tests; Windows/macOS runtime and actual AWS/R2
  accounts have not been exercised in this workspace.

## Develop

```sh
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
```

See [design checkpoints](docs/principles.md) for the decisions reconsidered during
implementation. MIT licensed.
