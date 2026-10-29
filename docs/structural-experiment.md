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
