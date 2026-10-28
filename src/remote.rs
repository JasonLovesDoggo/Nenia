//! Additive replication of immutable archive objects. The caller must hold the vault lock.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use futures::{stream, StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutMode};
use serde::{Deserialize, Serialize};
use url::Url;

const MAX_PACK_BYTES: u64 = crate::pack::MAX_PACK_ENCODED_SIZE as u64;
const MAX_MANIFEST_BYTES: u64 = crate::vault::MAX_MANIFEST_SIZE as u64;

#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Maximum number of objects in flight, between 1 and 32.
    pub concurrency: usize,
    /// Read and hash destination objects before skipping them. Disabling this
    /// trusts destination names and sizes and cannot detect silent corruption.
    pub verify_existing: bool,
    /// HTTP bearer token. Falls back to NENIA_TOKEN when absent.
    pub token: Option<String>,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            concurrency: 4,
            verify_existing: true,
            token: None,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct SyncReport {
    pub transferred_objects: u64,
    /// Bytes of newly published objects, not total network traffic.
    pub transferred_bytes: u64,
    pub existing_objects: u64,
    pub verified_existing_objects: u64,
}

impl SyncReport {
    fn add(&mut self, other: Self) {
        self.transferred_objects += other.transferred_objects;
        self.transferred_bytes += other.transferred_bytes;
        self.existing_objects += other.existing_objects;
        self.verified_existing_objects += other.verified_existing_objects;
    }

    fn existing(verified: bool) -> Self {
        Self {
            existing_objects: 1,
            verified_existing_objects: u64::from(verified),
            ..Self::default()
        }
    }

