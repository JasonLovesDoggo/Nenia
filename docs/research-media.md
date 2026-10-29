# Reversible media research

Question: how much redundancy remains after lexical factoring when screenshots
and edited text are treated as related observations instead of opaque blobs?

The inputs are the frozen 342-file original corpus and the completed 1,291-file
private corpus. No generated or public inputs contribute performance results.
Private assets, mappings, and archives stay outside the repository.

Experiments, in order:

1. Inventory complete embedded images and their exact duplicates. Recover PNG
   pixels, original filters, chunk layout, and DEFLATE representation separately.
2. Implement a predictor-independent DEFLATE reconstruction recipe. Compare it
   with Preflate, including streams that Preflate cannot handle. Preserve original
   Huffman headers, token choices, stored blocks, padding, and PNG ancillary data.
3. Compare independent lossless images with bounded-reference image deltas and
   shared pixel fragments. Measure similarity rather than assuming it exists.
4. Test similarity-based text deltas after exact deduplication. Count the index
   needed for encoding separately from the data needed for decoding.
5. Integrate promising transformations into a complete archive experiment.

The primary metric is the complete durable artifact size, including recipes,
references, correction streams, fallback payloads, and checksums. Every accepted
representation must reconstruct the original bytes, including native Zstd and
PNG/JPEG encodings. Screens may report component sizes, explicitly labeled as
such; they are not archive compression claims.

Candidates must have bounded decoding and an opaque fallback. Research screens
use one trial; selected complete archives require repeated sequential runs and
independent restore verification. Report ingest/restore time, peak RSS, dependency
depth, failures, and the cost of reconstructing one selected session. A smaller
artifact does not automatically justify a production format change.

Prior work: [Preflate](https://github.com/microsoft/preflate-rs) reconstructs
DEFLATE from plaintext and prediction corrections;
[Finesse](https://www.usenix.org/conference/fast19/presentation/zhang) detects
similar chunks for delta compression. These are established techniques, not
novel contributions claimed by Nenia.
