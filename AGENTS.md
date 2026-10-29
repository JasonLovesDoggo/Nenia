# Nenia

Build a small Rust trace archive. Preserve exact source bytes. Immutable content-addressed
packs and self-contained manifests are the source of truth; SQLite is disposable.
Never delete source traces or archive objects automatically. Bound decompression and memory.
Check hashes, version formats, write objects atomically before catalog transactions.

Use Rust 1.95.0 for the workspace and CI (`cargo +1.95.0`).
Run fmt, focused tests, then full tests and clippy before release. Commit working milestones.
No commit footers or co-author trailers. Agents share the checkout: stage only owned files.

