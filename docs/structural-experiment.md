# Structural compression experiment

Question: can reversible preprocessing and value-level sharing materially reduce
an archive of the supplied Codex and Claude traces, beyond opaque-file chunking?

The frozen inputs contain 510 actual sessions: the earlier 342-file, 774,000,764
byte corpus plus 168 newly supplied Claude files (287,449,057 bytes). Private
copies and source mappings remain outside this repository. No generated sessions
or public datasets are used. Report the earlier corpus separately so added data
cannot obscure the comparison.

The primary metric is complete durable archive bytes (packs plus manifests).
Also report disposable catalog size, ingestion/restore elapsed time and bounded
working memory. Every accepted representation must reproduce original file bytes,
including compressed containers, unknown fields, whitespace and malformed input.
Measure only completed work; validate every restore outside ingestion timing.

Screen transformations independently: native Zstd reconstruction, JSON string
sharing, canonical base64 decoding, and lossless embedded-media preprocessing.
Keep a transformation only if it reduces complete storage materially (at least
5% on the relevant corpus) with explicit complexity and recovery costs. Retain
failed and rejected screens. These exploratory runs are not final performance
claims. Final comparisons use fresh vaults, three alternating trials and identical
frozen inputs; report raw samples and software/environment identifiers.

150 MB for the original corpus is an aspiration, not a stated result or a reason
to discard bytes. The archive must keep a bounded opaque fallback when an encoder
or source representation is unrecognized. Existing version-1 archives must remain
readable. A custom format must specify its reconstruction contract independently
of a particular provider's evolving JSON schema.

The completed upload was frozen at 2026-10-29 02:37:56 UTC: 1,291 files and
2,098,579,235 native bytes, including 949 newly supplied files. Nine files repeat
exact native bytes (11,424,468 bytes); they remain in the corpus as supplied.
The earlier 510-file freeze is retained as an exploratory input, not described as
the completed upload. Final measurements use the completed freeze and report the
original 342-file corpus separately.

A separate placement screen asks whether grouping text, standard-base64 binaries
and URL-safe binaries into bounded packs improves read locality. It keeps the
same logical recipe, chunk boundaries, codec level and 8 MiB pack limit. The
predeclared decision rule is no archive-size regression and at least 20% lower
complete restore time in a screening run; a selected policy then needs repeated
production runs. This is a placement change, not a new archive transform. Peak
capture buffering may rise from one to three bounded pack buffers. This screen is
motivated by the measured cost of reading shared small values from mixed packs.

Placement screen outcome: 400,356,433 archive bytes and 24.044874 seconds restoring
the original corpus, versus a 24.043739-second selected-format median. The archive
improved by about 2.5%, but the 20% restore criterion failed. Retain the one-buffer
writer. No final performance run was overlapped with this screening candidate.
