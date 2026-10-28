# What the actual Codex files taught us

The supplied Mac snapshot contains 268 real sessions: 251 native `.jsonl.zst`
files and 17 raw `.jsonl` files. All supplied checksums passed. Source files were
kept private and unchanged; only aggregate measurements are published.

## Native format

- Native files occupy 414,576,373 bytes; decoded JSONL occupies 1,377,078,046 bytes.
- All 251 compressed files declare their decoded sizes, use windows no larger
  than 2 MiB, and have no external dictionary or frame checksum.
- There are 225,017 JSONL records. About 355.8 MB of decoded string bytes are image
  data URIs. Images help explain why stronger text compression has limited gains.
- Long string values contain 314.5 MB of exact repetition. This is a structural
  observation across JSON fields, not a promise of that much additional savings.
- One file decodes to 483,044,758 bytes. Its archive remains complete; metadata
  and indexing scans stop at the documented 256 MiB budget and report a warning.

This exposed an actual discovery gap: the first implementation ignored native
`.jsonl.zst` files. Discovery, metadata extraction, and optional text indexing now
handle them. Restoration still reproduces the original compressed bytes, including
malformed containers that can only be preserved as opaque data.

## Compression experiments

These screening runs used the same frozen 342-file Framework + Mac corpus:
774,000,764 native bytes. Each completed restore was checked against the input.
The codec sweep holds the original 64 KiB average chunking policy fixed.
Times are short, warm-cache, shared-host batch measurements.

| Zstd level | Archive bytes | Local total bytes | Initial ingest seconds |
| --- | ---: | ---: | ---: |
| 3 | 499,978,948 | 502,313,765 | 4.877 |
| 9 | 494,096,257 | 496,439,266 | 6.589 |
| 15 | 492,250,908 | 494,606,206 | 35.733 |
| 19 | 491,537,725 | 493,860,255 | 90.729 |

Level 9 remains the default. Level 15 saves only 0.37% of archive space versus
level 9 while taking over five times as long; level 19 saves 0.52% at nearly
14 times the ingestion time. These are one-trial screening observations, not
sustained throughput estimates.

At level 9, we then varied chunking. The first three rows are one-trial screens;
the selected policy is the median of three fresh trials.

| Chunk policy | Archive bytes | Local total bytes | Initial ingest seconds |
| --- | ---: | ---: | ---: |
| 64 KiB average everywhere | 494,096,257 | 496,439,266 | 6.589 |
| 32 KiB average everywhere | 491,191,590 | 495,168,903 | 6.993 |
| 16 KiB average everywhere | 489,273,111 | 496,564,088 | 7.392 |
| **32 KiB raw / 256 KiB fixed compressed** | **490,614,920** | **492,978,409** | **6.534** |

The selected policy reduces both remote and total storage without increasing
measured ingestion time materially. Smaller raw chunks help reuse; larger fixed
chunks avoid rolling-hash work and excess metadata on sealed compressed streams.
The 16 KiB policy saves payload space but loses the benefit to catalog overhead.
All policies retain the same bounded pack format and maximum chunk size, so old
archives remain readable. The final production build is separately rerun in the
[benchmark report](benchmarks.md).

## Why native files are not silently transcoded

All 251 native compressed files re-encoded byte-for-byte using Zstd 1.5.7,
level 3, a pledged content size, no checksum, and a single-thread encoder.
We checked the complete SHA-256 and length of every encoded output.

That makes an encoder-recipe scheme technically possible: deduplicate decoded
JSONL and regenerate the original Zstd stream during restore. We measured the
space before deciding. Decoded JSONL stored through Nenia used 333,653,111 archive
bytes, versus about 380.7 MB for exact native-container storage: roughly 12.4%
less. A single solid Zstd stream of the decoded data used 324,569,606 payload bytes,
but has no independent-file recovery metadata.

We did **not** make ordinary restores depend on preserving a historical encoder
implementation for that saving. Native compression parameters and future codec
versions can differ. Exact original bytes, simple recovery, and a stable read
format remain the default. These decoded-data measurements are diagnostics;
they are not included in the advertised native backup sizes.

## Evidence and reproduction

Aggregate samples, settings, executable/code fingerprints, and the native-format
analysis are under [optimization-2026-10-28](benchmark-results/optimization-2026-10-28/).
No generated workloads or public datasets were used.

The level sweep used `real_benchmark run --only-cohort combined --only-candidate
nenia --trials 1 --skip-index --skip-recovery --skip-growth --level LEVEL` against
one private frozen corpus. Use a new `--report` directory for each run. The
pre-optimization implementation is commit `ca51793`; the selected implementation
is `f625fef`. Isolated chunking variants changed only `MIN_CHUNK_SIZE` to one
quarter of the chosen average and `AVG_CHUNK_SIZE`, preserving the 256 KiB maximum.
The fixed-compressed variant additionally used `Chunker::new_fixed` for recognized
Zstd sources. The final production code and its tests contain that policy.

`tools/analyze_native_traces.py` reproduces the structural and re-encoding checks
on Linux with Python 3.11+, libzstd and the Zstd 1.5.7 CLI. Supply `--source-dir`,
`--decoded-dir` outside the checkout, and an aggregate `--report` path. Decoded
copies and any source mapping stay private. The tool never executes trace content.
