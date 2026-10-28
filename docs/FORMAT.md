# Nenia archive format 1

The archive consists only of `packs/<id>.nz` and `manifests/<id>.json.zst`.
An object ID is the lowercase, 64-character hexadecimal BLAKE3 hash of the
**complete encoded object bytes**. Config, SQLite, locks, and local observation
caches are not required to interpret the archive. `nenia init` supplies fresh
configuration before `pull` or a manual recovery.

All integer fields in a pack header are unsigned little-endian:

| Offset | Bytes | Meaning |
| --- | --- | --- |
| 0 | 8 | Magic `NENIAPK` followed by a NUL byte |
| 8 | 2 | Version, exactly 1 |
| 10 | 2 | Reserved flags, exactly 0 |
| 12 | 8 | Uncompressed payload length, 1..8,388,608 |
| 20 | remaining | One Zstd frame containing concatenated original chunks |

Writers enable the Zstd checksum, use a maximum window log of 23, and pledge the
raw size. Readers verify the encoded BLAKE3, header, version/flags, size, checksum,
and exact end of the single frame. Extra frames/trailing bytes are rejected.
The encoded pack limit is 8 MiB + 64 KiB, allowing incompressible data overhead.

A snapshot is limited to 262,144 chunk references. The chunking policy is deterministic Gear hashing with a fixed SplitMix64 table:
8 KiB minimum, approximately 32 KiB average, and 256 KiB maximum for raw logs.
Sealed `.jsonl.zst` sources use fixed 256 KiB chunks to avoid redundant rolling
hashes and excessive catalog entries. Both policies share the same format. The final chunk
may be smaller than the minimum. Empty files have no chunks. Readers do not need
the chunker or its table: manifests supply every offset and length. A future
chunking policy can therefore coexist with current readers without recompressing
old objects. The current writer is specified in `src/pack.rs`.

## Manifest

A manifest is a Zstd-compressed UTF-8 JSON object. Its ID hashes the compressed
bytes. Encoded and decoded manifests are bounded to 64 MiB; the decoder's window
is independently bounded to 64 MiB.

```json
{
  "version": 1,
  "revision": 3,
  "device_id": "a unique opaque device identifier",
  "origin": "/original/path/to/session.jsonl",
  "captured_at": "2026-10-28T05:00:00+00:00",
  "content_hash": "64 lowercase hex digits: BLAKE3 of exact original bytes",
  "bytes": 12345,
  "metadata": {
    "source": "codex",
    "session_id": "provider-specific session ID or null",
    "title": "best-effort title or null",
    "cwd": "/original/working/directory or null",
    "started_at": "provider timestamp or null",
    "events": 1,
    "malformed_lines": 0,
    "skipped_oversized_lines": 0
  },
  "chunks": [
    {
      "hash": "BLAKE3 of this original chunk",
      "pack": "BLAKE3 of the complete encoded containing pack",
      "offset": 0,
      "len": 12345
    }
  ]
}
```

This example uses explanatory strings in hash fields; real hashes must match the
strict 64-character syntax. JSON property order is not semantically significant.
Unknown fields are ignored. Unknown format versions are rejected.

`revision` increases per `(device_id, origin)` whenever content changes, including
a return to old content. It determines the latest snapshot independently of wall
clock changes. Older/imported manifests may omit revision/device ID, interpreted
as 0/empty respectively. A fresh installation receives a random device ID; do not
clone one device's `config.json` onto a different device.

For each chunk, decode its pack, verify `offset + len` is in bounds, take that
slice, and verify its plaintext BLAKE3. Concatenate slices in manifest order;
verify the total byte length and whole-file BLAKE3. No JSON normalization occurs.
CRLF, trailing whitespace, malformed records, binary bytes, and a missing final
newline remain exactly as captured. Original paths must be valid UTF-8; unsupported
path names are rejected instead of being merged through lossy conversion.

## Publication and recovery

Objects are written to private temporary files in the destination directory,
file-synced, and published without replacing an existing object. Existing objects
must match exactly. Unix directory entries are synced. Windows file flushing is
implemented; equivalent directory power-loss durability has not been established
by native testing in this workspace.

Packs precede their manifests. Catalog commits follow object publication. Upload
acknowledgment from the server follows validation and local publication. The
optional off-site mirror completes separately. There is no deletion protocol.

To reconstruct a catalog, verify objects and ingest each manifest's metadata and
chunk references. A corrupt or missing SQLite file does not change this procedure.
Nenia preserves the old catalog before creating a fresh one. Verification currently
also checks orphan packs: a damaged unreferenced object must be quarantined by the
operator before a clean rebuild. Never remove a pack just because it is old; newer
snapshots can reference it.

## HTTP protocol

`GET /healthz` is unauthenticated and returns `ok`. All `/v1` endpoints require
`Authorization: Bearer <token>`. The server speaks HTTP; HTTPS termination belongs
to the deployment's proxy/private-network setup.

| Method/path | Meaning |
| --- | --- |
| `GET /v1/objects?limit=4096` | First immutable-object inventory page |
| `GET /v1/objects?limit=4096&after=N&watermark=W` | Next page of the same inventory |
| `GET /v1/packs/<id>` | Encoded pack |
| `PUT /v1/packs/<id>` | Validate/publish pack; 201 new or 204 identical |
| `GET /v1/manifests/<id>` | Encoded manifest |
| `PUT /v1/manifests/<id>` | Validate all referenced data and publish manifest |
| `GET /v1/snapshots?limit=100&all_versions=false` | Catalog rows |
| `GET /v1/search?q=words&limit=100` | Metadata/optional full-text matches |
| `GET /v1/stats` | Storage accounting |

Inventory pages contain `packs`, `manifests`, `next_after`, `watermark`, and
`generation`. The first page captures a catalog sequence watermark. Continue until
`next_after` is null, passing the same watermark. Check that generation remains the
same: a catalog rebuild changes it. Object registration always orders packs before
dependent manifests, so an inventory at a fixed watermark contains all dependencies.
New uploads are excluded until the next inventory. This avoids an archive-wide HTTP
response-size ceiling. The client still holds object-name inventories in memory.

The maximum page limit is 10,000. Uploads are bounded to four simultaneous object
bodies and downloads to eight retained object bodies. Saturation returns 429;
clients retry with bounded exponential backoff. Invalid data is rejected. There
are no update/delete endpoints and no network access to SQLite.
