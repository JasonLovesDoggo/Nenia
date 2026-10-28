# Design and prior art

The primary job is to keep exact session history remotely for years with little
operational work. A client needs only a server URL and token. Capture works offline;
upload is additive and retryable. The server is one process and one directory.

## What we reused conceptually

[restic's repository design](https://restic.readthedocs.io/en/stable/100_references.html)
uses immutable, content-addressed packs and snapshots. Its
[content-defined chunking explanation](https://restic.net/blog/2015-09-12/restic-foundation1-cdc/)
shows why boundaries derived from content retain reuse when bytes are inserted.
Nenia uses the same broad ideas with a compact Gear chunker and BLAKE3 identities.
It does not implement or claim compatibility with the restic format.

[rustic](https://github.com/rustic-rs/rustic) already implements restic-compatible
backups in Rust. [Kopia](https://kopia.io/docs/features/) combines compression,
deduplication, encryption, and a repository server. For general machine backups,
these are substantially more mature choices. Nenia's scope is narrower: automatic
agent trace discovery, exact session versions, a queryable session catalog, and
one small client/server product. It does not claim greater durability or general
backup feature parity.

[ClickHouse's compression work](https://clickhouse.com/blog/optimize-clickhouse-codecs-compression-schema)
demonstrates arranging data so generic compression has less work to do, with typed
encodings where appropriate. Trace JSON contains large strings and unknown future
fields. Sorting or columnizing it would require a second representation to preserve
the original bytes. Nenia instead removes identical byte chunks and compresses
remaining chunks together in bounded packs. Zstd's dictionary matching handles
repeated JSON keys and nearby prompt fragments. There is no separate trained
dictionary to lose, distribute, or migrate.

## Data model

A snapshot is a device ID, source path, capture time, best-effort session metadata,
whole-file hash, byte length, and ordered chunk references. A chunk reference carries
its plaintext hash, encoded pack hash, raw offset, and length. Each snapshot can be
read without an index or another snapshot. Reversions create new snapshots while
reusing old chunks. The manifest overhead is the cost of independent recovery.

SQLite contains session rows, a chunk lookup table, a disposable size/mtime cache,
and an optional contentless FTS5 index. It is a cache of immutable archive objects;
it is never copied to remote storage or shared over a network filesystem.

The capture transaction publishes and syncs packs, then manifests, then commits
SQLite. A crash before the catalog commit may leave immutable orphan objects or
uncatalogued manifests. Reconciliation discovers valid manifests. Nothing automatically
deletes orphan objects. Server upload follows the same pack-before-manifest rule,
verifying the whole reconstructed snapshot before accepting its manifest.

## Performance boundaries

Raw traces use 32 KiB average content-defined chunks; sealed Zstd traces use
fixed 256 KiB chunks. The choice comes from [actual format measurements](format-analysis.md).
The source containers remain byte-for-byte recoverable.

The client keeps a full compressed offline spool. Deduplication is chunk-level
within that spool; server deduplication is by complete encoded object. This avoids
a remote round-trip for every chunk and avoids a distributed mutable index.

Whole packs decompress for reads; they are capped at 8 MiB raw. A two-entry LRU keeps
at most 16 MiB of raw cached packs. This makes reads of alternating old/new chunks
less wasteful. Indexing reads verified chunks directly and stops at its configured
text budget. It does not write a plaintext temporary copy of a session.

Normal backup stats sources and archive objects, lists inventories, and transfers
new objects. Existing remote payloads are reread only when requested. Inventory
work still grows with object count. HTTP inventories use consistent, bounded pages;
clients still hold name inventories in memory. Persistent server receipts and
globally shared chunk catalogs remain future scaling work.

The server's optional S3/file mirror is asynchronous. It reads immutable files while
uploads continue. An inventory race can postpone a cycle, but cannot publish a
manifest without its packs. It retries on the next interval. Server-local durable
acceptance and off-site replication completion are distinct events in logs.

## Why no novel codec, distributed database, or compulsory search index

The benchmark uses actual local Codex and Claude sessions plus pinned public
trace datasets. It compares complete archives with per-file and solid Zstd,
verifying every restored byte. A solid stream can still win on space. Nenia trades some compression density
for bounded reads, incremental uploads, independent snapshots, and recoverable
metadata. The full-text index can outweigh the compressed archive, so it is opt-in.

The implementation concentrates on correctness and operability: versioned formats,
bounded decoding, immutable writes, checksums, explicit integrity audits, reproducible
measurements, and tested restore paths. Retention/GC and encryption would each need
their own reviewed format and recovery protocol before becoming default behavior.
