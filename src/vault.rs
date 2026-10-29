use crate::adapters::{self, Candidate, Metadata, Source};
use crate::pack::{self, ChunkRef, Chunker, PackBuilder};
use crate::{
    native,
    structure::{self, Recipe},
};
use anyhow::{bail, ensure, Context, Result};
use chrono::Utc;
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

pub const MAX_MANIFEST_SIZE: usize = 64 * 1024 * 1024;
pub const DEFAULT_INDEX_BYTES: usize = 8 * 1024 * 1024;
const MAX_SNAPSHOT_CHUNKS: usize = MAX_MANIFEST_SIZE / 256;
type PendingSnapshots = Vec<(Manifest, Vec<String>)>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub compression_level: i32,
    pub device_id: String,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub device_id: String,
    pub origin: String,
    pub captured_at: String,
    pub content_hash: String,
    pub bytes: u64,
    pub metadata: Metadata,
    pub chunks: Vec<ChunkRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<Recipe>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub revision: u64,
    pub device_id: String,
    pub origin: String,
    pub captured_at: String,
    pub content_hash: String,
    pub bytes: u64,
    pub source: String,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub events: u64,
    pub indexed: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct IngestReport {
    pub archived: usize,
    pub unchanged: usize,
    pub input_bytes: u64,
    pub new_chunk_bytes: u64,
    pub new_pack_bytes: u64,
    pub snapshots: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub snapshots: u64,
    pub origins: u64,
    pub indexed_snapshots: u64,
    pub logical_bytes: u64,
    pub latest_bytes: u64,
    pub unique_chunk_bytes: u64,
    pub packs: u64,
    pub pack_bytes: u64,
    pub manifest_bytes: u64,
    pub catalog_bytes: u64,
    pub other_bytes: u64,
    pub archive_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct VerifyReport {
    pub snapshots: u64,
    pub packs: u64,
    pub checked_bytes: u64,
    pub orphan_packs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Inventory {
    pub packs: Vec<String>,
    pub manifests: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InventoryPage {
    pub packs: Vec<String>,
    pub manifests: Vec<String>,
    pub next_after: Option<i64>,
    pub watermark: i64,
    pub generation: String,
}

pub struct Vault {
    pub root: PathBuf,
    pub config: Config,
    db: Connection,
    _lock: File,
}

pub fn decode_manifest(encoded: &[u8], expected_id: &str) -> Result<Manifest> {
    pack::validate_hash_id(expected_id)?;
    ensure!(
        encoded.len() <= MAX_MANIFEST_SIZE,
        "manifest exceeds size limit"
    );
    ensure!(
        blake3::hash(encoded).to_hex().as_str() == expected_id,
        "manifest hash mismatch: {expected_id}"
    );
    let mut decoder = zstd::stream::read::Decoder::new(encoded)?;
    decoder.window_log_max(26)?;
    let mut raw = Vec::new();
    decoder
        .take(MAX_MANIFEST_SIZE as u64 + 1)
        .read_to_end(&mut raw)?;
    ensure!(
        raw.len() <= MAX_MANIFEST_SIZE,
        "decoded manifest exceeds size limit"
    );
    let m: Manifest = serde_json::from_slice(&raw).context("invalid manifest JSON")?;
    validate_manifest(&m)?;
    Ok(m)
}

fn validate_manifest(m: &Manifest) -> Result<()> {
    ensure!(
        matches!(m.version, 1 | 2),
        "unsupported manifest version {}",
        m.version
    );
    ensure!(
        (m.version == 2) == m.recipe.is_some(),
        "manifest version/recipe mismatch"
    );
    pack::validate_hash_id(&m.content_hash)?;
    ensure!(
        m.chunks.len() <= MAX_SNAPSHOT_CHUNKS,
        "snapshot exceeds chunk-reference limit"
    );
    ensure!(
        m.revision < i64::MAX as u64,
        "snapshot revision exceeds catalog limit"
    );
    let mut total = 0u64;
    for c in &m.chunks {
        if m.version == 1 || !c.hash.is_empty() {
            pack::validate_hash_id(&c.hash)?;
        }
        pack::validate_hash_id(&c.pack)?;
        ensure!(
            c.len > 0 && c.len as usize <= pack::MAX_CHUNK_SIZE,
            "invalid chunk length"
        );
        ensure!(
            c.offset
                .checked_add(c.len as u64)
                .is_some_and(|end| end <= pack::MAX_PACK_RAW_SIZE as u64),
            "chunk offset out of bounds"
        );
        total = total
            .checked_add(c.len as u64)
            .context("snapshot size overflow")?;
    }
    if let Some(recipe) = &m.recipe {
        pack::validate_hash_id(&recipe.decoded_hash)?;
        ensure!(
            recipe.operations.len() <= structure::MAX_OPERATIONS,
            "too many structural operations"
        );
        ensure!(
            recipe.decoded_bytes <= MAX_SNAPSHOT_CHUNKS as u64 * pack::MAX_CHUNK_SIZE as u64,
            "decoded snapshot exceeds size limit"
        );
        ensure!(
            recipe
                .container
                .as_deref()
                .is_none_or(|c| c == native::PROFILE),
            "unknown native reconstruction profile"
        );
        if recipe.container.is_some() {
            ensure!(
                recipe.decoded_bytes <= native::MAX_DECODED,
                "native normalization exceeds size limit"
            );
        } else {
            ensure!(
                recipe.decoded_bytes == m.bytes && recipe.decoded_hash == m.content_hash,
                "raw recipe identity mismatch"
            );
        }
        let mut used = vec![false; m.chunks.len()];
        for [index, offset, len, flags] in &recipe.operations {
            let chunk = m
                .chunks
                .get(*index as usize)
                .context("recipe chunk index out of bounds")?;
            ensure!(
                *len > 0 && offset.checked_add(*len).is_some_and(|n| n <= chunk.len),
                "recipe slice out of bounds"
            );
            structure::validate_flags(*flags)?;
            used[*index as usize] = true;
        }
        ensure!(used.iter().all(|u| *u), "unreferenced recipe chunk");
    } else {
        ensure!(
            total == m.bytes,
            "manifest chunk lengths do not match snapshot size"
        );
    }
    ensure!(
        m.bytes <= i64::MAX as u64,
        "snapshot exceeds catalog size limit"
    );
    Ok(())
}

fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= max as u64,
        "object too large: {}",
        path.display()
    );
    let mut data = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut data)?;
    ensure!(data.len() <= max, "object grew beyond size limit");
    Ok(data)
}

