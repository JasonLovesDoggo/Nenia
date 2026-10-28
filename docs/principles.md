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

Still to validate: native Windows file durability behavior, real S3 protocol,
compression with full metadata overhead, source discovery, cold recovery, and
whether a multi-year index remains optional and bounded.
