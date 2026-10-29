# Validation

The selected archive implementation is `0173121`. It is tested on the user's
original Framework/Mac corpus and the completed additional upload. Source files
remain private and unchanged. The rejected placement variant is recorded separately.

## Automated checks

```sh
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo build --release --locked --bin nenia
```

All **100 local tests** pass: 35 unit, 11 CLI, 2 compressed-source, 10 hook,
7 reliability, 20 remote, 6 server, 4 structural-format, and 5 vault tests.
One separately gated S3-emulator test is ignored by the ordinary test command.
Fixtures exercise edge cases; none supplies a performance workload.

New checks cover exact lexical reconstruction, all byte-valued Unicode characters,
nested escaping, base64 alphabets/padding/boundary bridges, unknown native encoders,
malformed recipes, raw/native payload sharing, recovery without source files or
SQLite, unchanged legacy captures, decoded viewing and LRU eviction. A fixed Zstd
reconstruction digest is checked across platforms and input write fragmentation.

Native Linux, macOS and Windows tests/release builds and Docker build/startup
passed in [GitHub Actions](https://github.com/JasonLovesDoggo/Nenia/actions/runs/40814072274)
at the selected code revision. This includes the historical-encoder compatibility
check and the Windows detached-worker handle-inheritance regression test.

## Actual data

The final comparison ran three alternating trials per implementation and corpus:
342 original files and 1,291 completed-upload files. Its twelve batches completed
**9,798 byte-exact restores**, with zero errors. All 638 native Zstd files in the
complete corpus reconstructed to their exact original containers.

Authenticated HTTP transfer and catalog-loss recovery each restored all 1,291
files. Eight actual live appends were captured, then those new versions and all
initial versions restored exactly. Unchanged rescans grew neither snapshots nor
archive bytes. See [the measurements and raw evidence](benchmarks.md).

All 939 checksums listed in the completed upload passed. Ten additional JSONL
exports were outside that supplied checksum list; every frozen input is checked
against its private BLAKE3 identity before benchmarking. The transfer manifest and
checksum file were excluded by discovery. The earlier Mac copy also passed all
268 supplied checksum/length checks.

The selected binary's Claude callback is separately exercised with a frozen actual
transcript, a detached worker, and exact restore. Its aggregate result is
[recorded here](benchmark-results/structural-2026-10-29/actual-hook.json). This invokes
the official event envelope directly, without a model request. Two manual setup attempts omitted `session_id` or used an uninitialized vault.
Both were rejected without capture and are recorded separately. The successful
check uses the normal installer in an isolated configuration and its generated
handler; global Claude settings remain unchanged. The installed
Claude hook remains configured, with its original settings backup preserved.

An earlier isolated Moto 5.2.5 test passed duplicate uploads, idempotency, exact
download, corrupt-object rejection and missing-pack rejection. No cloud resources
were created. Setup is documented in [remote storage](remote.md).

## Evidence boundaries

Performance measurements are on Linux; native CI supplies cross-platform functional
checks. Actual AWS/R2 accounts, WAN performance, physical power-loss injection and
multi-year live workloads have not been exercised. The full in-process recovery
benchmark includes client, server and indexing memory; it is not a server-only
memory measurement. Nenia compresses archives but does not encrypt them.

The public repository contains code, unit fixtures and aggregate results. It has
no private trace payloads, session IDs, source mappings, original data hashes,
archive objects or credentials. Unselected codec experiments remain clearly
separate from production claims. The README stays short.
