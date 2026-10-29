# Benchmarks on actual traces

The newer cold-archive study stores the original corpus in **299,254,562 bytes**
and the complete corpus in **556,793,091 bytes**. See [cold research](research-media.md)
for full accounting, rejected candidates, and decoder/recovery validation. The
measurements below describe the fast capture format retained by ordinary backup.


The current format saves **80.0 MB on the original corpus** and **144.2 MB on the
completed upload** compared with the previous Nenia format. Every measured restore
matches the original file byte for byte, including native compressed containers.
We did not reach the proposed 150 MB target without dropping data.

All inputs are the user's frozen Codex and Claude traces. No generated workload
or public dataset contributes to these results. Trace content, filenames, source
identities, original content hashes, credentials and archive objects stay private.

## Inputs and results

The original 342 files occupy 774,000,764 native bytes. The complete upload adds
949 JSONL/Zstd trace files, for **1,291 files / 2,098,579,235 native bytes**. Discovery
excluded the transfer manifest and checksum files. All 939 files listed in the new
upload's SHA-256 manifest passed; ten additional JSONL exports were not listed.
All frozen inputs are checked against their private BLAKE3 identities before each
run. Nine whole-file duplicates (11,424,468 bytes) remain as supplied.

Median of three fresh, alternating trials at compression level 9. Archive bytes
include packs and manifests. Local totals also include SQLite and configuration.

| Corpus and implementation | Archive bytes | Local total bytes | Ingest seconds |
| --- | ---: | ---: | ---: |
| Original 342 files, previous format | 490,614,709 | 492,990,831 | 6.578 |
| Original 342 files, current format | 410,636,397 | 431,722,702 | 26.140 |
| Complete 1,291 files, previous format | 850,990,974 | 858,954,028 | 19.089 |
| Complete 1,291 files, current format | 706,792,806 | 756,506,055 | 53.607 |

The current archive is **16.30% smaller on the original corpus** and **16.94%
smaller on the completed upload**. It uses more initial CPU time and a larger
local deduplication catalog. The catalog is disposable and is not uploaded.
Initial ingest takes roughly 4.0× / 2.8× as long, respectively. These are warm-cache
batch observations on one shared workstation, not sustained service capacity.

A separate one-trial level-19 screen reached **394,654,410 archive bytes** on the
original corpus, with a 415,765,292-byte local total and 149.8-second ingestion.
It remains available through `nenia init --compression-level 19`; level 9 remains
the default. Earlier per-file/solid Zstd comparisons are in the
[historical format-1 report](benchmarks-v1.md), rather than mixed into this paired run.

## Recovery, updates and memory

- Twelve measured batches completed **9,798 exact file restores**, with zero errors.
- All **638 native Zstd files** in the complete corpus were proven reproducible
  and restored to their original compressed bytes. Unknown encoder profiles still
  have an opaque fallback. 1,289 files used the new representation; two small
  raw files used the legacy representation.
- Authenticated loopback HTTP push/pull transferred **1,531 objects / 706,792,806
  bytes** and restored all 1,291 files. Deleting SQLite and rebuilding restored all
  1,291 again. This tests recovery, not WAN performance or an actual cloud account.
- Eight genuinely appended live sources added **90,112,628 input bytes** and
  **30,911,969 archive bytes**. All eight new versions and all 1,291 initial versions
  restored exactly. Two old live paths were unavailable; their frozen backups
  remained intact. No update history was fabricated.
- Unchanged full scans added zero snapshots and zero archive bytes. Median scan
  time fell from 2.809 to 0.303 seconds on the original corpus, and from 7.769 to
  0.776 seconds on the complete corpus. The metadata fast path read zero source
  payload bytes and took 27.7 / 77.4 ms with the current format.
- Current-format restore medians were **24.044 seconds** for the original corpus
  and **80.781 seconds** for the complete corpus, including bytewise comparison
  with the private originals. The old harness did not separately time restore.
- Optional indexing added **44,449,792 local bytes** in 50.104 seconds in the final
  complete-corpus trial. Indexed local storage was 800,955,847 bytes. Extracted
  text is capped at 8 MiB per snapshot; oversized records and recognized media
  fields are omitted. Metadata inspection of native containers has a separate
  256 MiB decoded budget. Normalized snapshots index their logical stream directly.

Capture/restore processes peaked at **151.2 MiB RSS** on the complete
corpus. The combined in-process indexing, HTTP client/server and recovery run
peaked at **968.2 MiB RSS**. That larger value is not a server-only memory
measurement; do not infer a smaller deployment memory requirement from the
per-pack bounds. RSS covers the whole process, not only the timed ingestion region.

## Method and reproducibility

Archive implementation: `0173121`; previous implementation: `f62a513`. Rust 1.88.0,
Zstd 1.5.7, Linux, Ryzen 5 7640U, 12 logical CPUs, approximately 23 GiB RAM, NVMe.
Each run starts with a fresh vault and identical frozen inputs. Candidate order
alternates; processes run sequentially. Timed ingestion includes reads, identity
checks, normalization proofs, compression, metadata extraction, fsync and catalog
commit/close. Restore validation, unchanged scans, indexing and recovery are
separate. File lengths are measured, not allocated blocks or cloud billing.

The host was shared, its power governor was `powersave`, and thermal state was not
controlled. Source data was warm in the OS cache. Raw min/median/max samples and
whole-process resource measurements are retained; no cold-storage claim is made.

```sh
python3 tools/compare_formats.py \
  --baseline-binary /private/baseline/real_benchmark \
  --baseline-repo /private/checkout-f62a513 \
  --current-binary target/release/examples/real_benchmark \
  --current-repo . \
  --original-corpus /private/original-corpus \
  --complete-corpus /private/complete-corpus \
  --report-dir docs/benchmark-results/new-run
python3 tools/summarize_comparison.py docs/benchmark-results/new-run
```

Build each benchmark binary from its recorded checkout, with `cargo build --locked
--release --example real_benchmark`, and put that toolchain on `PATH`. The driver
uses Linux `/usr/bin/time`; direct harness runs are available on other platforms.
`real_benchmark capture` creates private frozen inputs; source mappings remain
outside the repository. Report directories refuse overwrite. Completed comparisons consolidate raw samples,
environments and resource observations into `runs.jsonl`; the summarizer regenerates
the summary without keeping dozens of duplicate aggregate files.

[Summary](benchmark-results/structural-2026-10-29/final/comparison-summary.json) ·
[Run order](benchmark-results/structural-2026-10-29/final/protocol.json) ·
[Hardware](benchmark-results/structural-2026-10-29/hardware.json) ·
[Recovery](benchmark-results/structural-2026-10-29/final/recovery.json) ·
[Actual growth](benchmark-results/structural-2026-10-29/final/observed-growth.json) ·
[Format experiments](format-analysis.md).

Earlier screens and the complete comparison before the read-path review remain
under `structural-2026-10-29`, clearly separated from the selected implementation.
The PNG and pack-placement candidates were not selected; their payload estimates
are never substituted for complete-archive results.
