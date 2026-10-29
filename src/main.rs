use anyhow::{ensure, Context, Result};
use clap::{Args, Parser, Subcommand};
use nenia::{
    adapters::{self, Source},
    hooks,
    remote::{self, SyncOptions},
    server,
    vault::Vault,
};
use serde::Serialize;
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "Small, lossless backups for agent traces",
    long_about = "Nenia backs up agent sessions to a self-hosted server or object store. It discovers Codex, Claude Code and Pi automatically, keeps an offline spool, and preserves exact source bytes."
)]
struct Cli {
    /// Local spool (server data directory for `serve`).
    #[arg(long, global = true, env = "NENIA_VAULT")]
    vault: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Args)]
struct Capture {
    /// Files/directories to include; omitted discovers agent session stores.
    paths: Vec<PathBuf>,
    #[arg(long, value_enum, default_value = "auto")]
    source: Source,
    /// Build optional local full-text index after capture (8 MiB text/session).
    #[arg(long)]
    index: bool,
    /// Re-read unchanged files too, ignoring the size/mtime cache.
    #[arg(long)]
    full_scan: bool,
}

#[derive(Args, Clone)]
struct SyncArgs {
    /// Server URL, s3://bucket/prefix, or file:///backup; defaults to saved connection.
    #[arg(long, env = "NENIA_REMOTE")]
    remote: Option<String>,
    /// Re-read existing remote objects to detect corruption (adds download traffic).
    #[arg(long)]
    verify_existing: bool,
    #[arg(long, default_value_t=4, value_parser=clap::value_parser!(u8).range(1..=32))]
    concurrency: u8,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum HookProvider {
    Claude,
}

#[derive(Subcommand)]
enum HooksAction {
    /// Install event-driven backups in the provider's user settings.
    Install { provider: HookProvider },
    /// Remove only Nenia-managed hooks, preserving other settings.
    Uninstall { provider: HookProvider },
    /// Capture queued events and retry pending uploads without polling agent folders.
    Drain,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage event-driven agent backups.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Receive a provider hook event on stdin; queues work and returns immediately.
    Hook { provider: HookProvider },
    /// Create an offline spool or server data directory.
    Init {
        #[arg(long, default_value_t = 9, allow_hyphen_values = true)]
        compression_level: i32,
    },
    /// Save a remote connection. Reads the bearer token from NENIA_TOKEN.
    Connect { remote: String },
    /// Discover traces, capture changes, and push to the configured remote.
    Backup {
        #[command(flatten)]
        capture: Capture,
        #[command(flatten)]
        sync: SyncArgs,
    },
    /// Capture traces into the offline spool without uploading.
    Ingest {
        #[command(flatten)]
        capture: Capture,
    },
    /// Show automatically discovered trace files without reading their contents.
    Discover,
    /// Continuously capture and upload; transient failures retry next interval.
    Watch {
        #[command(flatten)]
        capture: Capture,
        #[command(flatten)]
        sync: SyncArgs,
        #[arg(long, default_value_t=60, value_parser=clap::value_parser!(u64).range(1..))]
        interval: u64,
        /// Run a single backup cycle (useful with a system scheduler).
        #[arg(long)]
        once: bool,
    },
    /// Run an agent normally, then back up its discovered traces, even on failure.
    Run {
        #[command(flatten)]
        sync: SyncArgs,
        #[arg(long = "path")]
        paths: Vec<PathBuf>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Serve a single-tenant backup endpoint; requires NENIA_TOKEN.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7433")]
        listen: String,
        /// Keep an additive off-site copy in S3 or a file:// destination.
        #[arg(long, env = "NENIA_MIRROR")]
        mirror: Option<String>,
        #[arg(long, default_value_t=300, value_parser=clap::value_parser!(u64).range(10..))]
        mirror_interval: u64,
        /// Maintain an optional full-text index on the server.
        #[arg(long)]
        index: bool,
    },
    /// Upload pending immutable objects without capturing new traces.
    Push {
        #[command(flatten)]
        sync: SyncArgs,
    },
    /// Download remote objects and reconcile the local catalog.
    Pull {
        #[command(flatten)]
        sync: SyncArgs,
    },
    /// List latest snapshots, or every preserved version.
    List {
        #[arg(long, value_enum)]
        source: Option<Source>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        all_versions: bool,
    },
    /// Search metadata and any optional full-text index (AND of words).
    Search {
        query: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        all_versions: bool,
    },
    /// Show a snapshot's self-contained manifest.
    Show { snapshot: String },
    /// Restore exact original bytes. A destination is never overwritten.
    Restore {
        snapshot: String,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Write exact original bytes to stdout.
    Cat {
        snapshot: String,
        /// Read JSONL directly, even when the original was a native Zstd file.
        #[arg(long)]
        decoded: bool,
    },
    /// Build the optional contentless full-text index.
    Index {
        #[arg(long, default_value_t = 8)]
        max_text_mib: usize,
    },
    /// Check every object's hash and every reconstructed snapshot.
    Verify,
    /// Repack encoded image bodies into shared packs in a new vault.
    Compact {
        #[arg(long)]
        output: PathBuf,
    },
    /// Rebuild the disposable SQLite catalog from verified archive objects.
    Rebuild,
    /// Show payload, metadata, catalog, and total storage sizes.
    Stats,
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("could not locate home; set HOME or USERPROFILE")
}

fn default_vault() -> Result<PathBuf> {
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(path).join("nenia"));
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(home()?
            .join("Library")
            .join("Application Support")
            .join("nenia"));
    }
    #[cfg(not(target_os = "macos"))]
    Ok(std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or(home()?.join(".local").join("share"))
        .join("nenia"))
}

