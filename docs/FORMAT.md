# Nenia archive formats

The archive consists only of `packs/<id>.nz` and `manifests/<id>.json.zst`.
An object ID is the lowercase, 64-character hexadecimal BLAKE3 hash of the
**complete encoded object bytes**. Config, SQLite, locks, and local observation
caches are not required to interpret the archive. `nenia init` supplies fresh
configuration before `pull` or a manual recovery.

All integer fields in a pack header are unsigned little-endian. The original
format-1 header is:

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
old objects. The primitive writer is specified in `crates/codecs/src/pack.rs`.

## Version 1 manifest

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

## Version 2: reversible trace factoring

Pack files keep format 1. Manifest version 2 uses a unique chunk table and a
lexical reconstruction recipe. A chunk is `[pack_id, offset, length]`; its pack's
BLAKE3 authenticates the complete bytes, so an additional plaintext chunk hash is
redundant. Recovery recomputes plaintext hashes for the disposable dedup index.
Version-1 manifests must still carry their original chunk hashes.

A version-2 manifest retains all version-1 identity/metadata fields, replaces the
chunk list with compact references, and adds:

```json
{
  "recipe": {
    "decoded_bytes": 12345,
    "decoded_hash": "BLAKE3 of the reconstructed JSONL bytes",
    "operations": [[0, 0, 256, 1]],
    "container": "zstd-1.5.7-stream3"
  }
}
```

Each operation is `[chunk_index, slice_offset, slice_length, flags]`. Authenticate
and decode the containing pack, take the referenced chunk, then take the operation's
slice. Operations address only the chunk table, never other operations or files.
There are no recursive archive references or dependencies on earlier snapshots.
All chunks must be referenced. Empty slices, invalid ranges and unknown flags fail.

Flags are unsigned integers 0..63:

- Bits 0..2: JSON escaping depth, restricted to 0..3. Escape double quotes,
  backslashes and control bytes exactly as compact `serde_json` strings do, without
  surrounding quotes; non-ASCII UTF-8 bytes are unchanged. Repeat for each layer.
- Bit 3: first encode the slice as canonical base64.
- Bit 4: use the URL-safe alphabet instead of the standard alphabet.
- Bit 5: omit base64 padding. Bits 4/5 require bit 3.

Base64 encoding precedes JSON escaping. Concatenate operation outputs, check
`decoded_bytes` and `decoded_hash`. Without `container`, those must equal the
manifest's original `bytes` and `content_hash`.

The writer factors canonical strings at least 256 bytes long, including nested
JSON strings, to a maximum depth of three. Unknown fields and noncanonical spelling
stay literal. Content-defined boundaries are 8/32/256 KiB minimum/average/maximum.
For binary values, four-character literal bridges reconstruct base64 triplets that
cross chunk boundaries. This allows binary deduplication at arbitrary offsets with
no carry state in the reader. The algorithm does not depend on provider field names.

### Native Codex Zstd reconstruction

`container` is absent or exactly `zstd-1.5.7-stream3`. For this profile, feed the
reconstructed bytes to the **Zstd 1.5.7 streaming encoder**, level 3, default
single-thread parameters, no checksum or dictionary, with pledged source size
`decoded_bytes`. Finish once without intermediate flushes. Verify the resulting
length and BLAKE3 against the manifest's original identity.

