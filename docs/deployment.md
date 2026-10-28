# Running Nenia continuously

Run the client under the same OS account as your agents. After `nenia connect`,
`nenia watch --interval 60` discovers and captures local traces every minute and
retries uploads while offline. Each cycle releases the vault lock before it
sleeps. `nenia watch --once` performs one cycle and returns a nonzero status when
the remote upload has not completed, which is useful with an existing scheduler.

By default rescans use size and modification time to avoid rereading unchanged
traces. Use `nenia backup --full-scan` periodically if another tool might rewrite
files while preserving their timestamps. Neither command deletes source files
or removes local archive objects after upload.

## Self-hosted server

The repository builds its own container from the official
[Rust](https://hub.docker.com/_/rust) and
[Debian](https://hub.docker.com/_/debian) images. There is no published Nenia image
or registry dependency for the application itself.

From the repository directory, set a random token and start the service:

```sh
export NENIA_TOKEN="$(openssl rand -hex 32)"
docker compose up --detach --build
```

Keep that token in your server's environment/secret configuration for later
restarts and give the same token to clients when running `nenia connect`.
Compose requires `NENIA_TOKEN`; the server also rejects tokens shorter than
24 characters. The container runs as UID/GID 10001 and stores its complete
vault in the persistent `nenia-data` Docker volume. The host port defaults to
`127.0.0.1:7433`; set `NENIA_PORT` to choose another local port. A reverse proxy
can expose HTTPS, or an SSH tunnel can provide access to the loopback endpoint.

```sh
curl --fail http://127.0.0.1:7433/healthz
docker compose logs --follow
```

The client accepts HTTP and HTTPS URLs; use HTTPS for ordinary remote
connections. The server itself speaks HTTP behind your proxy. A proxy must
permit request bodies
up to 64 MiB for manifests and allow the configured upload timeout. The server
is single tenant: clients sharing its token share the same archive namespace.

`docker compose down` stops the service while retaining the named volume. The
volume contains the backup; keep it on storage whose durability you accept.
Stop the server before filesystem-level maintenance or a cold volume copy.
The CLI and server use the same exclusive vault lock. Save `packs/` and
`manifests/` together; copying SQLite alone does not save a backup. Preserve
`config.json` locally to retain the server settings. The search catalog can be
rebuilt from the archive objects.

## Example Linux user service

If installed with `cargo install --path .`, save this as
`~/.config/systemd/user/nenia.service`:

```ini
[Unit]
Description=Nenia agent trace backups

[Service]
ExecStart=%h/.cargo/bin/nenia watch --interval 60
Restart=on-failure
RestartSec=15

[Install]
WantedBy=default.target
```

Run `systemctl --user daemon-reload`, then
`systemctl --user enable --now nenia.service`. Inspect failures with
`journalctl --user -u nenia.service`. The user service runs while the user
manager is active. If you already override provider homes in your shell, add
those same environment variables to the service. The default provider locations
need no additional configuration.

## macOS and Windows scheduling

On macOS, a user LaunchAgent can run the absolute path to `nenia` with the
arguments `watch --interval 60`, with `RunAtLoad` and `KeepAlive` enabled. Run it
in your login session so it can discover your own traces. LaunchAgents do not
expand `~` in `ProgramArguments`; use the complete binary path.

On Windows, create a Task Scheduler task under your own account whose action
is the full path to `nenia.exe`, with arguments `watch --once`. A repeating
five-minute trigger is sufficient; choose “Do not start a new instance” when
the task is already running. Use the same user profile and spool as the initial
`nenia connect`. The server credentials are already saved in that spool, so the
scheduled action does not need a token argument. Running as `SYSTEM` discovers
that account's home instead of your agent sessions.

## Validation status

Linux tests run real CLI processes against a local HTTP server, including
offline retries, fresh-device recovery, corrupt-catalog recovery, source
reversions, and wrapper exit status. Adapter code and tests also cross-check
for Windows MSVC and macOS ARM64. The GitHub Actions workflow adds native tests
on all three OSes, packaged build artifacts, and a container startup probe; it
does not publish releases or images. Docker and those hosted CI jobs were not
available to execute in the development environment, so they are provided as
unrun deployment checks rather than claimed validation.