fn output(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn candidates(capture: &Capture) -> Result<Vec<adapters::Candidate>> {
    if capture.paths.is_empty() {
        Ok(adapters::discover(&home()?)?)
    } else {
        Ok(adapters::discover_paths(&capture.paths)?)
    }
}

fn capture(vault: &mut Vault, args: &Capture, json: bool) -> Result<nenia::vault::IngestReport> {
    let files = candidates(args)?;
    let report = if args.full_scan {
        vault.ingest(&files, args.source)?
    } else {
        vault.ingest_changed(&files, args.source)?
    };
    if json {
        output(&report)?;
    } else {
        eprintln!(
            "Captured {} snapshots; {} unchanged; {} new compressed bytes",
            report.archived, report.unchanged, report.new_pack_bytes
        );
    }
    if args.index {
        let count = vault.index(nenia::vault::DEFAULT_INDEX_BYTES)?;
        eprintln!("Indexed {count} snapshots");
    }
    Ok(report)
}

fn connection(vault: &Vault, args: &SyncArgs) -> Result<(String, SyncOptions)> {
    let remote = args.remote.clone().or_else(||vault.config.remote.clone()).context("no remote configured; run `nenia connect https://your-server` or pass --remote; captured traces remain in the offline spool")?;
    let token = std::env::var("NENIA_TOKEN")
        .ok()
        .or_else(|| vault.config.token.clone());
    Ok((
        remote,
        SyncOptions {
            concurrency: args.concurrency as usize,
            verify_existing: args.verify_existing,
            token,
        },
    ))
}

async fn push(vault: &Vault, args: &SyncArgs, json: bool) -> Result<remote::SyncReport> {
    vault.check_catalog_objects()?;
    let (destination, options) = connection(vault, args)?;
    let report = remote::push(&vault.root, &destination, &options)
        .await
        .context(
            "upload failed; captured traces are safe in the offline spool; run backup/push again",
        )?;
    if json {
        output(&report)?;
    } else {
        eprintln!(
            "Uploaded {} objects ({} bytes); {} already present ({} hash-verified)",
            report.transferred_objects,
            report.transferred_bytes,
            report.existing_objects,
            report.verified_existing_objects
        );
    }
    Ok(report)
}

async fn backup(root: &Path, args: &Capture, sync: &SyncArgs, json: bool) -> Result<()> {
    let mut vault = Vault::init(root, 9)?;
    let captured = capture(&mut vault, args, false)?;
    let uploaded = push(&vault, sync, false).await?;
    if json {
        output(&serde_json::json!({"capture":captured,"upload":uploaded}))?;
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("nenia: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = match cli.vault {
        Some(root) => root,
        None => default_vault()?,
    };
    match cli.command {
        Commands::Hooks { action } => match action {
            HooksAction::Install {
                provider: HookProvider::Claude,
            } => {
                drop(Vault::init(&root, 9)?);
                let report = hooks::install_claude(
                    &hooks::claude_settings(&home()?),
                    &std::env::current_exe()?,
                    &root,
                )?;
                if cli.json {
                    output(&report)?;
                } else {
                    eprintln!(
                        "{} Claude Stop and SessionEnd hooks in {}",
                        if report.changed {
                            "Installed"
                        } else {
                            "Already installed:"
                        },
                        report.settings.display()
                    );
                    if let Some(backup) = report.backup {
                        eprintln!("Original settings saved to {}", backup.display());
                    }
                }
            }
            HooksAction::Uninstall {
                provider: HookProvider::Claude,
            } => {
                let report = hooks::uninstall_claude(&hooks::claude_settings(&home()?))?;
                if cli.json {
                    output(&report)?;
                } else {
                    eprintln!(
                        "{}",
                        if report.changed {
                            "Removed Nenia's Claude hooks"
                        } else {
                            "No Nenia hooks installed"
                        }
                    );
                }
            }
            HooksAction::Drain => {
                output(&hooks::drain(&root, Duration::from_secs(600)).await?)?;
            }
        },
        Commands::Hook {
            provider: HookProvider::Claude,
        } => {
            let result = (|| -> Result<()> {
                hooks::queue_claude(&root, &home()?, io::stdin().lock())?;
                hooks::wake_worker(&root, &std::env::current_exe()?)?;
                Ok(())
            })();
            // Hook errors must never become a Stop decision or block Claude's shutdown.
            if let Err(error) = result {
                eprintln!("nenia hook: {error:#}");
            }
        }
        Commands::Init { compression_level } => {
            let vault = Vault::init(&root, compression_level)?;
            if cli.json {
                output(
                    &serde_json::json!({"vault":root,"device_id":vault.config.device_id,"compression_level":vault.config.compression_level}),
                )?;
            } else {
                eprintln!(
                    "Initialized {} (device {})",
                    root.display(),
                    vault.config.device_id
                );
            }
        }
        Commands::Connect { remote } => {
            let url =
                url::Url::parse(&remote).context("remote must be an HTTP(S), S3, or file URL")?;
            ensure!(
                matches!(url.scheme(), "http" | "https" | "s3" | "file"),
                "unsupported remote scheme"
            );
            ensure!(
                url.username().is_empty() && url.password().is_none(),
                "use NENIA_TOKEN instead of credentials in the URL"
            );
            let mut vault = Vault::init(&root, 9)?;
            let token = std::env::var("NENIA_TOKEN").ok();
            if matches!(url.scheme(), "http" | "https") {
                ensure!(
                    token.as_ref().is_some_and(|t| t.len() >= 24),
                    "set NENIA_TOKEN to your server's token (at least 24 characters)"
                );
            }
            vault.connect(remote.clone(), token)?;
            eprintln!("Saved connection to {remote}. Run `nenia backup` to capture and upload.");
        }
        Commands::Discover => output(&adapters::discover(&home()?)?)?,
        Commands::Ingest { capture: args } => {
            capture(&mut Vault::init(&root, 9)?, &args, cli.json)?;
        }
        Commands::Backup {
            capture: args,
            sync,
        } => backup(&root, &args, &sync, cli.json).await?,
        Commands::Watch {
            capture: args,
            sync,
            interval,
            once,
        } => loop {
            let result = backup(&root, &args, &sync, cli.json).await;
            if once {
                return result;
            }
            if let Err(e) = result {
                eprintln!("Backup will retry in {interval}s: {e:#}");
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(interval)) => {},
                _ = tokio::signal::ctrl_c() => break,
            }
        },
        Commands::Run {
            sync,
            paths,
            command,
        } => {
            let mut child = tokio::process::Command::new(&command[0])
                .args(&command[1..])
                .spawn()
                .with_context(|| format!("start {}", command[0]))?;
            // The child inherits the terminal/process group. Let it handle Ctrl-C,
            // while the wrapper stays alive to archive its final trace records.
            let status = loop {
                tokio::select! {
                    result = child.wait() => break result?,
                    _ = tokio::signal::ctrl_c() => {},
                }
            };
            let result = backup(
                &root,
                &Capture {
                    paths,
                    source: Source::Auto,
                    index: false,
                    full_scan: false,
                },
                &sync,
                cli.json,
            )
            .await;
            if let Err(e) = result {
                eprintln!("Post-session backup pending: {e:#}");
            }
            if !status.success() {
                #[cfg(unix)]
                let code = {
                    use std::os::unix::process::ExitStatusExt;
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
                };
                #[cfg(not(unix))]
                let code = status.code().unwrap_or(1);
                std::process::exit(code);
            }
        }
        Commands::Serve {
            listen,
            mirror,
            mirror_interval,
            index,
        } => {
            let token = std::env::var("NENIA_TOKEN")
                .context("set NENIA_TOKEN to a random token of at least 24 characters")?;
            server::serve_with_mirror(
                Vault::init(&root, 9)?,
                &listen,
                token,
                mirror,
                Duration::from_secs(mirror_interval),
                index,
            )
            .await?;
        }
        Commands::Push { sync } => {
            push(&Vault::open(&root)?, &sync, cli.json).await?;
        }
        Commands::Pull { sync } => {
            let mut vault = Vault::init(&root, 9)?;
            let (source, mut options) = connection(&vault, &sync)?;
            // Local existing objects are always checked during restore.
            options.verify_existing = true;
            let report = remote::pull(&root, &source, &options).await?;
            let recovered = vault.reconcile()?;
            if cli.json {
                output(&report)?;
            } else {
                eprintln!(
                    "Downloaded {} objects; recovered {recovered} snapshots",
                    report.transferred_objects
                );
            }
        }
        Commands::List {
            source,
            limit,
            all_versions,
        } => {
            let rows = Vault::open(&root)?.list(source, limit, all_versions)?;
            print_snapshots(&rows, cli.json)?;
        }
        Commands::Search {
            query,
            limit,
            all_versions,
        } => {
            let rows = Vault::open(&root)?.search(&query, limit, all_versions)?;
            print_snapshots(&rows, cli.json)?;
        }
        Commands::Show { snapshot } => {
            let v = Vault::open(&root)?;
            output(&v.manifest(&v.resolve(&snapshot)?)?)?;
        }
        Commands::Restore { snapshot, output } => {
            let v = Vault::open(&root)?;
            v.export_file(&v.resolve(&snapshot)?, &output)?;
            eprintln!("Restored {}", output.display());
        }
        Commands::Cat { snapshot, decoded } => {
            let v = Vault::open(&root)?;
            let id = v.resolve(&snapshot)?;
            if decoded {
                v.export_decoded(&id, &mut io::stdout().lock())?;
            } else {
                v.export(&id, &mut io::stdout().lock())?;
            }
        }
        Commands::Index { max_text_mib } => {
            let mut v = Vault::open(&root)?;
            let bytes = max_text_mib
                .checked_mul(1024 * 1024)
                .context("index size overflow")?;
            output(&serde_json::json!({"indexed":v.index(bytes)?}))?;
        }
        Commands::Verify => output(&Vault::open(&root)?.verify()?)?,
        Commands::Compact {
            output: destination,
        } => output(&nenia::compact::run(&Vault::open(&root)?, &destination)?)?,
        Commands::Rebuild => {
            output(&serde_json::json!({"recovered":Vault::rebuild_catalog(&root)?}))?
        }
        Commands::Stats => output(&Vault::open(&root)?.stats()?)?,
    }
    Ok(())
}

fn print_snapshots(rows: &[nenia::vault::Snapshot], json: bool) -> Result<()> {
    if json {
        return output(&rows);
    }
    for s in rows {
        println!(
            "{}  {:7}  {:>10}  {}  {}",
            &s.id[..12],
            s.source,
            s.bytes,
            s.captured_at,
            s.title.as_deref().unwrap_or(&s.origin)
        );
    }
    Ok(())
}
