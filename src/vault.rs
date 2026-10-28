use crate::adapters::{self, Candidate, Metadata, Source};
use crate::pack::{self, ChunkRef, Chunker, PackBuilder};
use anyhow::{bail, ensure, Context, Result};
use chrono::Utc;
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

pub const MAX_MANIFEST_SIZE: usize = 64 * 1024 * 1024;
pub const DEFAULT_INDEX_BYTES: usize = 8 * 1024 * 1024;

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
    pub device_id: String,
    pub origin: String,
    pub captured_at: String,
    pub content_hash: String,
    pub bytes: u64,
    pub metadata: Metadata,
    pub chunks: Vec<ChunkRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
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
    ensure!(m.version == 1, "unsupported manifest version {}", m.version);
    pack::validate_hash_id(&m.content_hash)?;
    let mut total = 0u64;
    for c in &m.chunks {
        pack::validate_hash_id(&c.hash)?;
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
    ensure!(
        total == m.bytes,
        "manifest chunk lengths do not match snapshot size"
    );
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
                pack::atomic_write_immutable(&self.pack_path(id)?, bytes)
            }
            "manifests" => {
                let m = decode_manifest(bytes, id)?;
                let mut cache = PackCache::default();
                let mut hash = blake3::Hasher::new();
                for c in &m.chunks {
                    hash.update(pack::extract_chunk(cache.get(self, &c.pack)?, c)?);
                }
                ensure!(
                    hash.finalize().to_hex().as_str() == m.content_hash,
                    "snapshot content hash mismatch"
                );
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
        fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
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
        let db = Connection::open(root.join("catalog.sqlite3"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS chunks(hash TEXT PRIMARY KEY, pack TEXT NOT NULL, offset INTEGER NOT NULL, len INTEGER NOT NULL) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS snapshots(id TEXT PRIMARY KEY, device_id TEXT NOT NULL, origin TEXT NOT NULL, captured_at TEXT NOT NULL, content_hash TEXT NOT NULL, bytes INTEGER NOT NULL, source TEXT NOT NULL, session_id TEXT, title TEXT, events INTEGER NOT NULL, indexed INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS snapshots_origin ON snapshots(device_id, origin, captured_at);
            CREATE INDEX IF NOT EXISTS snapshots_content ON snapshots(device_id, origin, content_hash);
            CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(text, content='', detail=none, columnsize=0);")?;
        Ok(Self {
            root: root.to_owned(),
            config,
            db,
            _lock: lock,
        })
    }

    pub fn ingest(&mut self, files: &[Candidate], source_override: Source) -> Result<IngestReport> {
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

    fn ingest_batch(
        &self,
        files: &[Candidate],
        source_override: Source,
        report: &mut IngestReport,
    ) -> Result<()> {
        let mut builder = PackBuilder::new(self.config.compression_level);
        let mut pending_chunks: HashMap<String, (u64, u32)> = HashMap::new();
        let mut pending = Vec::new();
        for candidate in files {
            let path = fs::canonicalize(&candidate.path)
                .with_context(|| format!("resolve {}", candidate.path.display()))?;
            ensure!(
                !path.starts_with(fs::canonicalize(&self.root)?),
                "refusing to ingest the vault itself"
            );
            let file = File::open(&path)?;
            let initial = file.metadata()?;
            ensure!(
                initial.is_file(),
                "source is not a regular file: {}",
                path.display()
            );
            let source = if source_override == Source::Auto {
                candidate.source
            } else {
                source_override
            };
            let metadata = adapters::inspect(
                BufReader::new(File::open(&path)?.take(initial.len())),
                source,
                &path,
            )?;
            let mut reader = Chunker::new(BufReader::new(file.take(initial.len())));
            let mut hasher = blake3::Hasher::new();
            let mut hashes = Vec::new();
            let mut size = 0u64;
            while let Some(chunk) = reader.next_chunk()? {
                hasher.update(&chunk);
                size += chunk.len() as u64;
                let hash = blake3::hash(&chunk).to_hex().to_string();
                let exists = pending_chunks.contains_key(&hash)
                    || self
                        .db
                        .query_row("SELECT 1 FROM chunks WHERE hash=?1", [&hash], |_| Ok(()))
                        .optional()?
                        .is_some();
                if !exists {
                    if !builder.can_fit(chunk.len()) {
                        self.flush_pack(&mut builder, &mut pending_chunks, report)?;
                    }
                    let offset = builder.add(&chunk)?;
                    pending_chunks.insert(hash.clone(), (offset, chunk.len() as u32));
                    report.new_chunk_bytes += chunk.len() as u64;
                }
                hashes.push(hash);
            }
            ensure!(
                size == initial.len(),
                "source was truncated during capture: {}",
                path.display()
            );
            let content_hash = hasher.finalize().to_hex().to_string();
            let origin = path.to_string_lossy().into_owned();
            let exists = self.db.query_row("SELECT 1 FROM snapshots WHERE device_id=?1 AND origin=?2 AND content_hash=?3 LIMIT 1", params![self.config.device_id, origin, content_hash], |_| Ok(())).optional()?.is_some();
            report.input_bytes += size;
            if exists {
                report.unchanged += 1;
                continue;
            }
            pending.push((
                Manifest {
                    version: 1,
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
        }
        self.flush_pack(&mut builder, &mut pending_chunks, report)?;
        for (mut manifest, hashes) in pending {
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
        let raw = serde_json::to_vec(manifest)?;
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
        self.db.execute("INSERT OR IGNORE INTO snapshots(id,device_id,origin,captured_at,content_hash,bytes,source,session_id,title,events) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![id,m.device_id,m.origin,m.captured_at,m.content_hash,m.bytes,source_name(m.metadata.source),m.metadata.session_id,m.metadata.title,m.metadata.events])?;
        for c in &m.chunks {
            self.db.execute(
                "INSERT OR IGNORE INTO chunks(hash,pack,offset,len) VALUES(?1,?2,?3,?4)",
                params![c.hash, c.pack, c.offset, c.len],
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
        let sql = format!("SELECT id,origin,captured_at,content_hash,bytes,source,session_id,title,events,indexed,device_id FROM snapshots s WHERE (?1 IS NULL OR source=?1) {} ORDER BY captured_at DESC,id DESC LIMIT ?2", if all_versions { "" } else { "AND s.rowid=(SELECT rowid FROM snapshots n WHERE n.device_id=s.device_id AND n.origin=s.origin ORDER BY captured_at DESC,id DESC LIMIT 1)" });
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
        let sql = format!("SELECT id,origin,captured_at,content_hash,bytes,source,session_id,title,events,indexed,device_id FROM snapshots s WHERE (s.rowid IN (SELECT rowid FROM search WHERE search MATCH ?1) OR origin LIKE ?2 ESCAPE '\\' OR title LIKE ?2 ESCAPE '\\' OR session_id LIKE ?2 ESCAPE '\\') {} ORDER BY captured_at DESC,id DESC LIMIT ?3", if all_versions { "" } else { "AND s.rowid=(SELECT rowid FROM snapshots n WHERE n.device_id=s.device_id AND n.origin=s.origin ORDER BY captured_at DESC,id DESC LIMIT 1)" });
        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt
            .query_map(params![fts, literal, i64::try_from(limit)?], snapshot_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn export<W: Write>(&self, id: &str, out: &mut W) -> Result<()> {
        let m = self.manifest(id)?;
        let mut cache = PackCache::default();
        let mut hash = blake3::Hasher::new();
        for c in &m.chunks {
            let raw = cache.get(self, &c.pack)?;
            let bytes = pack::extract_chunk(raw, c)?;
            hash.update(bytes);
            out.write_all(bytes)?;
        }
        ensure!(
            hash.finalize().to_hex().as_str() == m.content_hash,
            "snapshot content hash mismatch: {id}"
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
        for (rowid, id) in ids {
            let m = self.manifest(&id)?;
            // Stream through a temp file so a huge trace never becomes a huge allocation.
            let mut temp = tempfile::tempfile()?;
            self.export(&id, &mut temp)?;
            use std::io::{Seek, SeekFrom};
            temp.seek(SeekFrom::Start(0))?;
            let mut text = adapters::index_text(BufReader::new(temp), max_bytes)?;
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
        let ids = self.object_ids("manifests", ".json.zst")?;
        self.db.execute_batch("BEGIN IMMEDIATE; DELETE FROM snapshots; DELETE FROM chunks; INSERT INTO search(search) VALUES('delete-all');")?;
        let result = (|| {
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
        let mut report = VerifyReport::default();
        let mut referenced = std::collections::HashSet::new();
        let mut cache = PackCache::default();
        for id in self.object_ids("manifests", ".json.zst")? {
            let m = self.manifest(&id)?;
            let mut hash = blake3::Hasher::new();
            for c in &m.chunks {
                referenced.insert(c.pack.clone());
                hash.update(pack::extract_chunk(cache.get(self, &c.pack)?, c)?);
            }
            ensure!(
                hash.finalize().to_hex().as_str() == m.content_hash,
                "snapshot content hash mismatch: {id}"
            );
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
        let latest_bytes = self.db.query_row("SELECT COALESCE(SUM(bytes),0) FROM snapshots s WHERE s.rowid=(SELECT rowid FROM snapshots n WHERE n.device_id=s.device_id AND n.origin=s.origin ORDER BY captured_at DESC,id DESC LIMIT 1)",[], |r|r.get(0))?;
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
            archive_bytes,
            total_bytes: archive_bytes + catalog_bytes,
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

#[derive(Default)]
struct PackCache {
    id: String,
    raw: Vec<u8>,
}

impl PackCache {
    fn get(&mut self, vault: &Vault, id: &str) -> Result<&[u8]> {
        if self.id != id {
            let encoded = read_bounded(&vault.pack_path(id)?, pack::MAX_PACK_RAW_SIZE + 65536)?;
            self.raw = pack::decode_pack(&encoded, id)?;
            self.id = id.to_owned();
        }
        Ok(&self.raw)
    }
}
