# Actual trace benchmarks

These measurements use the user's actual trace files. Inputs are frozen outside
this repository, every measured restore is compared byte-for-byte, and reports
contain aggregates only. Trace contents, source paths, session identifiers, corpus
hashes, archive objects, and credentials are not published.

## Framework corpus

The frozen capture contains 74 nonempty files: 72 Codex traces and 2 Claude traces,
359,424,391 bytes in total. The initial inventory contained 72 files; two real Codex
files appeared before the freeze at 2026-10-28 06:09:47 UTC. No file grew during
its copy. The frozen inputs were never refreshed between comparisons.

| Provider | Files | Original bytes | Smallest | Median | Largest |
| --- | ---: | ---: | ---: | ---: | ---: |
| Codex | 72 | 357,091,688 | 49,856 | 1,200,197 | 158,856,579 |
| Claude | 2 | 2,332,703 | 165,792 | 1,166,351.5 | 2,166,911 |
| Combined | 74 | 359,424,391 | 49,856 | 1,200,197 | 158,856,579 |

Initial archival storage, in bytes. Values are medians of three independent trials.
Archives include payloads and recovery manifests; Nenia local totals also include
SQLite and configuration. No repeated history was invented for this comparison.

| Cohort | Per-file Zstd | Solid Zstd | Nenia archive | Nenia total local |
| --- | ---: | ---: | ---: | ---: |
| Claude | 192,837 | 159,639 | 161,124 | 218,565 |
| Codex | 127,248,844 | 124,317,426 | 113,210,090 | 114,082,635 |
| Combined | 127,441,588 | 124,473,459 | 113,403,978 | 114,280,619 |

For this combined corpus, Nenia's archive is 11.01% smaller than per-file Zstd and
8.89% smaller than solid Zstd. Its original-byte-to-archive ratio is 3.17:1. Claude's
two-file cohort favors solid Zstd slightly; no result is a universal compression
claim or a prediction for another user's traces.

| Cohort | Per-file median seconds | Solid median seconds | Nenia median seconds | Nenia range seconds |
| --- | ---: | ---: | ---: | ---: |
| Claude | 0.0149 | 0.0126 | 0.0346 | 0.0323–0.0390 |
| Codex | 2.5146 | 2.5315 | 3.1240 | 3.0985–3.2015 |
| Combined | 2.6069 | 2.5569 | 3.0193 | 2.9952–3.3021 |

Optional indexing added 5,632,000 local bytes for the combined corpus, bringing the
local total to 119,912,619 bytes. Its median indexing time was 1.284 seconds. The
8 MiB extracted-text cap applies separately to each source snapshot, so indexing
large traces is intentionally partial. The first report's index-size delta is
relative to initial ingestion, including any unchanged-recapture bookkeeping.

Every unchanged full recapture created zero snapshots and added zero archive bytes.
For the combined corpus it took a median 0.782 seconds. Metadata-only polling took
1.770 ms and read zero payload bytes. These are separate measurements from initial
ingestion.

During the run, eight original traces actually acquired appends: 1,878,159 new
source bytes produced 762,106 additional archive bytes. Capture took 0.083 seconds.
All eight new versions and all 74 original versions restored exactly. This is one
observed update, not a steady-state append benchmark.

The 27 measured batches produced 1,332 exact restores and zero errors. A separate
authenticated loopback HTTP push/pull transferred 111 objects and restored all 74
files into a fresh vault. Deleting the disposable SQLite catalog and reopening the
vault restored all 74 files again. HTTP testing proves local protocol correctness,
not WAN throughput or cloud durability.

Raw evidence: [aggregate corpus](benchmark-results/real-2026-10-28/corpus-aggregate.json),
[samples](benchmark-results/real-2026-10-28/samples.jsonl),
[summary](benchmark-results/real-2026-10-28/summary.json),
[environment](benchmark-results/real-2026-10-28/environment.json),
[recovery](benchmark-results/real-2026-10-28/recovery.json), and
[observed growth](benchmark-results/real-2026-10-28/observed-growth.json).
The run used source commit `93de9ffb39dd499777fc05cd6850ddebd867c7a9`, Zstd 1.5.7,
and Rust 1.88.0. Source-code and executable hashes identify the measured build;
those hashes do not identify private trace contents.

## Incoming Mac native files

All 268 transferred files passed their supplied SHA-256 and length checks, with no
missing or unlisted traces. The transfer contains 251 native `.jsonl.zst` files
(362,748,802 bytes) and 17 raw `.jsonl` files (51,827,571 bytes): 414,576,373 native
bytes total. These containers must restore exactly. Their compressed size must not
be confused with the size of decoded JSONL.

The Mac baseline and controlled compression experiments are in progress. The new
combined corpus includes the original frozen Framework bytes; it does not recapture
those changing live sources.

## Method and reproduction

[examples/real_benchmark.rs](../examples/real_benchmark.rs) captures actual source
files and runs the experiment. Initial comparisons use Zstd level 9 and an 8 MiB
window, with checksums and declared source lengths. Nenia uses 16/64/256 KiB
minimum/target/maximum content-defined chunks and 8 MiB raw packs. Baseline mapping
manifests preserve file boundaries and content hashes privately. No fixtures,
reconstructed histories, or public datasets contribute to these results.

Each trial starts with a fresh archive. Candidate order rotates across three
trials. Timing includes source reads, hashing, compression, recovery metadata,
file and directory synchronization, and Nenia's initial catalog transactions and
close. Input freezing, correctness checks, unchanged captures, indexing, and
recovery are outside initial-ingestion timing. Baselines publish their mapping
manifest only at completion; these timings do not compare equivalent incremental
acknowledgment or random-access behavior. Source data is warm in the OS cache;
shared-host load and power state are uncontrolled. Sizes count file lengths,
not allocated filesystem blocks or cloud billing. No failed trials were excluded.

Run from the repository root, choosing a private directory outside the repository:

```sh
cargo build --release --example real_benchmark
private="$HOME/.cache/nenia-benchmark/private-run"
report="docs/benchmark-results/my-real-run"
target/release/examples/real_benchmark capture --private-root "$private" --report "$report"
target/release/examples/real_benchmark run --private-root "$private" --report "$report" --trials 3
```

A capture refuses to overwrite an existing private corpus. A run refuses to
replace existing samples. Detailed errors remain in the private working directory.
Only the aggregate report directory is suitable for publication. Native `.jsonl.zst`
bytes are preserved; decoding them, when measured separately, is a diagnostic with
a different byte-preservation contract.
