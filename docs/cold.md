# Cold archives

Nenia 0.2 can restore, verify, index, serve, push, and pull cold vaults using the
same commands and object-store layout as ordinary vaults. Upgrade clients and
servers before transferring format-2 packs.

The cold compiler is experimental. Normal capture still uses the fast lexical
format. Cold compilation analyzes a frozen collection, separates images from
text, and chooses shared image representations. It writes a **new vault** and
preserves its inputs. The current research entry point is
[`cold_import`](../examples/cold_import.rs); preparation and measurements are
explained in [the research record](research-media.md).

Once a cold vault exists, its encoded image bodies can share ordinary packs:

```sh
nenia --vault ./cold-vault compact --output ./packed-vault
nenia --vault ./packed-vault verify
nenia --vault ./packed-vault list
nenia --vault ./packed-vault restore <snapshot> --output session.jsonl
```

`compact` repacks existing cold image bodies. It does not run the experimental
image optimizer on ordinary captures. It preserves every manifest's source
identity and leaves the source vault intact. The destination is a normal Nenia
vault: configure its remote with `connect`, then use `push`.

The portable reader reconstructs original PNG and JPEG files, including their
compressed bitstreams, metadata, filters, and chunk layout. Native Codex Zstd
wrappers are also reconstructed exactly. Unknown source encodings remain opaque.
The default build excludes the legacy Preflate research decoder.

Cold restores spend CPU recreating historical image encodings. A cold vault is
appropriate for infrequently accessed history; archive size alone is not a
throughput claim. The original source logs and previous vaults are never removed
by compilation or compaction.