    fn transferred(size: u64) -> Self {
        Self {
            transferred_objects: 1,
            transferred_bytes: size,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pack,
    Manifest,
}

impl Kind {
    fn directory(self) -> &'static str {
        match self {
            Self::Pack => "packs",
            Self::Manifest => "manifests",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Pack => ".nz",
            Self::Manifest => ".json.zst",
        }
    }

    fn limit(self) -> u64 {
        match self {
            Self::Pack => MAX_PACK_BYTES,
            Self::Manifest => MAX_MANIFEST_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
struct Object {
    kind: Kind,
    id: String,
    size: Option<u64>,
}

impl Object {
    fn parse(key: &str, size: Option<u64>) -> Option<Self> {
        let (directory, filename) = key.split_once('/')?;
        let kind = match directory {
            "packs" => Kind::Pack,
            "manifests" => Kind::Manifest,
            _ => return None,
        };
        let id = filename.strip_suffix(kind.extension())?;
        if id.len() != 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        Some(Self {
            kind,
            id: id.to_owned(),
            size,
        })
    }

    fn key(&self) -> String {
        format!(
            "{}/{}{}",
            self.kind.directory(),
            self.id,
            self.kind.extension()
        )
    }

    fn check_size(&self) -> Result<()> {
        ensure!(
            self.size.is_none_or(|size| size <= self.kind.limit()),
            "object exceeds size limit: {}",
            self.key()
        );
        Ok(())
    }
}

type Inventory = BTreeMap<String, Object>;

struct Remote {
    backend: Backend,
    prefix: String,
    local_root: Option<PathBuf>,
}

enum Backend {
    ObjectStore(Box<dyn ObjectStore>),
    Http { client: reqwest::Client, base: Url },
}

#[derive(Deserialize)]
struct HttpInventory {
    packs: Vec<String>,
    manifests: Vec<String>,
}

impl Remote {
    fn open(remote: &str, create: bool, options: &SyncOptions) -> Result<Self> {
        let mut url = Url::parse(remote)
            .context("remote must be an absolute file://, s3://, or HTTPS URL")?;
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "remote URL cannot have a query or fragment"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "remote URL cannot include credentials"
        );
        match url.scheme() {
            "file" => {
                ensure!(url.port().is_none(), "file remote cannot include a port");
                let root = url.to_file_path().map_err(|()| {
                    anyhow::anyhow!("file remote must name an absolute local directory")
                })?;
                if create {
                    fs::create_dir_all(&root).context("create file remote")?;
                }
                let root = root.canonicalize().context("open file remote directory")?;
                ensure!(root.is_dir(), "file remote must be a directory");
                Ok(Self {
                    backend: Backend::ObjectStore(Box::new(LocalFileSystem::new_with_prefix(
                        &root,
                    )?)),
                    prefix: String::new(),
                    local_root: Some(root),
                })
            }
            "s3" => {
                ensure!(
                    url.port().is_none(),
                    "S3 URL cannot include a port; configure AWS_ENDPOINT separately"
                );
                let bucket = url.host_str().context("S3 remote requires a bucket")?;
                // Validate the original path because URL parsers normalize ../ segments.
                let raw_path = remote
                    .strip_prefix("s3://")
                    .context("use lowercase s3://")?
                    .split_once('/')
                    .map_or("", |(_, path)| path);
                ensure!(
                    raw_path
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b)),
                    "S3 prefix supports ASCII letters, digits, /, _, -, and . only"
                );
                let prefix = raw_path.trim_matches('/');
                ensure!(
                    prefix.is_empty()
                        || prefix
                            .split('/')
                            .all(|part| !part.is_empty() && part != "." && part != ".."),
                    "S3 prefix contains an empty or traversal segment"
                );
                let store = AmazonS3Builder::from_env()
                    .with_bucket_name(bucket)
                    .build()
                    .context("configure S3 remote using AWS environment variables")?;
                Ok(Self {
                    backend: Backend::ObjectStore(Box::new(store)),
                    prefix: if prefix.is_empty() {
                        String::new()
                    } else {
                        format!("{prefix}/")
                    },
                    local_root: None,
                })
            }
            "https" | "http" => {
                ensure!(url.host_str().is_some(), "HTTP remote requires a host");
                let path = url.path().trim_end_matches('/').to_owned() + "/";
                url.set_path(&path);
                let mut headers = reqwest::header::HeaderMap::new();
                if let Some(token) = options
                    .token
                    .clone()
                    .or_else(|| std::env::var("NENIA_TOKEN").ok())
                {
                    ensure!(!token.is_empty(), "NENIA_TOKEN must not be empty");
                    let mut header =
                        reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                            .context("invalid HTTP authentication token")?;
                    header.set_sensitive(true);
                    headers.insert(reqwest::header::AUTHORIZATION, header);
                }
                let client = reqwest::Client::builder()
                    .default_headers(headers)
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(std::time::Duration::from_secs(15))
                    .timeout(std::time::Duration::from_secs(300))
                    .build()?;
                Ok(Self {
                    backend: Backend::Http { client, base: url },
                    prefix: String::new(),
                    local_root: None,
                })
            }
            scheme => bail!("unsupported remote scheme {scheme:?}; use file://, s3://, or HTTPS"),
        }
    }

    fn key(&self, object: &Object) -> ObjectPath {
        ObjectPath::from(format!("{}{}", self.prefix, object.key()))
    }

    async fn inventory(&self) -> Result<Inventory> {
        if let Some(root) = &self.local_root {
            return local_inventory(root);
        }
        let mut inventory = Inventory::new();
        let store = match &self.backend {
            Backend::ObjectStore(store) => store,
            Backend::Http { client, base } => {
                let response = send_http(client.get(base.join("v1/objects")?)).await?;
                let bytes = read_http_body(response, MAX_MANIFEST_BYTES).await?;
                let objects: HttpInventory =
                    serde_json::from_slice(&bytes).context("invalid remote inventory")?;
                for (kind, ids) in [
                    (Kind::Pack, objects.packs),
                    (Kind::Manifest, objects.manifests),
                ] {
                    for id in ids {
                        let key = format!("{}/{id}{}", kind.directory(), kind.extension());
                        let object = Object::parse(&key, None)
                            .context("invalid hash in remote inventory")?;
                        inventory.insert(key, object);
                    }
                }
                return Ok(inventory);
            }
        };
        let prefix = ObjectPath::from(self.prefix.trim_end_matches('/'));
        let mut listing = store.list(Some(&prefix));
        while let Some(meta) = listing
            .try_next()
            .await
            .context("list remote archive objects")?
        {
            if let Some(key) = meta.location.as_ref().strip_prefix(&self.prefix) {
                if let Some(object) = Object::parse(key, Some(meta.size)) {
                    inventory.insert(object.key(), object);
                }
            }
        }
        Ok(inventory)
    }

    async fn read(&self, object: &Object) -> Result<Vec<u8>> {
        object.check_size()?;
        if let Some(root) = &self.local_root {
            return read_local(root, object).await;
        }
        let store = match &self.backend {
            Backend::ObjectStore(store) => store,
            Backend::Http { client, base } => {
                let url = base.join(&format!("v1/{}/{}", object.kind.directory(), object.id))?;
                let response = send_http(client.get(url)).await?;
                let bytes = read_http_body(response, object.kind.limit()).await?;
                validate_bytes(object, &bytes)?;
                return Ok(bytes);
            }
        };
        let result = store
            .get(&self.key(object))
            .await
            .with_context(|| format!("download {}", object.key()))?;
        ensure!(
            result.meta.size <= object.kind.limit(),
            "remote object exceeds size limit: {}",
            object.key()
        );
        let mut chunks = result.into_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.try_next().await.context("read remote object body")? {
            ensure!(
                bytes.len() as u64 + chunk.len() as u64 <= object.kind.limit(),
                "remote object body exceeds size limit: {}",
                object.key()
            );
            bytes.extend_from_slice(&chunk);
        }
        validate_bytes(object, &bytes)?;
        Ok(bytes)
    }

    async fn put(&self, object: &Object, bytes: Vec<u8>) -> Result<SyncReport> {
        if let Some(root) = &self.local_root {
            return write_local(root, object, bytes).await;
        }
        let size = bytes.len() as u64;
        let store = match &self.backend {
            Backend::ObjectStore(store) => store,
            Backend::Http { client, base } => {
                let url = base.join(&format!("v1/{}/{}", object.kind.directory(), object.id))?;
                let response = send_http(
                    client
                        .put(url)
                        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                        .body(bytes),
                )
                .await?;
                return match response.status() {
                    reqwest::StatusCode::CREATED => Ok(SyncReport::transferred(size)),
                    reqwest::StatusCode::NO_CONTENT => {
                        self.read(object)
                            .await
                            .context("verify concurrent HTTP publication")?;
                        Ok(SyncReport::existing(true))
                    }
                    status => bail!("unexpected HTTP upload status {status}"),
                };
            }
        };
        match store
            .put_opts(&self.key(object), bytes.into(), PutMode::Create.into())
            .await
        {
            Ok(_) => Ok(SyncReport::transferred(size)),
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => {
                // Another writer may have published the same object since listing.
                self.read(object)
                    .await
                    .context("verify concurrent remote publication")?;
                Ok(SyncReport::existing(true))
            }
            Err(error) => Err(error).with_context(|| format!("publish {}", object.key())),
        }
    }
}

async fn send_http(request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    for attempt in 0..4 {
        let result = request
            .try_clone()
            .context("HTTP request cannot be retried")?
            .send()
            .await;
        match result {
            Ok(response) => {
                let status = response.status();
                let retryable = status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT;
                if !retryable || attempt == 3 {
                    ensure!(status.is_success(), "remote returned HTTP {status}");
                    return Ok(response);
                }
            }
            Err(error) if attempt < 3 && (error.is_connect() || error.is_timeout()) => {}
            Err(error) => return Err(error).context("HTTP archive request failed"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(200 << attempt)).await;
    }
    unreachable!("the last HTTP attempt always returns")
}

async fn read_http_body(response: reqwest::Response, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        response.content_length().is_none_or(|len| len <= limit),
        "HTTP response exceeds size limit"
    );
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.try_next().await.context("read HTTP response body")? {
        ensure!(
            bytes.len() as u64 + chunk.len() as u64 <= limit,
            "HTTP response exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn real_directory(path: &Path, create: bool) -> Result<()> {
    if create {
        fs::create_dir_all(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_dir(),
        "archive directory must not be a symlink: {}",
        path.display()
    );
    Ok(())
}

fn local_inventory(root: &Path) -> Result<Inventory> {
    ensure!(
        root.is_dir(),
        "archive directory does not exist: {}",
        root.display()
    );
    let mut inventory = Inventory::new();
    for kind in [Kind::Pack, Kind::Manifest] {
        let directory = root.join(kind.directory());
        match fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(metadata) => ensure!(
                metadata.file_type().is_dir(),
                "archive directory must not be a symlink: {}",
                directory.display()
            ),
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let key = format!(
                "{}/{}",
                kind.directory(),
                entry.file_name().to_string_lossy()
            );
            if let Some(mut object) = Object::parse(&key, None) {
                ensure!(
                    entry.file_type()?.is_file(),
                    "archive object must be a regular file: {}",
                    entry.path().display()
                );
                object.size = Some(entry.metadata()?.len());
                inventory.insert(key, object);
            }
        }
    }
    Ok(inventory)
}

fn validate_bytes(object: &Object, bytes: &[u8]) -> Result<()> {
    ensure!(
        object.size.is_none_or(|size| bytes.len() as u64 == size),
        "object size changed or differs: {}",
        object.key()
    );
    ensure!(
        blake3::hash(bytes).to_hex().as_str() == object.id,
        "object hash mismatch: {}",
        object.key()
    );
    Ok(())
}

fn check_references(object: &Object, bytes: &[u8], packs: &HashSet<&str>) -> Result<()> {
    if object.kind == Kind::Manifest {
        let manifest = crate::vault::decode_manifest(bytes, &object.id)?;
        for chunk in manifest.chunks {
            ensure!(
                packs.contains(chunk.pack.as_str()),
                "manifest {} references missing pack {}",
                object.id,
                chunk.pack
            );
        }
    }
    Ok(())
}

fn read_local_sync(root: &Path, object: &Object) -> Result<Vec<u8>> {
    object.check_size()?;
    real_directory(&root.join(object.kind.directory()), false)?;
    let path = root.join(object.key());
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.file_type().is_file(),
        "archive object must be a regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= object.kind.limit(),
        "local object exceeds size limit: {}",
        object.key()
    );
    let mut bytes = Vec::new();
    File::open(&path)?
        .take(object.kind.limit() + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= object.kind.limit(),
        "local object exceeds size limit: {}",
        object.key()
    );
    validate_bytes(object, &bytes)?;
    Ok(bytes)
}

async fn read_local(root: &Path, object: &Object) -> Result<Vec<u8>> {
    let root = root.to_owned();
    let object = object.clone();
    tokio::task::spawn_blocking(move || read_local_sync(&root, &object)).await?
}

async fn write_local(root: &Path, object: &Object, bytes: Vec<u8>) -> Result<SyncReport> {
    let root = root.to_owned();
    let object = object.clone();
    tokio::task::spawn_blocking(move || {
        let directory = root.join(object.kind.directory());
        real_directory(&directory, true)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".nenia-")
            .suffix(".tmp")
            .tempfile_in(&directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(root.join(object.key())) {
            Ok(_) => {
                #[cfg(unix)]
                File::open(&directory)?.sync_all()?;
                Ok(SyncReport::transferred(bytes.len() as u64))
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                read_local_sync(&root, &object).context("verify existing local publication")?;
                Ok(SyncReport::existing(true))
            }
            Err(error) => Err(error).context("publish local archive object"),
        }
    })
    .await?
}

/// A previous attempt may have published a file before its directory sync failed.
/// Flush phase directories even when every addressed object was already present.
async fn sync_local_directory(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let directory = directory.to_owned();
        tokio::task::spawn_blocking(move || {
            match fs::symlink_metadata(&directory) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
                Ok(metadata) => ensure!(
                    metadata.file_type().is_dir(),
                    "archive directory must not be a symlink: {}",
                    directory.display()
                ),
            }
            File::open(&directory)?
                .sync_all()
                .with_context(|| format!("flush archive directory {}", directory.display()))
        })
        .await?
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(())
    }
}

