# Claude Code event hooks

Claude backups can run when a turn finishes and when a session ends. No watcher
or polling process is needed. First configure the spool's remote with
`nenia connect`, then run the installed binary from its permanent location:

```sh
nenia hooks install claude
```

The installer merges `Stop` and `SessionEnd` handlers into
`~/.claude/settings.json`, honoring `CLAUDE_CONFIG_DIR`. It preserves other
settings and hooks, saves the original bytes to a private
`settings.json.nenia-backup-<hash>.json` file, and atomically replaces the settings.
Repeated installation makes no duplicate handlers or unnecessary edits. Existing
settings symlinks retain their targets; dangling links or invalid JSON are left
untouched. An advisory installer lock and a final content check reduce concurrent
edit conflicts, but another settings writer does not participate in a filesystem
compare-and-swap transaction.

Each handler names the absolute Nenia executable and passes arguments through
Claude's documented `command` + `args` exec form. There is no shell parsing, so
spaces, apostrophes, dollar signs, and Windows path separators remain literal.
The handler also pins the absolute spool path; run the installer again after
moving the executable or selecting a different spool with `--vault`.

## What happens on an event

`nenia hook claude` accepts the official JSON event on stdin. It validates the
event and queues only its `transcript_path`; it never scans the rest of your
Claude directory. The queue contains path metadata, not the event's conversation
text. It accepts regular `.jsonl` transcript files, with absolute paths, `~`
paths, or relative paths accompanied by an absolute `cwd`.

The callback synchronously writes one small durable queue record, starts a
detached worker if needed, and returns. It does no compression or network upload
on Claude's execution path. The callback produces no stdout and returns success
even when it reports an error on stderr; the installed handler also specifies
`onFailure: continue`. Its five-second hook budget is for enqueue/startup only.

This short synchronous callback is intentional. Claude documents that unfinished
async hooks can be killed when `claude -p` exits, and SessionEnd has a short
default timeout. A detached worker survives the hook callback's exit instead of
depending on Claude's background-hook lifetime. Unix workers create a new session
with `setsid`; Windows workers use detached process and process group flags. All
worker standard handles are disconnected from Claude.

Workers coalesce concurrent events using a separate lock. They wait up to ten
minutes for a busy vault and retain queued requests if capture still cannot run.
A shared queue lock protects the worker's final empty check and lock release, so
an event arriving as the worker exits is not silently stranded. Events for the
same transcript in one batch are captured together; this preserves its latest
bytes, rather than promising a separate snapshot of every turn.

After capture is durable, the worker removes those queue records and pushes the
spool to its saved connection. It uses the URL and token saved by `nenia connect`,
not a potentially different `NENIA_REMOTE` inherited from Claude. An offline
upload leaves the compressed archive intact. Each upload cycle has a five-minute
time budget. New events retry pending uploads; there is no idle retry daemon.
The source transcript must remain available until its queued capture completes.

## Retry, inspect, and remove

To process a retained event or retry an offline upload without another Claude
turn:

```sh
nenia hooks drain
```

`nenia push` also retries already captured archive objects. It does not process
pending hook requests. Pending requests live in `<vault>/hooks/pending/`; the
latest worker result is `<vault>/hooks/last-run.json`. `nenia list`, `nenia verify`,
and ordinary restore commands work on hook-created snapshots.

```sh
nenia hooks uninstall claude
```

Uninstall removes only handlers with Nenia's managed marker and command argument
shape. It preserves the archived data, pending queue, settings backups, and all
other hooks. An already running detached worker may finish its current backup.

## Compatibility and verification

The implementation was checked against local Claude Code **2.1.296**, including
its installed settings schema, and these primary references:

- [Claude hooks reference](https://code.claude.com/docs/en/hooks): Stop and
  SessionEnd stdin fields, exec-form arguments, failure behavior, timeout and
  async lifecycle semantics.
- [Claude settings documentation](https://code.claude.com/docs/en/settings): user
  settings location, `CLAUDE_CONFIG_DIR`, and live settings reload. Current Claude
  versions reload hooks when settings change; use `/hooks` to inspect the result.

Use a Claude version that supports exec-form `args` and `onFailure`; older
versions may need upgrading. Tests use synthetic official-shaped payloads and
real Nenia processes, covering setting preservation, idempotence, path characters,
detachment, busy-vault handoff, offline recovery, and exact remote restoration.
Linux execution is verified; native Windows/macOS hook lifecycle execution remains
for their CI/device environments. The handlers cover the main transcript supplied
by Stop and SessionEnd; separate subagent transcript files are not automatically
included by these two hooks.