This matches the [Codex writer](https://github.com/openai/codex/blob/main/codex-rs/rollout/src/compression.rs).
The writer proves the entire original compressed stream matches this profile
**before** selecting it. Different encoders, parameters, malformed containers and
streams outside the normalization budget retain the opaque version-1 path.
The historical encoder is part of the read contract: dependencies are pinned,
the runtime checks the linked codec version, and a fixed compatibility digest is
checked on native Linux, macOS and Windows. A future codec upgrade must retain this
implementation for existing profiles. A generic newer Zstd encoder is not a substitute.

### Bounds and upgrades

The manifest remains bounded to 64 MiB decoded/encoded, with at most 262,144 unique
chunks and 2,000,000 operations. Output cannot exceed its declared size or 64 GiB.
Native normalization is limited to 2 GiB decoded with a 64 MiB Zstd window. Capture
uses an anonymous private temporary file for this decoded native stream; it is
removed when capture closes it. Raw lexical processing uses at most 32 MiB source
segments; oversized portions stay literal. The pack cache holds at most eight
8 MiB decoded packs. Escaping expansion is bounded by one 256 KiB chunk and three
escape layers, independently of the source file's total size.

Readers accept versions 1 and 2. **Upgrade the server before clients.** Older
servers reject version 2 instead of accepting an unreadable snapshot. Existing
objects are immutable and are not rewritten or deleted automatically. A full scan
of unchanged legacy snapshots does not create unused transformed objects. Fresh
captures use version 2 for raw inputs of at least 4 KiB and proven native containers;
small raw files retain version 1.

`nenia cat SNAPSHOT --decoded` exposes JSONL directly, including from legacy native
containers (a legacy decoded view aborts after exceeding the 2 GiB budget). `cat` without this flag and `restore` always reproduce original bytes.

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

## Version 2 packs: cold image reconstruction

Nenia 0.2 adds a pack codec while keeping existing manifest recipes and chunk
coordinates. A format-2 pack's decoded payload is the **original image file's
bytes**, not its pixels. The same 8 MiB decoded-pack limit and 256 KiB chunk limit
apply. All encoded objects remain BLAKE3-addressed and bounded to 8 MiB + 64 KiB.

| Offset | Bytes | Meaning |
| --- | --- | --- |
| 0 | 8 | `NENIAPK` followed by NUL |
| 8 | 2 | Version 2 |
| 10 | 2 | Codec: 1 = PNG, 2 = JPEG, 3 = referenced image body |
| 12 | 8 | Original image-file length |
| 20 | 4 | Dependency count, at most 1,024 |
| 24 | 32 × count | Sorted, unique binary BLAKE3 IDs of ordinary format-1 packs |
| following | remaining | Versioned reconstruction payload |

Dependencies must be ordinary format-1 packs. The decoder rejects recursive
image dependencies, undeclared dependencies, and unused dependency declarations.
A referenced body and its pixels can occupy slices of these ordinary packs;
whole encoded-body and pixel-object hashes are checked after assembly.

Uploads validate object identity and bounded framing before accepting packs.
Manifest publication requires all dependencies, complete reconstruction, and the
original trace hash. This permits arbitrary pack upload order without exposing
an incomplete snapshot. Catalog recovery and orphan accounting include dependency
packs.

PNG payloads start with `NENIAPG2`. Their original-file size and hash precede
independently compressed side data, reconstruction data, and pixels or a pixel
recipe. Side data preserves non-IDAT chunks, original IDAT splits/CRCs, zlib
header/trailer bytes, and scanline filters. Reconstruction uses either the bounded
DEFLATE match recipe, pinned fdeflate 0.3.7 (whole input or per scanline), or pinned
zlib 1.3.1 with recorded level/memory/strategy. An encoder profile is accepted only
after reproducing the entire original stream. Portable PNGs reject the legacy
Preflate correction type.

Pixel recipes may describe independent images, exact scanline/tile fragments,
keyframe deltas, or exact residual sprites. Pixel arrays are bounded to 32 MiB.
Leaves use Zstd, lossless WebP, or JPEG XL. Every leaf has an encoded hash, explicit
layout, and bounded ordinary-pack references. Cached decoded pixels are keyed by
codec, layout, encoded identity, and storage references; caches cannot bypass
changed metadata validation. JPEG XL allocation has its own explicit budget.

JPEG payloads start with `NENIAJP2`, followed by mode, original-file size/hash,
width/height, and opaque Zstd or Lepton 0.5.5 data. Lepton images are bounded to
8,192 pixels per dimension and 16 megapixels total; writes cannot exceed the
original-file size. Opaque preservation remains available for unsupported files.

Referenced bodies start with `NENIARF1`: inner codec, original-file length,
encoded-body hash, encoded-body length, then bounded compressed chunk references.
They reduce repeated image-header and encoded-body storage without adding a
recursive pack graph. The implementation is in
[`crates/codecs`](../crates/codecs/src/media.rs).

`nenia compact --output NEW_VAULT` rewrites manifests into a fresh destination
and verifies/catalogs the result. It does not delete or mutate source objects.