fn check_options(options: &SyncOptions) -> Result<()> {
    ensure!(
        (1..=32).contains(&options.concurrency),
        "sync concurrency must be between 1 and 32"
    );
    Ok(())
}

/// Upload packs, then publish manifests. Never removes or overwrites objects.
/// The caller must hold the vault's exclusive writer lock throughout this call.
/// A lock-owning server may publish new immutable source objects concurrently;
/// a missed dependency fails safely and is included in a later sync.
pub async fn push(vault: &Path, remote: &str, options: &SyncOptions) -> Result<SyncReport> {
    check_options(options)?;
    let source = local_inventory(vault)?;
    let remote = Remote::open(remote, true, options)?;
    let destination = remote.inventory().await?;
    let packs: HashSet<_> = source
        .values()
        .filter(|object| object.kind == Kind::Pack)
        .map(|object| object.id.as_str())
        .collect();
    let mut report = SyncReport::default();
    for kind in [Kind::Pack, Kind::Manifest] {
        let objects: Vec<_> = source
            .values()
            .filter(|object| object.kind == kind)
            .cloned()
            .collect();
        let results = stream::iter(objects)
            .map(|object| {
                let remote = &remote;
                let destination = &destination;
                let packs = &packs;
                async move {
                    if let Some(existing) = destination.get(&object.key()) {
                        ensure!(
                            existing.size.is_none() || existing.size == object.size,
                            "destination object size mismatch: {}",
                            object.key()
                        );
                        if !options.verify_existing {
                            return Ok(SyncReport::existing(false));
                        }
                        let bytes = read_local(vault, &object).await?;
                        check_references(&object, &bytes, packs)?;
                        drop(bytes);
                        remote.read(&object).await?;
                        Ok(SyncReport::existing(true))
                    } else {
                        let bytes = read_local(vault, &object).await?;
                        check_references(&object, &bytes, packs)?;
                        remote.put(&object, bytes).await
                    }
                }
            })
            .buffer_unordered(options.concurrency);
        futures::pin_mut!(results);
        while let Some(result) = results.try_next().await? {
            report.add(result);
        }
        if let Some(root) = &remote.local_root {
            sync_local_directory(&root.join(kind.directory())).await?;
        }
    }
    if let Some(root) = &remote.local_root {
        // Persist newly created packs/ and manifests/ directory entries as well.
        sync_local_directory(root).await?;
    }
    Ok(report)
}

