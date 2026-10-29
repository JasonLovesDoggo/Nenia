# Lossless sharing across agent screenshots

The portable cold format stores the original **774,000,764 bytes in 299,254,562
bytes**, including every manifest and required object. The completed upload is
**2,098,579,235 bytes in 556,793,091 bytes**. These are original on-disk input sizes,
including files that were already Zstd-compressed. No records, images, encrypted
fields, whitespace, or native compression wrappers were discarded.

This is an engineering study on one user's private data, not a general benchmark
or a claim of a compression limit. The 150 MB target was not reached. Ordinary
capture remains fast; the experimental cold compiler prepares an offline
representation that the normal Nenia reader, server, and replication code accept.

## What changed

Agent screenshots often have independently compressed wrappers. Sharing bytes
inside those wrappers misses relationships between their pixels. Nenia separates
three things: original-file reconstruction, pixel representation, and placement
of shared objects. Each layer has an independent identity check.

The DEFLATE recipe preserves headers, match decisions, stored blocks, and padding
while taking literal bytes from separately stored plaintext. A second version
replaces physical match distances with ranks in a reproducible 32 KiB history,
using at most 64 probes per match. On the complete PNG set this reduced the generic
recipe stream from 113,480,043 to 64,841,516 bytes. This is a component result, not
the archive saving. Exact native encoder profiles often need less metadata.

The pixel experiments included independent lossless images, shared scanlines,
shared tiles, scroll-aligned keyframe differences, and repeated residual shapes.
References are bounded: a temporal image uses at most one independent keyframe.
The portable object graph ultimately depends only on ordinary format-1 packs.

The planner counts both recipe bytes and shared objects. Greedy per-image choices
can miss a useful shared base because the first image pays its entire cost.
Coordinate moves, followed by shared-object opening and closing moves, reduced
the original pixel artifact from 100,215,323 to 87,727,724 bytes. Proving identical
decoded pixel arrays across codecs and unifying their encodings reduced that to
86,098,505 bytes. The planner uses local search; it does not claim a global optimum.

## Controlled storage comparisons

All rows below include reconstruction references for the same 482 applicable
8-bit PNG images in the original corpus. They store **pixels only**: original PNG
reconstruction is counted later in the complete vault.

| Pixel representation | Bytes |
| --- | ---: |
| Shared scanlines | 101,036,757 |
| Shared 32-pixel tiles | 94,310,274 |
| Local keyframe deltas | 95,540,741 |
| Global keyframe selection | 94,963,094 |
| Adaptive independent JPEG XL / WebP / Zstd | 104,528,010 |
| Exact residual sprites | 109,927,000 |
| Sprites with an exact background residual | 116,488,843 |
| Joint planning | 87,727,724 |
| Joint planning plus pixel identity canonicalization | **86,098,505** |

The complete corpus contains 1,004 applicable pixel images. Other PNG-like values,
including truncated snippets and unsupported layouts, remain byte-preserved.
This matters: an image decoder's inability to interpret a value never authorizes
its removal from a trace.

The JPEG coefficient dictionary was also implemented and verified. Its original
collection occupied 65,424,380 bytes, versus 50,899,605 for independent adaptive
Lepton containers. It also caused substantial read amplification. Batched scatter
restoration fixed a cache-thrashing problem, but the representation still lost.
It remains an experiment rather than a default codec.

Native encoder replay requires proof, not resemblance. The tested Rust zlib
implementation matched only 24 eligible streams. C zlib 1.3.1 and fdeflate covered
more cases; other PNGs use the generic reconstruction recipe. The portable reader
excludes the legacy Preflate research path. Consequently the exploratory archive
(292.2 MB / 531.7 MB) is smaller than the portable vault actually delivered
(299.3 MB / 556.8 MB); the two must not be conflated.

## Durability and operation

The result uses ordinary Nenia manifests and content-addressed packs. Image packs
produce original image-file bytes, so existing trace recipes still reconstruct
exact original JSONL and native Zstd files. Dependencies are explicit, validated,
and limited to ordinary packs. A manifest is published only after complete source
reconstruction. SQLite remains disposable.

`compact` writes a new vault and never changes the source. Packing encoded image
bodies together recovered another 2,769,844 bytes on the original corpus. The
portable archives were rebuilt and verified by the production reader with zero
orphan packs. Direct frozen-source byte comparisons and HTTP recovery evidence
are recorded alongside the storage reports. The HTTP transfer moved 3,270 objects
(556,793,091 bytes) in each direction; independent catalog reconstruction then
restored all 1,291 sources with literal byte comparisons. An early pre-catalog
enumeration returned zero files and is explicitly excluded in the accompanying
harness notes. The harness now rejects empty catalogs and checks coverage.

Cold compilation and restoration spend CPU reproducing historical encodings.
These exploratory jobs sometimes overlapped other experiments or builds; their
wall times are retained but do not establish comparative throughput. Archive
sizes are complete logical object bytes, not filesystem allocation or a cloud
bill. Disposable catalog size is reported separately. The combined in-process
source/client/server/replica recovery harness peaked at 1,655,672 KiB RSS; this is
not a server-only memory measurement. Decoder correctness
fixtures are generated public test vectors; none contributes performance evidence.

## Reproduction and evidence

The frozen inputs and all intermediate archives stay outside Git. The original
cohort has 342 files; the completed cohort has 1,291. Published reports contain
only counts, byte totals, timing/resource observations, and anonymous per-asset
ordinals. Every sample from a completed or rejected screen is retained in
[`benchmark-results/media-research-2026-10-29`](benchmark-results/media-research-2026-10-29).
Large sample arrays are stored in the `.samples.jsonl` file named by each report.

The runnable experiment binaries are in [`experiments/media`](../experiments/media).
They accept private input/output paths and a separate aggregate report path.
`extract_media.py` inventories actual embedded assets; `reconstruct`, `jpeg-screen`,
`pixel-share`, `temporal`, `jxl-screen`, and `sprites` create candidate artifacts;
`pixel-plan` combines them. `trace-archive` verifies the complete trace representation.
`cold_import` compiles it into a usable vault with the portable reader's bounded
codecs, and `cold_verify` compares every output byte against the frozen sources.

```sh
cargo +1.95.0 build --release --manifest-path experiments/media/Cargo.toml --bins
cargo +1.95.0 build --release --example cold_import --example cold_verify
# Each binary prints its positional input contract when called without arguments.
nenia --vault PRIVATE_COLD_VAULT compact --output PRIVATE_PACKED_VAULT
cargo +1.95.0 run --release --example cold_verify -- \
  PRIVATE_PACKED_VAULT PRIVATE_CORPUS_JSON AGGREGATE_REPORT_JSON http
```

This work builds on established ideas: [Preflate's reversible DEFLATE
reconstruction](https://github.com/microsoft/preflate-rs),
[Finesse's resemblance detection](https://www.usenix.org/conference/fast19/presentation/zhang),
[Lepton's exact JPEG recompression](https://github.com/microsoft/lepton_jpeg_rust),
and [fdeflate's PNG-oriented encoder](https://github.com/image-rs/fdeflate).
The contribution here is their measured alternatives and integration with exact
trace reconstruction and cost-aware sharing, not a claim to have invented those
underlying compression techniques.

The delivered private vault was rebuilt solely from its immutable objects, then
indexed: 1,291 snapshots are searchable. A search for the generic term `function`
returned results; no result text or source identifiers are published.
