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
encodings where appropriate. Trace JSON contains large repeated strings, embedded base64 and unknown future
fields. Nenia factors canonical string values and binary payloads before chunking,
then records a reversible recipe. It preserves all original spelling, whitespace,
unknown fields and malformed input. The generic compression layer is still Zstd;
there is no trained dictionary to lose or distribute.

## Data model

A snapshot is a device ID, source path, capture time, best-effort session metadata,
whole-file hash, byte length, chunk references and, for format 2, a reconstruction
recipe. Each snapshot can be read without an index or another snapshot. Version 2
references authenticate containing packs and omit redundant per-chunk hashes;
recovery recreates those hashes for the catalog. Reversions preserve new versions
while reusing content. See the [format contract](FORMAT.md).

SQLite contains session rows, a chunk lookup table, a disposable size/mtime cache,
and an optional contentless FTS5 index. It is a cache of immutable archive objects;
it is never copied to remote storage or shared over a network filesystem.

The capture transaction publishes and syncs packs, then manifests, then commits
SQLite. A crash before the catalog commit may leave immutable orphan objects or
uncatalogued manifests. Reconciliation discovers valid manifests. Nothing automatically
deletes orphan objects. Server upload follows the same pack-before-manifest rule,
verifying the whole reconstructed snapshot before accepting its manifest.

## Performance boundaries

Raw traces and decoded canonical values use content-defined boundaries. Native
Codex Zstd files are normalized only after proving byte-for-byte reconstruction
with a pinned historical encoder; unknown variants remain opaque. This allows
raw and sealed copies of a session to share their payload. The encoder compatibility
obligation is explicit in the format, rather than assumed from a compression level.

The client keeps a full compressed offline spool. Deduplication is chunk-level
within that spool; server deduplication is by complete encoded object. This avoids
a remote round-trip for every chunk and avoids a distributed mutable index.

Whole packs decompress for reads; they are capped at 8 MiB raw. An eight-entry LRU keeps
at most 64 MiB of raw cached packs. This makes reads of alternating old/new chunks
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

## Keep the operational model small

Benchmarks use only the user's frozen Codex and Claude traces. They include native
containers, full archive/catalog costs and byte-exact restoration. Exploratory
PNG reconstruction and tiny-chunk variants are recorded separately from production
measurements. No public dataset or generated session supplies performance claims.

Full-text indexing remains optional. Immutable writes, checksums, bounded readers,
explicit verification and independent recovery remain the core guarantees.
Retention/GC and encryption need their own format and recovery protocol before
becoming default behavior.