/// Download packs before manifests. The catalog is deliberately not copied;
/// rebuild it after pulling. The caller must hold the exclusive vault lock.
pub async fn pull(vault: &Path, remote: &str, options: &SyncOptions) -> Result<SyncReport> {
    check_options(options)?;
    let remote = Remote::open(remote, false, options)?;
    let source = remote.inventory().await?;
    fs::create_dir_all(vault).context("create destination vault directory")?;
    let destination = local_inventory(vault)?;
    let packs: HashSet<_> = source
        .values()
        .filter(|object| object.kind == Kind::Pack)
        .map(|object| object.id.as_str())
        .collect();
    let mut report = SyncReport::default();
    for kind in [Kind::Pack, Kind::Manifest] {
        let objects: Vec<_> = source
            .values()
            .filter(|object| object.kind == kind)
            .cloned()
            .collect();
        let results = stream::iter(objects)
            .map(|object| {
                let remote = &remote;
                let destination = &destination;
                let packs = &packs;
                async move {
                    object.check_size()?;
                    if let Some(existing) = destination.get(&object.key()) {
                        ensure!(
                            object.size.is_none() || existing.size == object.size,
                            "destination object size mismatch: {}",
                            object.key()
                        );
                        // Even the trust option validates manifests and their dependencies.
                        let verified = options.verify_existing || kind == Kind::Manifest;
                        if verified {
                            let bytes = read_local(vault, &object).await?;
                            check_references(&object, &bytes, packs)?;
                        }
                        Ok(SyncReport::existing(verified))
                    } else {
                        let bytes = remote.read(&object).await?;
                        check_references(&object, &bytes, packs)?;
                        write_local(vault, &object, bytes).await
                    }
                }
            })
            .buffer_unordered(options.concurrency);
        futures::pin_mut!(results);
        while let Some(result) = results.try_next().await? {
            report.add(result);
        }
        sync_local_directory(&vault.join(kind.directory())).await?;
    }
    sync_local_directory(vault).await?;
    Ok(report)
}