fn source_name(source: Source) -> String {
    serde_json::to_value(source)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

impl Vault {
    pub fn connect(&mut self, remote: String, token: Option<String>) -> Result<()> {
        self.config.remote = Some(remote);
        self.config.token = token;
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        temp.write_all(&serde_json::to_vec_pretty(&self.config)?)?;
        temp.as_file().sync_all()?;
        temp.persist(self.root.join("config.json"))?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    pub fn inventory(&self) -> Result<Inventory> {
        Ok(Inventory {
            packs: self.object_ids("packs", ".nz")?,
            manifests: self.object_ids("manifests", ".json.zst")?,
        })
    }

    pub fn inventory_page(
        &self,
        after: i64,
        watermark: Option<i64>,
        limit: usize,
    ) -> Result<InventoryPage> {
        ensure!(
            after >= 0 && (1..=10000).contains(&limit),
            "invalid inventory page parameters"
        );
        let current: i64 =
            self.db
                .query_row("SELECT COALESCE(MAX(seq),0) FROM objects", [], |r| r.get(0))?;
        let watermark = watermark.unwrap_or(current);
        ensure!(
            watermark >= after && watermark <= current,
            "inventory watermark no longer available; restart inventory"
        );
        let generation = self.db.query_row(
            "SELECT value FROM catalog_meta WHERE key='generation'",
            [],
            |r| r.get(0),
        )?;
        let mut statement = self.db.prepare(
            "SELECT seq,kind,id FROM objects WHERE seq>?1 AND seq<=?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = statement
            .query_map(params![after, watermark, limit + 1], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = rows.len() > limit;
        let mut page = InventoryPage {
            packs: Vec::new(),
            manifests: Vec::new(),
            next_after: None,
            watermark,
            generation,
        };
        for (seq, kind, id) in rows.into_iter().take(limit) {
            // A lost file must be absent from inventory so a normal retry repairs it.
            let exists = match kind.as_str() {
                "packs" => self.pack_path(&id)?.is_file(),
                "manifests" => self.manifest_path(&id)?.is_file(),
                _ => bail!("invalid catalog object kind"),
            };
            if more {
                page.next_after = Some(seq);
            }
            if !exists {
                continue;
            }
            match kind.as_str() {
                "packs" => page.packs.push(id),
                "manifests" => page.manifests.push(id),
                _ => bail!("invalid catalog object kind"),
            }
        }
        Ok(page)
    }

    fn register_object(&self, kind: &str, id: &str) -> Result<()> {
        self.db
            .prepare_cached("INSERT INTO objects(kind,id) SELECT ?1,?2 WHERE NOT EXISTS(SELECT 1 FROM objects WHERE kind=?1 AND id=?2)")?
            .execute(params![kind, id])?;
        Ok(())
    }

    pub fn object(&self, kind: &str, id: &str) -> Result<Vec<u8>> {
        match kind {
            "packs" => read_bounded(&self.pack_path(id)?, pack::MAX_PACK_RAW_SIZE + 65536),
            "manifests" => read_bounded(&self.manifest_path(id)?, MAX_MANIFEST_SIZE),
            _ => bail!("unknown object kind"),
        }
    }

    /// Accept an immutable object. Manifests become visible only after every chunk verifies.
    pub fn receive_object(&mut self, kind: &str, id: &str, bytes: &[u8]) -> Result<bool> {
        match kind {
            "packs" => {
                pack::decode_pack(bytes, id)?;
                let created = pack::atomic_write_immutable(&self.pack_path(id)?, bytes)?;
                self.register_object("packs", id)?;
                Ok(created)
            }
            "manifests" => {
                let m = decode_manifest(bytes, id)?;
                let mut cache = PackCache::default();
                self.export_manifest(&m, &mut std::io::sink(), &mut cache)?;
                let created = pack::atomic_write_immutable(&self.manifest_path(id)?, bytes)?;
                self.db.execute_batch("BEGIN IMMEDIATE")?;
                match self.insert_manifest(id, &m) {
                    Ok(()) => self.db.execute_batch("COMMIT")?,
                    Err(e) => {
                        let _ = self.db.execute_batch("ROLLBACK");
                        return Err(e);
                    }
                }
                Ok(created)
            }
            _ => bail!("unknown object kind"),
        }
    }

    /// Reconcile only objects missing from the cache, without rereading years of known data.
    pub fn reconcile(&mut self) -> Result<usize> {
        let mut count = 0;
        for id in self.object_ids("packs", ".nz")? {
            self.register_object("packs", &id)?;
        }
        for id in self.object_ids("manifests", ".json.zst")? {
            if self
                .db
                .query_row("SELECT 1 FROM snapshots WHERE id=?1", [&id], |_| Ok(()))
                .optional()?
                .is_none()
            {
                let bytes = self.object("manifests", &id)?;
                self.receive_object("manifests", &id, &bytes)?;
                count += 1;
            } else {
                self.register_object("manifests", &id)?;
            }
        }
        self.checkpoint()?;
        Ok(count)
    }

    pub fn init(root: &Path, compression_level: i32) -> Result<Self> {
        ensure!(
            (-7..=22).contains(&compression_level),
            "compression level must be -7..22"
        );
        #[cfg(unix)]
        let missing: Vec<_> = root
            .ancestors()
            .filter(|p| !p.as_os_str().is_empty())
            .take_while(|p| !p.exists())
            .map(Path::to_owned)
            .collect();
        fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
            for path in missing {
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                File::open(parent)?.sync_all()?;
            }
        }
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("generate device ID: {e}"))?;
        let config = Config {
            version: 1,
            compression_level,
            device_id: blake3::hash(&random).to_hex()[..16].to_string(),
            remote: None,
            token: None,
        };
        let path = root.join("config.json");
        if !path.exists() {
            pack::atomic_write_immutable(&path, &serde_json::to_vec_pretty(&config)?)?;
        }
        Self::open(root)
    }

    pub fn open(root: &Path) -> Result<Self> {
        let lock = Self::lock(root)?;
        Self::open_locked(root, lock)
    }

    fn lock(root: &Path) -> Result<File> {
        ensure!(
            root.join("config.json").is_file(),
            "vault not initialized at {}; run `nenia init`",
            root.display()
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("vault.lock"))?;
        lock.try_lock_exclusive()
            .context("vault is busy; another Nenia command holds its lock")?;
        Ok(lock)
    }

    fn open_locked(root: &Path, lock: File) -> Result<Self> {
        let config: Config =
            serde_json::from_slice(&read_bounded(&root.join("config.json"), 65536)?)?;
        ensure!(
            config.version == 1,
            "unsupported vault version {}",
            config.version
        );
        ensure!(
            (-7..=22).contains(&config.compression_level),
            "invalid configured compression level"
        );
        fs::create_dir_all(root.join("packs"))?;
        fs::create_dir_all(root.join("manifests"))?;
        #[cfg(unix)]
        File::open(root)?.sync_all()?;
        let db = Connection::open(root.join("catalog.sqlite3"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS chunks(hash TEXT PRIMARY KEY, pack TEXT NOT NULL, offset INTEGER NOT NULL, len INTEGER NOT NULL) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS snapshots(id TEXT PRIMARY KEY, device_id TEXT NOT NULL, origin TEXT NOT NULL, revision INTEGER NOT NULL, captured_at TEXT NOT NULL, content_hash TEXT NOT NULL, bytes INTEGER NOT NULL, source TEXT NOT NULL, session_id TEXT, title TEXT, events INTEGER NOT NULL, indexed INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS snapshots_origin ON snapshots(device_id, origin, revision, captured_at);
            CREATE INDEX IF NOT EXISTS snapshots_content ON snapshots(device_id, origin, content_hash);
            CREATE TABLE IF NOT EXISTS objects(seq INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, id TEXT NOT NULL, UNIQUE(kind,id));
            CREATE TABLE IF NOT EXISTS catalog_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS observations(device_id TEXT NOT NULL, origin TEXT NOT NULL, source TEXT NOT NULL, bytes INTEGER NOT NULL, modified TEXT NOT NULL, PRIMARY KEY(device_id,origin,source)) WITHOUT ROWID;
            CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(text, content='', detail=none, columnsize=0);")?;
        if db
            .query_row(
                "SELECT 1 FROM catalog_meta WHERE key='generation'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_none()
        {
            let mut random = [0u8; 32];
            getrandom::fill(&mut random)
                .map_err(|e| anyhow::anyhow!("generate catalog identity: {e}"))?;
            db.execute(
                "INSERT INTO catalog_meta(key,value) VALUES('generation',?1)",
                [blake3::hash(&random).to_hex().to_string()],
            )?;
        }
        Ok(Self {
            root: root.to_owned(),
            config,
            db,
            _lock: lock,
        })
    }

    /// Recover even a corrupt SQLite database, preserving the old catalog for inspection.
    pub fn rebuild_catalog(root: &Path) -> Result<usize> {
        let lock = Self::lock(root)?;
        let config: Config =
            serde_json::from_slice(&read_bounded(&root.join("config.json"), 65536)?)?;
        ensure!(config.version == 1, "unsupported vault version");
        let checker = Self {
            root: root.to_owned(),
            config,
            db: Connection::open_in_memory()?,
            _lock: lock,
        };
        checker
            .verify()
            .context("archive verification failed; existing catalog was left untouched")?;
        let backup = tempfile::Builder::new()
            .prefix("catalog-before-rebuild-")
            .tempdir_in(root)?
            .keep();
        for name in [
            "catalog.sqlite3",
            "catalog.sqlite3-wal",
            "catalog.sqlite3-shm",
        ] {
            let path = root.join(name);
            if path.exists() {
                fs::rename(&path, backup.join(name))?;
            }
        }
        #[cfg(unix)]
        File::open(root)?.sync_all()?;
        let mut fresh = Self::open_locked(root, checker._lock)?;
        fresh.rebuild_verified()
    }

    pub fn ingest(&mut self, files: &[Candidate], source_override: Source) -> Result<IngestReport> {
        self.check_catalog_objects()?;
        let mut report = IngestReport::default();
        // Bound both metadata and pending chunk memory, while sharing compression across files.
        for batch in files.chunks(256) {
            self.db.execute_batch("BEGIN IMMEDIATE")?;
            match self.ingest_batch(batch, source_override, &mut report) {
                Ok(()) => self.db.execute_batch("COMMIT")?,
                Err(e) => {
                    let _ = self.db.execute_batch("ROLLBACK");
                    return Err(e.context("ingest interrupted; durable objects remain recoverable with `nenia rebuild`"));
                }
            }
        }
        self.checkpoint()?;
        Ok(report)
    }

    /// Cheap existence checks prevent a damaged local cache from acknowledging backups.
    pub fn check_catalog_objects(&self) -> Result<()> {
        for (sql, directory, suffix) in [
            ("SELECT id FROM snapshots", "manifests", ".json.zst"),
            ("SELECT DISTINCT pack FROM chunks", "packs", ".nz"),
        ] {
            let mut statement = self.db.prepare(sql)?;
            let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                let id = row?;
                pack::validate_hash_id(&id)?;
                ensure!(self.root.join(directory).join(format!("{id}{suffix}")).is_file(),"catalog references missing {directory} object {id}; restore missing objects with `nenia pull`");
            }
        }
        Ok(())
    }

    /// Fast periodic capture. Full ingest remains available for files with preserved timestamps.
    pub fn ingest_changed(
        &mut self,
        files: &[Candidate],
        source_override: Source,
    ) -> Result<IngestReport> {
        let mut changed = Vec::new();
        let mut skipped = 0;
        for candidate in files {
            let path = fs::canonicalize(&candidate.path)?;
            let stat = fs::metadata(&path)?;
            let modified = modified_key(&stat)?;
            let origin = path
                .to_str()
                .context("source paths must be valid UTF-8; rename this file before capture")?;
            let known = self.db.query_row("SELECT 1 FROM observations WHERE device_id=?1 AND origin=?2 AND source=?3 AND bytes=?4 AND modified=?5",params![self.config.device_id,origin,source_name(source_override),stat.len(),modified],|_|Ok(())).optional()?.is_some();
            if known {
                skipped += 1;
            } else {
                changed.push(candidate.clone());
            }
        }
        let mut report = self.ingest(&changed, source_override)?;
        report.unchanged += skipped;
        Ok(report)
    }

    fn ingest_batch(
        &self,
        files: &[Candidate],
        source_override: Source,
        report: &mut IngestReport,
    ) -> Result<()> {
        let mut builder = PackBuilder::new(self.config.compression_level);
        let mut pending_chunks: HashMap<String, (u64, u32)> = HashMap::new();
        let mut checked_packs = std::collections::HashSet::new();
        let mut pending = Vec::new();
        let mut pending_references = 0;
        for candidate in files {
            let path = fs::canonicalize(&candidate.path)
                .with_context(|| format!("resolve {}", candidate.path.display()))?;
            ensure!(
                !path.starts_with(fs::canonicalize(&self.root)?),
                "refusing to ingest the vault itself"
            );
            let mut file = File::open(&path)?;
            let initial = file.metadata()?;
            ensure!(
                initial.is_file(),
                "source is not a regular file: {}",
                path.display()
            );
            let origin = path
                .to_str()
                .context("source paths must be valid UTF-8; rename this file before capture")?
                .to_owned();
            let latest: Option<(String,u64)> = self.db.query_row("SELECT content_hash,revision FROM snapshots WHERE device_id=?1 AND origin=?2 ORDER BY revision DESC,captured_at DESC,id DESC LIMIT 1", params![self.config.device_id, origin], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            // Check identity before writing transformed chunks. Otherwise an
            // unchanged legacy snapshot could leave a full orphaned v2 encoding.
            let mut observed = blake3::Hasher::new();
            let observed_size = std::io::copy(
                &mut Read::by_ref(&mut file).take(initial.len()),
                &mut observed,
            )?;
            ensure!(
                observed_size == initial.len(),
                "source was truncated during capture"
            );
            let observed_hash = observed.finalize().to_hex().to_string();
            file.rewind()?;
            if latest
                .as_ref()
                .is_some_and(|(hash, _)| hash == &observed_hash)
            {
                self.observe_stable(&path, &initial, source_override)?;
                report.input_bytes += initial.len();
                report.unchanged += 1;
                continue;
            }
            let source = if source_override == Source::Auto {
                if candidate.source == Source::Generic {
                    Source::Auto
                } else {
                    candidate.source
                }
            } else {
                source_override
            };
            let metadata = adapters::inspect_source(
                BufReader::new(file.try_clone()?.take(initial.len())),
                source,
                &path,
            )?;
            file.rewind()?;
            let compressed = adapters::is_zstd_source(&path);
            let prepared = if compressed {
                native::prepare(&mut file, initial.len(), &self.root)?
            } else {
                None
            };
            let structural = prepared.is_some() || (!compressed && initial.len() >= 4096);
            let mut hashes = Vec::new();
            let (recipe, size, content_hash) = if structural {
                let (reader, native_hash): (Box<dyn Read>, Option<String>) = match prepared {
                    Some(prepared) => (Box::new(prepared.file), Some(prepared.original_hash)),
                    None => (Box::new(file.take(initial.len())), None),
                };
                let mut local = HashMap::new();
                let mut recipe = structure::encode(reader, |chunk| {
                    let hash = self.stage_chunk(
                        chunk,
                        &mut builder,
                        &mut pending_chunks,
                        &mut checked_packs,
                        report,
                    )?;
                    if let Some(index) = local.get(&hash) {
                        return Ok(*index);
                    }
                    ensure!(
                        hashes.len() < MAX_SNAPSHOT_CHUNKS,
                        "source exceeds snapshot chunk-reference limit"
                    );
                    let index = hashes.len() as u32;
                    hashes.push(hash.clone());
                    local.insert(hash, index);
                    Ok(index)
                })?;
                let (size, content_hash) = if let Some(hash) = native_hash {
                    recipe.container = Some(native::PROFILE.into());
                    (initial.len(), hash)
                } else {
                    (recipe.decoded_bytes, recipe.decoded_hash.clone())
                };
                (Some(recipe), size, content_hash)
            } else {
                let reader = if compressed {
                    Chunker::new_fixed(file.take(initial.len()))
                } else {
                    Chunker::new(file.take(initial.len()))
                };
                let mut hash = blake3::Hasher::new();
                let mut size = 0;
                for chunk in reader {
                    let chunk = chunk?;
                    ensure!(
                        hashes.len() < MAX_SNAPSHOT_CHUNKS,
                        "source exceeds snapshot chunk-reference limit"
                    );
                    hash.update(&chunk);
                    size += chunk.len() as u64;
                    hashes.push(self.stage_chunk(
                        &chunk,
                        &mut builder,
                        &mut pending_chunks,
                        &mut checked_packs,
                        report,
                    )?);
                }
                (None, size, hash.finalize().to_hex().to_string())
            };
            ensure!(
                size == initial.len(),
                "source was truncated during capture: {}",
                path.display()
            );
            ensure!(
                content_hash == observed_hash,
                "source changed during capture; retry this file"
            );
            let revision = latest.map_or(1, |(_, revision)| revision + 1);
            report.input_bytes += size;
            self.observe_stable(&path, &initial, source_override)?;
            pending_references += hashes.len();
            pending.push((
                Manifest {
                    version: if recipe.is_some() { 2 } else { 1 },
                    recipe,
                    revision,
                    device_id: self.config.device_id.clone(),
                    origin,
                    captured_at: Utc::now().to_rfc3339(),
                    content_hash,
                    bytes: size,
                    metadata,
                    chunks: Vec::new(),
                },
                hashes,
            ));
            if pending_references >= 65536 {
                self.flush_pack(&mut builder, &mut pending_chunks, report)?;
                self.publish_snapshots(&mut pending, report)?;
                pending_references = 0;
            }
        }
        self.flush_pack(&mut builder, &mut pending_chunks, report)?;
        self.publish_snapshots(&mut pending, report)
    }

    fn observe_stable(&self, path: &Path, initial: &fs::Metadata, source: Source) -> Result<()> {
        if let Ok(final_stat) = fs::metadata(path) {
            if initial.len() == final_stat.len() && initial.modified()? == final_stat.modified()? {
                self.db.execute("INSERT OR REPLACE INTO observations(device_id,origin,source,bytes,modified) VALUES(?1,?2,?3,?4,?5)",
                    params![self.config.device_id, path.to_str(), source_name(source), initial.len(), modified_key(&final_stat)?])?;
            }
        }
        Ok(())
    }

    fn stage_chunk(
        &self,
        chunk: &[u8],
        builder: &mut PackBuilder,
        pending: &mut HashMap<String, (u64, u32)>,
        checked: &mut std::collections::HashSet<String>,
        report: &mut IngestReport,
    ) -> Result<String> {
        let hash = blake3::hash(chunk).to_hex().to_string();
        if pending.contains_key(&hash) {
            return Ok(hash);
        }
        let old_pack: Option<String> = self
            .db
            .prepare_cached("SELECT pack FROM chunks WHERE hash=?1")?
            .query_row([&hash], |r| r.get(0))
            .optional()?;
        if let Some(id) = old_pack {
            if checked.insert(id.clone()) {
                ensure!(self.pack_path(&id)?.is_file(), "catalog references missing pack {id}; restore missing objects with `nenia pull` before capturing more snapshots");
            }
        } else {
            if !builder.can_fit(chunk.len()) {
                self.flush_pack(builder, pending, report)?;
            }
            let offset = builder.add(chunk)?;
            pending.insert(hash.clone(), (offset, chunk.len() as u32));
            report.new_chunk_bytes += chunk.len() as u64;
        }
        Ok(hash)
    }

    fn publish_snapshots(
        &self,
        pending: &mut PendingSnapshots,
        report: &mut IngestReport,
    ) -> Result<()> {
        for (mut manifest, hashes) in pending.drain(..) {
            for hash in hashes {
                let chunk = self.db.query_row(
                    "SELECT pack,offset,len FROM chunks WHERE hash=?1",
                    [&hash],
                    |row| {
                        Ok(ChunkRef {
                            hash: hash.clone(),
                            pack: row.get(0)?,
                            offset: row.get(1)?,
                            len: row.get(2)?,
                        })
                    },
                )?;
                manifest.chunks.push(chunk);
            }
            let id = self.save_manifest(&manifest)?;
            self.insert_manifest(&id, &manifest)?;
            report.archived += 1;
            report.snapshots.push(id);
        }
        Ok(())
    }

    fn flush_pack(
        &self,
        builder: &mut PackBuilder,
        pending: &mut HashMap<String, (u64, u32)>,
        report: &mut IngestReport,
    ) -> Result<()> {
        if builder.is_empty() {
            return Ok(());
        }
        let pack =
            std::mem::replace(builder, PackBuilder::new(self.config.compression_level)).finish()?;
        if pack::atomic_write_immutable(&self.pack_path(&pack.id)?, &pack.bytes)? {
            report.new_pack_bytes += pack.bytes.len() as u64;
        }
        self.register_object("packs", &pack.id)?;
        for (hash, (offset, len)) in pending.drain() {
            self.db.execute(
                "INSERT OR IGNORE INTO chunks(hash,pack,offset,len) VALUES(?1,?2,?3,?4)",
                params![hash, pack.id, offset, len],
            )?;
        }
        Ok(())
    }

    fn pack_path(&self, id: &str) -> Result<PathBuf> {
        pack::validate_hash_id(id)?;
        Ok(self.root.join("packs").join(format!("{id}.nz")))
    }

    fn manifest_path(&self, id: &str) -> Result<PathBuf> {
        pack::validate_hash_id(id)?;
        Ok(self.root.join("manifests").join(format!("{id}.json.zst")))
    }

    fn save_manifest(&self, manifest: &Manifest) -> Result<String> {
        validate_manifest(manifest)?;
        let mut stored = manifest.clone();
        if stored.version == 2 {
            for chunk in &mut stored.chunks {
                chunk.hash.clear();
            }
        }
        let raw = serde_json::to_vec(&stored)?;
        ensure!(
            raw.len() <= MAX_MANIFEST_SIZE,
            "snapshot has too many chunks for manifest format"
        );
        let encoded = zstd::stream::encode_all(raw.as_slice(), 9)?;
        ensure!(
            encoded.len() <= MAX_MANIFEST_SIZE,
            "compressed manifest too large"
        );
        let id = blake3::hash(&encoded).to_hex().to_string();
        pack::atomic_write_immutable(&self.manifest_path(&id)?, &encoded)?;
        Ok(id)
    }

    fn insert_manifest(&self, id: &str, m: &Manifest) -> Result<()> {
        let mut packs = std::collections::HashSet::new();
        for c in &m.chunks {
            if packs.insert(&c.pack) {
                self.register_object("packs", &c.pack)?;
            }
        }
        self.register_object("manifests", id)?;
        self.db.execute("INSERT OR IGNORE INTO snapshots(id,device_id,origin,captured_at,content_hash,bytes,source,session_id,title,events,revision) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", params![id,m.device_id,m.origin,m.captured_at,m.content_hash,m.bytes,source_name(m.metadata.source),m.metadata.session_id,m.metadata.title,m.metadata.events,m.revision])?;
        let mut cache = PackCache::default();
        for c in &m.chunks {
            let hash = if c.hash.is_empty() {
                blake3::hash(pack::extract_chunk(cache.get(self, &c.pack)?, c)?)
                    .to_hex()
                    .to_string()
            } else {
                c.hash.clone()
            };
            self.db.execute(
                "INSERT OR IGNORE INTO chunks(hash,pack,offset,len) VALUES(?1,?2,?3,?4)",
                params![hash, c.pack, c.offset, c.len],
            )?;
        }
        Ok(())
    }

    pub fn manifest(&self, id: &str) -> Result<Manifest> {
        decode_manifest(
            &read_bounded(&self.manifest_path(id)?, MAX_MANIFEST_SIZE)?,
            id,
        )
    }

    pub fn resolve(&self, prefix: &str) -> Result<String> {
        ensure!(
            !prefix.is_empty()
                && prefix.len() <= 64
                && prefix
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "use a lowercase snapshot hash or unambiguous prefix"
        );
        let mut stmt = self
            .db
            .prepare("SELECT id FROM snapshots WHERE id LIKE ?1 LIMIT 2")?;
        let ids = stmt
            .query_map([format!("{prefix}%")], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match ids.as_slice() {
            [id] => Ok(id.clone()),
            [] => bail!("snapshot not found: {prefix}"),
            _ => bail!("ambiguous snapshot prefix: {prefix}"),
        }
    }

    pub fn list(
        &self,
        source: Option<Source>,
        limit: usize,
        all_versions: bool,
    ) -> Result<Vec<Snapshot>> {
        let filter = source.filter(|s| *s != Source::Auto).map(source_name);
        let sql = format!("SELECT id,origin,captured_at,content_hash,bytes,source,session_id,title,events,indexed,device_id,revision FROM snapshots s WHERE (?1 IS NULL OR source=?1) {} ORDER BY captured_at DESC,id DESC LIMIT ?2", if all_versions { "" } else { "AND s.rowid=(SELECT rowid FROM snapshots n WHERE n.device_id=s.device_id AND n.origin=s.origin ORDER BY revision DESC,captured_at DESC,id DESC LIMIT 1)" });
        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt
            .query_map(params![filter, i64::try_from(limit)?], snapshot_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn search(&self, query: &str, limit: usize, all_versions: bool) -> Result<Vec<Snapshot>> {
        let terms: Vec<_> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect();
        ensure!(!terms.is_empty(), "search needs at least one word");
        ensure!(terms.len() <= 64, "search has too many terms");
        let fts = terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" AND ");
        let literal = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let sql = format!("SELECT id,origin,captured_at,content_hash,bytes,source,session_id,title,events,indexed,device_id,revision FROM snapshots s WHERE (s.rowid IN (SELECT rowid FROM search WHERE search MATCH ?1) OR origin LIKE ?2 ESCAPE '\\' OR title LIKE ?2 ESCAPE '\\' OR session_id LIKE ?2 ESCAPE '\\') {} ORDER BY captured_at DESC,id DESC LIMIT ?3", if all_versions { "" } else { "AND s.rowid=(SELECT rowid FROM snapshots n WHERE n.device_id=s.device_id AND n.origin=s.origin ORDER BY revision DESC,captured_at DESC,id DESC LIMIT 1)" });
        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt
            .query_map(params![fts, literal, i64::try_from(limit)?], snapshot_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn export<W: Write>(&self, id: &str, out: &mut W) -> Result<()> {
        let m = self.manifest(id)?;
        let mut cache = PackCache::default();
        self.export_manifest(&m, out, &mut cache)
    }

    /// Read a trace as plaintext, independent of its native compression wrapper.
    /// Ordinary export/restore still reproduce the exact original container.
    pub fn export_decoded<W: Write>(&self, id: &str, out: &mut W) -> Result<()> {
        let m = self.manifest(id)?;
        let mut cache = PackCache::default();
        let mut reader = SnapshotReader::new(self, &mut cache, &m);
        if m.recipe.is_none() && adapters::is_zstd_source(Path::new(&m.origin)) {
            let mut decoder = zstd::stream::read::Decoder::new(reader)?;
            decoder.window_log_max(26)?;
            let size = std::io::copy(&mut decoder.take(native::MAX_DECODED + 1), out)?;
            ensure!(size <= native::MAX_DECODED, "decoded view exceeds the 2 GiB output budget; use exact export with an external decoder");
        } else {
            std::io::copy(&mut reader, out)?;
        }
        Ok(())
    }

    fn export_manifest<W: Write>(
        &self,
        m: &Manifest,
        out: &mut W,
        cache: &mut PackCache,
    ) -> Result<()> {
        let mut reader = SnapshotReader::new(self, cache, m);
        let mut output = native::HashWrite::new(out);
        if let Some(recipe) = m.recipe.as_ref().filter(|r| r.container.is_some()) {
            let mut encoder = native::encoder(&mut output, recipe.decoded_bytes)?;
            std::io::copy(&mut reader, &mut encoder)?;
            encoder.finish()?;
        } else {
            std::io::copy(&mut reader, &mut output)?;
        }
        ensure!(
            output.bytes == m.bytes && output.hash.finalize().to_hex().as_str() == m.content_hash,
            "snapshot content hash mismatch"
        );
        Ok(())
    }

    pub fn export_file(&self, id: &str, destination: &Path) -> Result<()> {
        ensure!(
            !destination.exists(),
            "destination already exists: {}",
            destination.display()
        );
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        self.export(id, tmp.as_file_mut())?;
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(destination)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn index(&mut self, max_bytes: usize) -> Result<usize> {
        ensure!(
            max_bytes > 0 && max_bytes <= 256 * 1024 * 1024,
            "index limit must be 1..256 MiB"
        );
        let ids: Vec<(i64, String)> = self
            .db
            .prepare("SELECT rowid,id FROM snapshots WHERE indexed=0 ORDER BY rowid")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut count = 0;
        let mut cache = PackCache::default();
        for (rowid, id) in ids {
            let m = self.manifest(&id)?;
            let reader = SnapshotReader::new(self, &mut cache, &m);
            // Recipes expose the decoded JSON directly; indexing must not
            // regenerate and then decompress the native container again.
            let path = if m.recipe.is_some() {
                Path::new("snapshot.jsonl")
            } else {
                Path::new(&m.origin)
            };
            let mut text = adapters::index_source(reader, max_bytes, path)?;
            text.push_str(&format!(
                " {} {} {}",
                m.origin,
                m.metadata.title.as_deref().unwrap_or(""),
                m.metadata.session_id.as_deref().unwrap_or("")
            ));
            let tx = self.db.transaction()?;
            tx.execute(
                "INSERT INTO search(rowid,text) VALUES(?1,?2)",
                params![rowid, text],
            )?;
            tx.execute("UPDATE snapshots SET indexed=1 WHERE rowid=?1", [rowid])?;
            tx.commit()?;
            count += 1;
        }
        self.checkpoint()?;
        Ok(count)
    }

    pub fn rebuild(&mut self) -> Result<usize> {
        // Verify before replacing the catalog; a corrupt backup cannot erase a useful index.
        self.verify()?;
        self.rebuild_verified()
    }

    fn rebuild_verified(&mut self) -> Result<usize> {
        let ids = self.object_ids("manifests", ".json.zst")?;
        self.db.execute_batch("BEGIN IMMEDIATE; DELETE FROM snapshots; DELETE FROM chunks; DELETE FROM observations; DELETE FROM objects; INSERT INTO search(search) VALUES('delete-all');")?;
        let result = (|| {
            let mut random = [0u8; 32];
            getrandom::fill(&mut random)
                .map_err(|e| anyhow::anyhow!("generate catalog identity: {e}"))?;
            self.db.execute(
                "UPDATE catalog_meta SET value=?1 WHERE key='generation'",
                [blake3::hash(&random).to_hex().to_string()],
            )?;
            for id in self.object_ids("packs", ".nz")? {
                self.register_object("packs", &id)?;
            }
            for id in &ids {
                self.insert_manifest(id, &self.manifest(id)?)?;
            }
            Ok::<_, anyhow::Error>(())
        })();
        match result {
            Ok(()) => self.db.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = self.db.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        self.checkpoint()?;
        Ok(ids.len())
    }

    pub fn verify(&self) -> Result<VerifyReport> {
        if self
            .db
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE name='snapshots'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            self.check_catalog_objects()?;
        }
        let mut report = VerifyReport::default();
        let mut referenced = std::collections::HashSet::new();
        let mut cache = PackCache::default();
        for id in self.object_ids("manifests", ".json.zst")? {
            let m = self.manifest(&id)?;
            for chunk in &m.chunks {
                referenced.insert(chunk.pack.clone());
            }
            self.export_manifest(&m, &mut std::io::sink(), &mut cache)?;
            report.snapshots += 1;
            report.checked_bytes += m.bytes;
        }
        for id in self.object_ids("packs", ".nz")? {
            cache.get(self, &id)?;
            report.packs += 1;
            if !referenced.contains(&id) {
                report.orphan_packs += 1;
            }
        }
        Ok(report)
    }

    pub fn stats(&self) -> Result<Stats> {
        self.checkpoint()?;
        let (snapshots, indexed_snapshots, logical_bytes): (u64, u64, u64) = self.db.query_row(
            "SELECT COUNT(*),COALESCE(SUM(indexed),0),COALESCE(SUM(bytes),0) FROM snapshots",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let origins = self.db.query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT device_id,origin FROM snapshots)",
            [],
            |r| r.get(0),
        )?;
        let latest_bytes = self.db.query_row("SELECT COALESCE(SUM(bytes),0) FROM snapshots s WHERE s.rowid=(SELECT rowid FROM snapshots n WHERE n.device_id=s.device_id AND n.origin=s.origin ORDER BY revision DESC,captured_at DESC,id DESC LIMIT 1)",[], |r|r.get(0))?;
        let unique_chunk_bytes =
            self.db
                .query_row("SELECT COALESCE(SUM(len),0) FROM chunks", [], |r| r.get(0))?;
        let packs = self.object_ids("packs", ".nz")?;
        let pack_bytes = directory_bytes(&self.root.join("packs"))?;
        let manifest_bytes = directory_bytes(&self.root.join("manifests"))?;
        let catalog_bytes = [
            "catalog.sqlite3",
            "catalog.sqlite3-wal",
            "catalog.sqlite3-shm",
        ]
        .iter()
        .map(|p| {
            fs::metadata(self.root.join(p))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum();
        let archive_bytes = pack_bytes + manifest_bytes;
        let total_bytes = walkdir::WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .try_fold(0u64, |total, entry| {
                let entry = entry?;
                Ok::<_, anyhow::Error>(
                    total
                        + if entry.file_type().is_file() {
                            entry.metadata()?.len()
                        } else {
                            0
                        },
                )
            })?;
        let other_bytes = total_bytes.saturating_sub(archive_bytes + catalog_bytes);
        Ok(Stats {
            snapshots,
            origins,
            indexed_snapshots,
            logical_bytes,
            latest_bytes,
            unique_chunk_bytes,
            packs: packs.len() as u64,
            pack_bytes,
            manifest_bytes,
            catalog_bytes,
            other_bytes,
            archive_bytes,
            total_bytes,
        })
    }

    fn object_ids(&self, directory: &str, suffix: &str) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.root.join(directory))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(id) = name.strip_suffix(suffix) {
                pack::validate_hash_id(id)?;
                ids.push(id.to_owned());
            }
        }
        ids.sort();
        Ok(ids)
    }

    fn checkpoint(&self) -> Result<()> {
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
}

fn snapshot_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Snapshot> {
    Ok(Snapshot {
        id: r.get(0)?,
        revision: r.get(11)?,
        origin: r.get(1)?,
        captured_at: r.get(2)?,
        content_hash: r.get(3)?,
        bytes: r.get(4)?,
        source: r.get(5)?,
        session_id: r.get(6)?,
        title: r.get(7)?,
        events: r.get(8)?,
        indexed: r.get(9)?,
        device_id: r.get(10)?,
    })
}

fn directory_bytes(path: &Path) -> Result<u64> {
    let mut size = 0;
    for e in fs::read_dir(path)? {
        let e = e?;
        if e.file_type()?.is_file() {
            size += e.metadata()?.len();
        }
    }
    Ok(size)
}

fn modified_key(metadata: &fs::Metadata) -> Result<String> {
    Ok(format!("{:?}", metadata.modified()?))
}

#[derive(Default)]
struct PackCache {
    entries: Vec<(String, Vec<u8>)>,
}

impl PackCache {
    fn get(&mut self, vault: &Vault, id: &str) -> Result<&[u8]> {
        if let Some(index) = self.entries.iter().position(|(key, _)| key == id) {
            self.entries.swap(0, index);
        } else {
            if self.entries.len() == 8 {
                self.entries.pop();
            }
            let encoded = read_bounded(&vault.pack_path(id)?, pack::MAX_PACK_RAW_SIZE + 65536)?;
            let raw = pack::decode_pack(&encoded, id)?;
            self.entries.insert(0, (id.to_owned(), raw));
        }
        Ok(&self.entries[0].1)
    }
}

/// A bounded pull reader over either legacy chunks or a lexical recipe.
struct SnapshotReader<'a> {
    vault: &'a Vault,
    cache: &'a mut PackCache,
    manifest: &'a Manifest,
    next: usize,
    current: Vec<u8>,
    position: usize,
    hash: blake3::Hasher,
    bytes: u64,
}
impl<'a> SnapshotReader<'a> {
    fn new(vault: &'a Vault, cache: &'a mut PackCache, manifest: &'a Manifest) -> Self {
        Self {
            vault,
            cache,
            manifest,
            next: 0,
            current: Vec::new(),
            position: 0,
            hash: blake3::Hasher::new(),
            bytes: 0,
        }
    }
    fn advance(&mut self) -> Result<bool> {
        let m = self.manifest;
        let (chunk, slice) = if let Some(recipe) = &m.recipe {
            match recipe.operations.get(self.next) {
                Some([index, offset, len, flags]) => {
                    (m.chunks.get(*index as usize), Some((*offset, *len, *flags)))
                }
                None => (None, None),
            }
        } else {
            (m.chunks.get(self.next), None)
        };
        let (size, expected) = m.recipe.as_ref().map_or((m.bytes, &m.content_hash), |r| {
            (r.decoded_bytes, &r.decoded_hash)
        });
        let Some(chunk) = chunk else {
            ensure!(
                self.bytes == size && self.hash.finalize().to_hex().as_str() == expected,
                "snapshot content hash mismatch"
            );
            return Ok(false);
        };
        let raw = pack::extract_chunk(self.cache.get(self.vault, &chunk.pack)?, chunk)?;
        self.current = if let Some((offset, len, flags)) = slice {
            structure::expand(&raw[offset as usize..offset as usize + len as usize], flags)?
        } else {
            raw.to_vec()
        };
        self.bytes += self.current.len() as u64;
        ensure!(self.bytes <= size, "recipe exceeds declared output size");
        self.hash.update(&self.current);
        self.position = 0;
        self.next += 1;
        Ok(true)
    }
}
impl Read for SnapshotReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.position == self.current.len() {
            if !self.advance().map_err(std::io::Error::other)? {
                return Ok(0);
            }
        }
        let n = out.len().min(self.current.len() - self.position);
        out[..n].copy_from_slice(&self.current[self.position..self.position + n]);
        self.position += n;
        Ok(n)
    }
}
