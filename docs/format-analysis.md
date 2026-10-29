# Compression beneath the JSON wrappers

Whole-record deduplication scarcely helped the original 342-file corpus: only
745 bytes of complete records repeated. Timestamps and envelopes change while
large values repeat. The design now works below those envelopes.

The original files occupy 774,000,764 bytes on disk and 1,736,502,437 bytes after
native Zstd decoding. The completed upload expands the measured corpus to 1,291
files / 2,098,579,235 native bytes. All inputs are the user's actual traces.

## What changed

- Canonical JSON strings are shared in their unescaped form, including nested
  tool-result JSON. A lexical recipe reconstructs their exact original spelling.
- Canonical base64 is decoded before deduplication. Four-character literal bridges
  preserve triplets across content-defined boundaries, avoiding alignment losses.
- Native Codex Zstd is opened only after its entire compressed bitstream is proven
  reproducible by the pinned Zstd 1.5.7 profile. Unknown profiles stay opaque.
- Compact references omit redundant per-chunk hashes. Pack and whole-snapshot
  authentication remain mandatory; SQLite can still be deleted and rebuilt.
- Unchanged full scans check file identity before parsing or transforming. This
  also prevents unused transformed objects when reading a legacy archive.

These rules depend on byte representations, not a fixed list of Codex or Claude
fields. Unknown fields, malformed JSON, binary bytes, whitespace, and original
compressed containers remain recoverable. The [format contract](FORMAT.md) specifies
limits and the historical encoder compatibility obligation.

## Screens, including rejected ideas

These are exploratory runs, not the final repeated comparison. Some ran alongside
other development work; their timings do not establish isolated performance.

| Original 342 files | Complete archive bytes |
| --- | ---: |
| Previous format, published median | 490,614,637 |
| First lexical-sharing prototype | 415,428,050 |
| Tiny chunks with full hash references | 419,171,924 |
| Tiny chunks with compact references | 403,326,252 |
| Normal chunks with base64 bridges and compact references | 409,884,584 |
| Integrated application, level 9 | 410,635,397 |
| Integrated application, level 19 | 394,654,410 |

The tiny-chunk screen needed 358,751 unique chunks versus 123,814 for the normal
bridged policy, and decompressed about 13.5 GB of packs during restore versus
5.4 GB. Its modest remote saving did not justify the larger catalog and read work.
Level 19 recovered another 16.0 MB in the integrated application but took about
150 seconds to ingest, versus 25 seconds at level 9. The default remains level 9.

A later pack-placement screen kept text and the two base64 families in separate
bounded packs. It used 400,356,433 archive bytes, but restore took 24.045 seconds
versus the selected writer's 24.044-second median. It missed the predeclared 20%
restore improvement and added two pack buffers, so it was rejected. The exact
variant is retained as `experiments/typed-packs.patch` against `0173121`.

### Deeper image coding

We also tested all 770 distinct PNGs extracted from the initial 510-file freeze:
254,657,749 original PNG bytes and 3,929,425,129 inflated scanline bytes. Experiments
included deduplicated filtered rows, unfiltered pixels, DEFLATE reconstruction,
and lossless WebP with exact transparent RGB preservation.

604 DEFLATE streams reconstructed exactly with Preflate 0.7.6; 166 used the opaque
fallback. Every tested WebP round trip preserved pixel bytes. Adaptive pixel coding
estimated 183,697,085 payload bytes including reconstruction information, compared
with 220,963,235 bytes for deduplicated original PNGs compressed with Zstd. This is
**an isolated media screen**, without a complete Nenia asset format, catalog or
whole-archive recovery test. It is not an advertised archive result. The prototype
is retained under `experiments/media`; it adds no production dependency.

### The 150 MB target

We did not achieve 150 MB while retaining every byte. In a separate grouping
experiment on the original corpus, extracted standard-base64 binaries still used
260.9 MB at Zstd level 9; URL-safe binaries, predominantly encrypted model state,
used another 86.4 MB. Text, structural literals and recovery metadata are additional.
These measurements describe tested representations, **not a theoretical lower
bound or a claim that further compression is impossible**. Dropping attachments
or encrypted fields would change the backup contract, so these results never count
such omissions as compression.

## Reproduction

[Final measurements](benchmarks.md) distinguish complete archives, local catalog
costs, CPU time and correctness from these screens. Aggregate inputs, samples and
codec experiments are in [the evidence directory](benchmark-results/structural-2026-10-29/).
Private source copies, content identities, payloads and per-session metadata remain
outside the repository.

`examples/structure_probe.rs` compares lexical representations on a private
`real_benchmark` corpus. `tools/analyze_structure.py` inventories decoded values;
`tools/group_structure_probe.py` groups an early private prototype archive by
representation. The standalone PNG screen takes a private concatenated PNG stream
and an aggregate report path; it requires Rust 1.89 or newer. The production binary
continues to require Rust 1.88. [Earlier opaque-format experiments](format-analysis-v1.md)
are retained as historical evidence, rather than current design guidance.
