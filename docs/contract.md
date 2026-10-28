# Implementation contract

One Rust binary and library. Local vault layout:

```
config.json
catalog.sqlite3             # disposable local catalog
vault.lock                  # advisory exclusive writer lock
packs/<blake3>.nz           # immutable compressed objects
manifests/<blake3>.json.zst  # immutable, independently readable snapshots
```

Every snapshot records exact source bytes, their BLAKE3, original byte length,
source path and adapter metadata, plus ordered chunk references. Each chunk reference
is `{ hash: String, pack: String, offset: u64, len: u32 }`. Hashes are lowercase BLAKE3 hex.
The pack ID is the BLAKE3 of the entire encoded pack file, not its plaintext.
Manifest ID is BLAKE3 of the compressed manifest bytes. Manifests contain their format
version but not their own ID. All object paths are derived only from validated hashes.

Pack module owns content-defined chunking and a versioned bounded Zstd pack codec.
Target chunks: min 16 KiB / average 64 KiB / max 256 KiB. Pack target 8 MiB raw.
Pack builder appends unique raw chunks and exposes offsets; entire bounded pack
is compressed together so repeated JSON keys and prompts compress across chunks.
Reader checks object hash, bounds, decompression size, and per-chunk hashes.

Adapters own discovery and metadata extraction for Codex, Claude Code, Pi, and generic
JSONL/JSON/text exports (including Plaud exports). Unknown data must still archive
losslessly. Source parsing is best effort and never gates preservation of bytes.

Remote module owns additive push/pull of packs then manifests, using file:// or S3.
It never copies the SQLite catalog or deletes local/remote data. Ignore temp files,
validate object hashes, publish manifests only after their packs. Pull into a new or
existing vault then root CLI rebuilds catalog. S3 configuration uses standard AWS env.
Sync must acquire the same vault lock or be called with it held; root CLI holds it.

Root owns vault/catalog, manifests, CLI, and integration. Coordinate API details via messages.
