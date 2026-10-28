# Validation record

Core source: `0028c05`; complete server test suite: `046dc23`. Subsequent report
and packaging commits do not change the implementation. Validation ran on Linux
with Rust 1.88.0 in this workspace on 2026-10-28 UTC.

## Completed

```sh
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo build --release --locked --bin nenia
```

All 73 automatic tests passed: 24 unit tests, 11 CLI, 7 reliability, 20 remote,
6 server, and 5 vault tests. The separately gated S3 test is ignored in that command.
Clippy and formatting checks passed; the release binary built and its help was
exercised. The tests use synthetic fixtures and temporary stores, not private
agent sessions.

The real HTTP flows cover automatic discovery, offline retry after source deletion,
saved credentials, fresh-device pull and exact restore, two-device isolation,
reversions, size/mtime/full-scan behavior, missing objects, corruption, SQLite loss
and corruption, overwrite protection, index rebuilds, consistent inventory pages,
server indexing, bounded slow downloads, and Ctrl-C/child exit handling.

The final S3 protocol test ran separately at `046dc23`:

```sh
cargo test --test remote s3_emulator -- --ignored
```

Moto 5.2.5 served an isolated localhost endpoint with fake credentials, a disposable
bucket, and a unique prefix. Result: 1 passed, 0 failed in 0.21 seconds. It covered
concurrent duplicate uploads, idempotency, exact download, corrupt object rejection,
and missing dependency rejection. The emulator was stopped. No cloud resources
were created. Reproduction instructions are in [remote.md](remote.md).

The [final benchmark](benchmarks.md) performed 36 measured batches and 612 exact
snapshot reconstructions with zero failures. It retains raw measurements,
environment details, generator seeds, source hashes, and comparison limitations.

## Not established here

- Native Windows/macOS runtime execution. Adapter code cross-checks passed, and
  hosted CI is configured to run the full suite natively on all three platforms.
- Docker runtime and hosted GitHub Actions. Container configuration was reviewed,
  YAML parsed, official base tags verified, and a CI startup probe provided.
- Actual AWS/R2 service behavior, real WAN performance, or provider billing.
- Physical power-loss fault injection, filesystem snapshot consistency during
  arbitrary in-place writes, multi-year live workloads, or general backup-tool
  production maturity.

These boundaries are kept separate from the successful local tests. The release
is a working v0.1 with a documented format and recovery path.
