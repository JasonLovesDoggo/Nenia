# Nenia

Compressed, lossless backups for Codex, Claude Code, and Pi sessions.
One Rust binary, your own server or S3 bucket, and an optional search index.

## Server

```sh
git clone https://github.com/JasonLovesDoggo/Nenia.git
cd Nenia
export NENIA_TOKEN="$(openssl rand -hex 32)"
docker compose up -d --build
```

Keep the token for your devices. Expose port 7433 through HTTPS or your private
network. [Deployment details](docs/deployment.md).

## Device

```sh
cargo install --git https://github.com/JasonLovesDoggo/Nenia --locked
export NENIA_TOKEN="your-server-token"
nenia connect https://traces.example.com
nenia hooks install claude
nenia backup
```

Claude hooks capture each completed turn and session without polling.
For other agents, use `nenia run -- codex`, or back up an export with
`nenia backup ./sessions`. Captured data stays in the local spool while offline.

## Restore

```sh
nenia pull
nenia list
nenia restore <snapshot> --output session.jsonl
nenia cat <snapshot> --decoded
```

[Hooks](docs/hooks.md) · [S3](docs/remote.md) ·
[Real-trace measurements](docs/benchmarks.md) · [Archive format](docs/FORMAT.md)

MIT licensed.
