# Importing traces

Nenia reads saved traces; it does not wrap, launch, or modify an agent. Automatic
discovery needs no provider credentials. It scans the current user's normal
session stores recursively, including Claude subagent transcripts and Codex's
archived sessions, including native `.jsonl.zst` files.

| Source | Default location beneath the user's home | Environment override |
| --- | --- | --- |
| Codex | `.codex/sessions`, `.codex/archived_sessions` | `CODEX_HOME` replaces `.codex` |
| Claude Code | `.claude/projects` | `CLAUDE_CONFIG_DIR` replaces `.claude` |
| Pi | `.pi/agent/sessions` | `PI_CODING_AGENT_DIR` replaces `.pi/agent`; `PI_CODING_AGENT_SESSION_DIR` overrides the sessions root |
| Plaud / other exports | Explicit file or folder | No proprietary integration |

Paths use native filesystem APIs on Windows, macOS, and Linux. A leading `~` in
an environment override expands to the supplied home. Nenia discovers JSONL and
NDJSON in provider stores, including their `.zst` variants. An explicitly supplied
directory also includes JSON, TXT, and Markdown files and their `.zst` variants,
case insensitively. An explicitly supplied file can have
any extension. Discovery returns canonical paths in deterministic order and
deduplicates overlapping roots. Missing default roots are normal. Symlinked
files, directories, roots, and root ancestors are skipped.

If an agent was launched with a custom command-line session directory or Pi's
project-specific `sessionDir` setting, pass that directory explicitly. Nenia
does not inspect provider settings or read global prompt-history files. Cloud
sessions must first be downloaded/exported. Plaud support means archiving a
local text/Markdown/JSON export and optionally labeling it `plaud`; it does not
claim to log in, download recordings, or interpret a proprietary database.

## Metadata and search

The `auto` adapter recognizes common Codex `session_meta` / `response_item`
records, Claude `sessionId` conversation records, and Pi's versioned `session`
header. Metadata includes source, session ID, title, original working directory,
first available timestamp, record count, malformed-record count, oversized-record
count, and source-decoding warnings. A title comes from explicit metadata or the first user prompt,
then the filename. Provider-specific fields are best-effort compatibility
hints, not a stable upstream schema guarantee. Explicit source labels override
automatic detection.

All input bytes are preserved separately from metadata. Invalid UTF-8, partial
last records, unknown fields, newer formats, and malformed JSON do not stop
archival. Actual file read errors are reported. Empty files are valid archives.
Record count means nonempty physical lines for JSONL/text; a JSON document counts
as one record, or one record per top-level array element. It is not a count of
model turns or tokens. The full Pi branch tree is preserved, not just its active
branch.

Metadata parsing retains at most 1 MiB for one JSON record. Larger records are
skipped for metadata while scanning continues. Whole JSON documents up to 1 MiB
are parsed directly, including pretty-printed JSON. Larger documents fall back
to line processing, so their metadata and search coverage may be incomplete.
Strings retained as metadata are bounded; titles are at most 160 characters.

Optional search text contains readable string values and plain text, with
common image/audio/base64/signature fields excluded. It is capped by the caller
(the CLI uses 8 MiB per snapshot), respects UTF-8 boundaries, and omits oversized
JSON records. Raw records with invalid UTF-8, NULs, or many control characters
are excluded from the index. This index is a convenience, not a complete or sanitized copy: prompts,
tool arguments, and output can still contain secrets. The exact raw archive is
never redacted or truncated by the index policy.

## Native compressed sources

Native Codex `.jsonl.zst` files remain compressed in the archive. Their hashes,
byte counts, chunk references, `cat` output, and restores describe the original
compressed container exactly. Restore to a `.jsonl.zst` filename and use a Zstd
decoder if you want a separate plaintext copy. Nenia does not silently rewrite
these files as uncompressed JSONL.

Metadata and optional search decode recognized textual `.zst` exports, including
concatenated frames. Decoding accepts at most a 64 MiB frame window and examines
at most 256 MiB of decoded source content. The normal per-record and index-text
limits still apply. A scan that reaches the decoded-byte budget retains prefix
metadata and records a warning; later content and frame completion are unverified
by that scan. These limits do not truncate the archived compressed bytes.

Invalid/truncated source compression is preserved as opaque bytes with a metadata
warning and no decoded body text in the optional index. Actual source-read errors
and errors reading corrupt Nenia packs still fail the operation, rather than
being mistaken for an unsupported source format.

Header-only inspection of the authorized Mac sample found 251 native Zstd files
with windows no larger than 2 MiB. One declared 483,044,758 decoded bytes, exceeding
the 256 MiB scan budget; its full compressed contents remain archivable. No trace
contents or private filenames were added to these fixtures or documentation.

## Sources and verification

The discovery paths were checked against the following primary sources:

- [OpenAI's troubleshooting documentation](https://learn.chatgpt.com/docs/reference/troubleshooting)
  documents `CODEX_HOME`, active sessions, and archived sessions. The
  [Codex app-server documentation](https://learn.chatgpt.com/docs/app-server)
  describes persistent JSONL thread logs and archive/unarchive behavior.
- [Claude's session documentation](https://code.claude.com/docs/en/agent-sdk/sessions)
  documents the project-directory JSONL layout and `CLAUDE_CONFIG_DIR`.
  [Claude's hooks reference](https://code.claude.com/docs/en/hooks)
  shows nested subagent transcript paths.
- [Pi's session format](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/session-format.md)
  specifies the session header, message records, branch tree, and session name
  entries. [Pi's session guide](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/sessions.md)
  documents directory selection and exported JSONL.

Unit fixtures cover each adapter, JSON arrays,
pretty JSON, Markdown, malformed Unicode, interrupted writes, oversized records,
bounded indexing, environment roots, and symlink handling. No personal trace
contents are used or checked into this repository. The adapter and its tests
also pass cross-target `cargo check` for Windows MSVC and macOS ARM64. Runtime
verification on this development host is Linux; native Windows and macOS
execution still needs CI.

Storage and restore measurements use actual local sessions and public traces; see
[the real-trace benchmark](benchmarks.md). Private session contents are not published.
