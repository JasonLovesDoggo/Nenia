# Product decisions and rebuild checkpoints

The product is a remote backup service with an offline spool on each device.
It is single tenant. A self-hosted server runs as one binary/container, one data
directory and one bearer token; no external database, queue, or orchestration.
Clients discover Codex, Claude Code, and Pi session directories on Windows, macOS,
and Linux. Exported files from other tools can be added explicitly. A server URL
and token are the only connection configuration; source configuration is optional.

The archive format is independent of deployment. Immutable packs and manifests
can be mirrored directly to S3-compatible storage or restored to another server.
SQLite is a rebuildable catalog, never the only copy of archive metadata.
Offline captures remain in the spool until the next successful upload. This
version does not delete source traces, local archive objects, or remote objects.

At each major milestone ask: knowing what we know now, would we rebuild this
differently? Record evidence, remove avoidable complexity, and prefer simpler
recovery and fewer operational dependencies over cleverness.

## Checkpoint 1: storage versus product

Original framing put local storage first. The user's clarified goal is remote
multi-device backup. Keep the lossless format and rebuildable catalog; promote
remote connection, automatic discovery, offline retry, and a single-process
server to the primary flow. Avoid introducing a distributed database: immutable
content-addressed objects already give us idempotent retries and cross-device reuse.

## Checkpoint 2: measure the whole archive

Synthetic independent traces, append histories, forks, and random binary controls
were measured against per-file and solid Zstd. The strong savings come from
overlapping retained history, not an invented codec. Independent traces can be
larger than plain compression. The optional token index can outweigh compressed
payloads. Keep search optional, show archive/catalog/index overhead separately,
and retain negative controls and the strong solid-compression baseline.

The review replaced full-session temporary files during indexing with a verified
chunk reader and added a two-pack cache for alternating old/new chunks. Binary
data no longer becomes meaningless text tokens. No speculative codec tuning was
introduced after seeing the benchmark results.

## Checkpoint 3: recoverability and long-lived operation

Independent reviews found source reversion, corrupt-catalog recovery, missing
object, wrapper exit-status, and wall-clock ordering cases. Regression tests now
cover them. Revisions order each device/source independently of capture time.
Rebuild preserves old catalog files and verifies the archive before replacement.
Missing indexed objects cause capture to fail rather than acknowledge an unusable
backup. A full source scan remains available alongside the size/mtime fast path.

HTTP inventories now use consistent watermark pages. Object registration orders
packs before manifests; catalog generations detect a rebuild between pages. This
removes the original single-response ceiling without a distributed database or
persistent remote-receipt cache that could hide missing server data.

The server can mirror to S3 while serving clients, so credentials stay on the
server and off-site replication needs no additional daemon. It logs local
acceptance separately from asynchronous mirror progress. Source files and archived
objects are never automatically deleted. Object-name inventory memory and filesystem
metadata work still grow with archive size; this is not a tested billion-object store.

## Remaining evidence boundaries

Linux native flows and an isolated S3 protocol emulator are exercised. Hosted CI,
native Windows/macOS runtime, Docker runtime, actual AWS/R2 accounts, and physical
power-loss fault injection were not available locally. Cross-platform code paths,
native CI, deployment checks, and their limitations are documented. No claim of
general backup-tool maturity or cloud durability is inferred from local tests.
