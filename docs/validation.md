# Validation

Linux validation uses the actual Framework sessions and the complete copied Mac
snapshot. The core optimization is commit `f625fef`; the final real-data harness
is `c5ab6c6`. Source files remain private and unchanged.

## Completed locally

```sh
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo build --release --locked --bin nenia
```

All 92 automatic tests pass: 31 unit, 11 CLI, 2 compressed-source, 10 hook,
7 reliability, 20 remote, 6 server, and 5 vault tests. Tests cover corruption,
recovery, concurrent hook handoff, offline retries, source reversions, paged
inventories, authentication, bounded downloads, and exact native-container restore.
Unit fixtures exercise edge cases; performance claims use actual sessions only.

The final real-data run archived 342 files / 774,000,764 native bytes, completed
3,078 byte-exact measured restores, and separately restored all 342 files through
HTTP and again after deleting SQLite. Eight genuinely grown source files were
also captured; their new versions and every original version restored exactly.
See [measurements and retained evidence](benchmarks.md).

The locally installed Claude hook was invoked with an existing private transcript.
The callback returned in about 10 ms, its detached worker captured the file, and
restoration matched all 2,166,911 original bytes. The event envelope was invoked
directly; no model request was made. Existing Claude settings were backed up and
unrelated settings preserved. With no remote configured, the capture stays offline.

An isolated Moto 5.2.5 S3 protocol test previously passed concurrent duplicate
uploads, idempotency, exact download, corrupt object rejection, and missing pack
rejection. The emulator was stopped. No cloud resources were created. Its command
and setup are documented in [remote storage](remote.md).

## Evidence boundaries

Native Windows/macOS runtime and Docker checks are configured in GitHub Actions.
Actual AWS/R2 accounts, WAN performance, physical power-loss fault injection, and
multi-year live workloads have not been exercised locally. The archive is compressed,
not encrypted by Nenia; use HTTPS and appropriate private storage.

The public repository includes code, deterministic edge-case tests, aggregate
measurements, and documentation. It does not contain private traces, source
mappings, session identifiers, original data hashes, archive objects, or credentials.
