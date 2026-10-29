> Historical format-1 report (2026-10-28). See [current results](benchmarks.md).

# Benchmarks on actual sessions

The final run uses **342 of the user's actual sessions**, including the complete
Mac snapshot. No generated workloads or public datasets contribute to these results.
All source content, private paths, session IDs, corpus hashes, and archive objects
stay outside this repository. Published evidence contains aggregate measurements only.

## Inputs

| Corpus | Files | Native file bytes | Decoded JSONL bytes |
| --- | ---: | ---: | ---: |
| Framework: 72 Codex + 2 Claude | 74 | 359,424,391 | 359,424,391 |
| Mac: 251 compressed + 17 raw Codex | 268 | 414,576,373 | 1,377,078,046 |
| **Combined** | **342** | **774,000,764** | **1,736,502,437** |

All 268 Mac files passed the supplied SHA-256 and length checks. The Framework
files were frozen before comparison; the Mac capture included those frozen copies
without rereading their changing originals. There were no missing or unlisted Mac
traces. Native `.jsonl.zst` bytes are preserved exactly, not silently transcoded.

## Final results

Median of three fresh trials, with Zstd level 9 throughout. Archive sizes include
payloads and recovery metadata. Local totals additionally include SQLite and config.

| Layout | Archive bytes | Local total bytes | Initial ingest seconds |
| --- | ---: | ---: | ---: |
| Per-file Zstd | 508,865,307 | 508,865,307 | 3.867 |
| Solid Zstd | 504,077,858 | 504,077,858 | 3.682 |
| Nenia | 490,614,637 | 492,998,621 | 6.573 |

Nenia's archive is **36.61% smaller than the native source files**,
3.59% smaller than per-file Zstd, and 2.67% smaller than
solid Zstd on this corpus. Most Mac inputs were already compressed, which limits
additional compression. Nenia also maintains independent snapshots and a catalog;
the simpler baselines finish faster. This is not a universal compression claim.

The selected policy uses 32 KiB average content-defined chunks for raw logs and
256 KiB fixed chunks for sealed Zstd logs, with 8 MiB raw packs. The previous
64 KiB policy used about 496.4 MB total locally; the selected policy uses about
493.0 MB. [Format analysis and controlled experiments](format-analysis.md)
explain why levels 15/19, smaller chunks everywhere, and encoder-dependent
transcoding were not selected.

Optional full-text indexing added 25,092,096 local bytes
(median 5.274 seconds), bringing the local total to
518,090,717 bytes. The index caps extracted text at
8 MiB per snapshot; compressed source scanning is bounded to 256 MiB decoded bytes.
One Mac file exceeds that scan budget and receives an explicit metadata warning.
Its archived bytes remain complete.

## Correctness and observed updates

- Nine measured batches completed **3,078 exact file restores**, with zero errors.
- Authenticated loopback HTTP push/pull transferred 427 objects and restored all
  342 files into a fresh vault. Deleting SQLite and rebuilding restored all 342 again.
- Every unchanged full recapture added zero snapshots and zero archive bytes.
  Median full rescan was 2.793 seconds;
  the size/mtime fast path was 7.050 ms and read zero payload bytes.
- Eight original live traces had actually grown by 83,433,867 bytes. Their new
  snapshots added 29,214,851 archive bytes. All eight new versions and all 342
  original versions restored exactly. Two original live source paths were no
  longer present; their frozen snapshots remained restorable. No update history
  was fabricated.
- The installed Claude callback was separately invoked with an existing private
  2,166,911-byte transcript. It returned in about 10 ms, the detached worker
  captured it, and restore matched exactly. The source remained untouched. This
  tested the official event envelope directly, without making a model request.

## Method and evidence

Production archive code: `f625fef`; final harness: `c5ab6c6`; Rust 1.88.0,
Zstd 1.5.7, Linux. Each trial starts fresh and candidate order rotates. Timings
include source reads, hashing, compression, metadata extraction, durability calls,
and catalog commit/close. Correctness checks, indexing, unchanged rescans, and
recovery are measured separately. The host is shared and source data is warm in
OS cache; no cold-storage or sustained-service performance claim is made. File
lengths are reported, not filesystem allocation or cloud billing.

The baselines preserve file boundaries and exact bytes, but publish recovery
mapping only at completion. Their timings do not measure equivalent incremental
acknowledgment, search, or random-access behavior.

[Final samples](benchmark-results/real-final-2026-10-28/samples.jsonl) ·
[Summary](benchmark-results/real-final-2026-10-28/summary.json) ·
[Environment and code hashes](benchmark-results/real-final-2026-10-28/environment.json) ·
[Recovery](benchmark-results/real-final-2026-10-28/recovery.json) ·
[Observed growth](benchmark-results/real-final-2026-10-28/observed-growth.json) ·
[Actual hook check](benchmark-results/real-hook-2026-10-28.json).

Earlier Framework-only and native Mac baselines remain in `benchmark-results/`.
An initial final run completed its timed and recovery checks but failed while
sampling a live source that had disappeared. Its evidence is retained under
`real-final-incomplete-2026-10-28`; the harness now counts unavailable originals
and samples open file handles. No timed samples were discarded or relabeled.

## Reproduce with your own files

```sh
cargo build --release --example real_benchmark
private="$HOME/.cache/nenia-benchmark/private-run"
report="docs/benchmark-results/my-real-run"
target/release/examples/real_benchmark capture --private-root "$private" --report "$report"
target/release/examples/real_benchmark run --private-root "$private" --report "$report" --trials 3
```

Use `capture --source-dir DIRECTORY --cohort LABEL` for another source directory,
and `--include-corpus PREVIOUS_PRIVATE_ROOT` to combine frozen inputs. Use
`run --only-cohort combined` to compare the combined collection. Both raw JSONL
and native compressed JSONL are supported. The harness refuses to put private
inputs inside the checkout or overwrite an existing corpus/report. Only aggregate
report directories are suitable for publication.
