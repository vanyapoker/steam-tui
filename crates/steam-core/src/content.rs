//! Native SteamPipe content transport. Format provenance: SteamKit2
//! `Types/{DepotManifest,Manifest}.cs`, `Steam/CDN/{Client,DepotChunk}.cs`,
//! `Util/{VZipUtil,VZstdUtil}.cs` at
//! https://github.com/SteamRE/SteamKit/tree/e21d0fc9d670936244a5fb62ee9f804e87c94b1e/SteamKit2/SteamKit2 .
//! Manifests are ZIP-wrapped framed protobuf (v5) or binary v4, not whole-file
//! AES. Only filenames and compressed chunks use encrypted-IV AES256-CBC.
//! Secrets stay in zeroizing memory and HTTP/RPC errors never expose URLs.
//! Limits: 128 MiB manifests, 16 MiB encrypted/plain chunks, 64 MiB decoder
//! dictionaries/windows, one ordinary (non-ZIP64) ZIP entry, eight direct HTTPS
//! servers. Proxy-only/HTTP-only caches and encrypted private branches are not
//! guessed or silently substituted. Manifest signature sections are structurally
//! checked, not cryptographically authenticated; trust comes from CM-authorized
//! manifest IDs, normal CDN TLS, filename/payload checks and chunk/file hashes.

mod formats;
#[cfg(test)]
mod tests;

use crate::catalog::DepotSpec;
use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver_2::{
    CMsgClientGetDepotDecryptionKey, CMsgClientGetDepotDecryptionKeyResponse,
};
use steam_vent_proto_steam::steammessages_contentsystem_steamclient::*;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use zeroize::{Zeroize, Zeroizing};

pub type Error = Box<dyn StdError + Send + Sync>;
type Result<T, E = Error> = std::result::Result<T, E>;
const MAX_MANIFEST: usize = 128 * 1024 * 1024;
const MAX_CHUNK: usize = 16 * 1024 * 1024;
const MAX_WINDOW: u64 = 64 * 1024 * 1024;
const MAX_SERVERS: usize = 8;
const DEADLINE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct DepotManifest {
    pub depot_id: u32,
    pub manifest_id: u64,
    pub files: Vec<DepotFile>,
}
#[derive(Clone, Debug)]
pub struct DepotFile {
    pub path: PathBuf,
    pub size: u64,
    pub sha1: Option<[u8; 20]>,
    pub flags: u32,
    pub chunks: Vec<DepotChunk>,
    pub link_target: Option<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DepotChunk {
    pub id: [u8; 20],
    pub offset: u64,
    pub uncompressed_size: u32,
    pub compressed_size: u32,
    pub adler: u32,
}

/// Verified plaintext from a chunk of a CM-authorized manifest.
/// Keeping the descriptor binds the verification to the eventual write offset.
pub struct VerifiedChunk {
    chunk: DepotChunk,
    bytes: Vec<u8>,
}
impl VerifiedChunk {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn bytes_for(&self, chunk: &DepotChunk) -> Result<&[u8]> {
        if &self.chunk != chunk {
            return Err(failure(
                "Verified chunk descriptor does not match the write",
            ));
        }
        Ok(&self.bytes)
    }
}

fn failure(message: impl Into<String>) -> Error {
    Box::new(io::Error::other(message.into()))
}
fn cancelled() -> Error {
    Box::new(io::Error::new(
        io::ErrorKind::Interrupted,
        "content download cancelled",
    ))
}
async fn bounded<T>(
    cancel: &CancellationToken,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(cancelled()),
        result = tokio::time::timeout(DEADLINE, operation) => result.map_err(|_| failure("Steam content request timed out"))?,
    }
}

struct Server {
    host: String,
    allowed_apps: Vec<u32>,
}
struct Auth {
    token: Zeroizing<String>,
    expires: u32,
}
type DepotIdentity = (u32, u64, u32);
#[derive(Default)]
struct Cache {
    keys: HashMap<(u32, u32), Zeroizing<[u8; 32]>>,
    tokens: HashMap<(u32, u32, String), Auth>,
    chunks: HashMap<DepotIdentity, HashSet<DepotChunk>>,
}
/// One install's bounded HTTPS client; compatible with a Tokio LocalSet.
/// No plaintext depot keys, tokens or credentials are written to disk.
pub struct ContentClient {
    connection: Connection,
    http: reqwest::Client,
    servers: Vec<Server>,
    cache: Mutex<Cache>,
    branch: String,
}
impl ContentClient {
    pub async fn new(connection: Connection) -> Result<Self> {
        let cancel = CancellationToken::new();
        let response = bounded(&cancel, async {
            connection
                .service_method(CContentServerDirectory_GetServersForSteamPipe_Request {
                    cell_id: Some(connection.cell_id()),
                    max_servers: Some(32),
                    ..Default::default()
                })
                .await
                .map_err(|error| failure(format!("Steam CDN directory request failed: {error}")))
        })
        .await?;
        let mut entries = response.servers;
        entries.sort_by(|a, b| {
            a.priority_class()
                .cmp(&b.priority_class())
                .then_with(|| a.weighted_load().total_cmp(&b.weighted_load()))
        });
        let mut servers = Vec::new();
        let mut seen = HashSet::new();
        for server in entries {
            if server.use_as_proxy()
                || server.steam_china_only()
                || !matches!(server.type_(), "CDN" | "SteamCache")
                || server.https_support() == "none"
            {
                continue;
            }
            let host = if server.vhost().is_empty() {
                server.host()
            } else {
                server.vhost()
            };
            if !valid_host(host) || !seen.insert(host.to_owned()) {
                continue;
            }
            servers.push(Server {
                host: host.to_owned(),
                allowed_apps: server.allowed_app_ids,
            });
        }
        if servers.is_empty() {
            return Err(failure("Steam returned no direct HTTPS content servers"));
        }
        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(DEADLINE)
            .user_agent("steam-tui/SteamPipe")
            .build()
            .map_err(|_| failure("could not initialize HTTPS content client"))?;
        Ok(Self {
            connection,
            http,
            servers,
            cache: Mutex::new(Cache::default()),
            branch: "public".into(),
        })
    }
    /// Retain the install plan's branch when obtaining manifest request codes.
    pub fn with_branch(mut self, branch: &str) -> Self {
        self.branch = branch.to_owned();
        self
    }

    async fn key(
        &self,
        spec: &DepotSpec,
        cancel: &CancellationToken,
    ) -> Result<Zeroizing<[u8; 32]>> {
        let mut cache = self.cache.lock().await;
        if let Some(key) = cache.keys.get(&(spec.depot_id, spec.source_app_id)) {
            return Ok(Zeroizing::new(**key));
        }
        let mut response: CMsgClientGetDepotDecryptionKeyResponse = bounded(cancel, async {
            self.connection
                .job(CMsgClientGetDepotDecryptionKey {
                    depot_id: Some(spec.depot_id),
                    app_id: Some(spec.source_app_id),
                    ..Default::default()
                })
                .await
                .map_err(|_| failure("Steam depot key request failed"))
        })
        .await?;
        if response.eresult() != 1 || response.depot_id() != spec.depot_id {
            if let Some(bytes) = response.depot_encryption_key.as_mut() {
                bytes.zeroize();
            }
            return Err(failure(format!(
                "Steam denied the key for depot {} (result {}); check depot ownership",
                spec.depot_id,
                response.eresult()
            )));
        }
        let bytes = Zeroizing::new(response.depot_encryption_key.take().unwrap_or_default());
        let mut key = Zeroizing::new([0_u8; 32]);
        if bytes.len() != key.len() {
            return Err(failure("Steam returned an invalid depot key length"));
        }
        key.copy_from_slice(&bytes);
        cache
            .keys
            .insert((spec.depot_id, spec.source_app_id), Zeroizing::new(*key));
        Ok(Zeroizing::new(*key))
    }
    async fn auth(
        &self,
        spec: &DepotSpec,
        host: &str,
        cancel: &CancellationToken,
    ) -> Result<Zeroizing<String>> {
        let mut cache = self.cache.lock().await;
        let identity = (spec.depot_id, spec.source_app_id, host.to_owned());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Some(auth) = cache.tokens.get(&identity)
            && u64::from(auth.expires) > now + 30
        {
            return Ok(Zeroizing::new(auth.token.to_string()));
        }
        let mut response = bounded(cancel, async {
            self.connection
                .service_method(CContentServerDirectory_GetCDNAuthToken_Request {
                    depot_id: Some(spec.depot_id),
                    app_id: Some(spec.source_app_id),
                    host_name: Some(host.into()),
                    ..Default::default()
                })
                .await
                .map_err(|_| failure("Steam CDN authorization request failed"))
        })
        .await?;
        let token = Zeroizing::new(response.token.take().unwrap_or_default());
        if token.is_empty() || token.len() > 8192 || token.contains(['\r', '\n', '#']) {
            return Err(failure("Steam returned an invalid CDN authorization token"));
        }
        cache.tokens.insert(
            identity,
            Auth {
                token: Zeroizing::new(token.to_string()),
                expires: response.expiration_time(),
            },
        );
        Ok(token)
    }
    async fn fetch(
        &self,
        spec: &DepotSpec,
        path: &str,
        limit: usize,
        exact: Option<usize>,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        validate_spec(spec)?;
        let mut last = "no content server allowed this app".to_owned();
        for server in self
            .servers
            .iter()
            .filter(|s| s.allowed_apps.is_empty() || s.allowed_apps.contains(&spec.source_app_id))
            .take(MAX_SERVERS)
        {
            let result = bounded(cancel, async {
                let base = format!("https://{}/{path}", server.host);
                let identity = (spec.depot_id, spec.source_app_id, server.host.clone());
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let cached = self
                    .cache
                    .lock()
                    .await
                    .tokens
                    .get(&identity)
                    .filter(|auth| u64::from(auth.expires) > now + 30)
                    .map(|auth| Zeroizing::new(auth.token.to_string()));
                let initial = cached.as_ref().map(|token| {
                    let separator = if token.starts_with('?') { "" } else { "?" };
                    Zeroizing::new(format!("{base}{separator}{}", token.as_str()))
                });
                let mut response = self
                    .http
                    .get(initial.as_ref().map_or(base.as_str(), |url| url.as_str()))
                    .send()
                    .await
                    .map_err(|_| failure("HTTPS content request failed"))?;
                if matches!(response.status().as_u16(), 401 | 403) {
                    self.cache.lock().await.tokens.remove(&identity);
                    let token = self.auth(spec, &server.host, cancel).await?;
                    // Steam tokens are already query-encoded; never log this URL.
                    let separator = if token.starts_with('?') { "" } else { "?" };
                    let url = Zeroizing::new(format!("{base}{separator}{}", token.as_str()));
                    response = self
                        .http
                        .get(url.as_str())
                        .send()
                        .await
                        .map_err(|_| failure("authorized HTTPS content request failed"))?;
                }
                if !response.status().is_success() {
                    return Err(failure(format!(
                        "Steam CDN returned HTTP {}",
                        response.status().as_u16()
                    )));
                }
                if let Some(len) = response.content_length()
                    && (len > limit as u64 || exact.is_some_and(|expected| len != expected as u64))
                {
                    return Err(failure(
                        "Steam CDN Content-Length is outside the manifest bounds",
                    ));
                }
                let mut bytes = Vec::with_capacity(exact.unwrap_or(0));
                while let Some(part) = response
                    .chunk()
                    .await
                    .map_err(|_| failure("HTTPS content body interrupted"))?
                {
                    if bytes
                        .len()
                        .checked_add(part.len())
                        .is_none_or(|n| n > limit)
                    {
                        return Err(failure("Steam CDN body exceeds the content size limit"));
                    }
                    bytes.extend_from_slice(&part);
                }
                if exact.is_some_and(|n| bytes.len() != n) {
                    return Err(failure("Steam CDN chunk length differs from its manifest"));
                }
                Ok(bytes)
            })
            .await;
            match result {
                Ok(bytes) => return Ok(bytes),
                Err(error) => {
                    if cancel.is_cancelled() {
                        return Err(cancelled());
                    }
                    last = error.to_string();
                }
            }
        }
        Err(failure(format!(
            "content download failed after bounded HTTPS server failover: {last}"
        )))
    }
    pub async fn manifest(
        &self,
        spec: &DepotSpec,
        cancel: &CancellationToken,
    ) -> Result<DepotManifest> {
        validate_spec(spec)?;
        let key = self.key(spec, cancel).await?;
        let code = bounded(cancel, async {
            self.connection
                .service_method(CContentServerDirectory_GetManifestRequestCode_Request {
                    app_id: Some(spec.source_app_id),
                    depot_id: Some(spec.depot_id),
                    manifest_id: Some(spec.manifest_id),
                    app_branch: (self.branch != "public").then(|| self.branch.clone()),
                    ..Default::default()
                })
                .await
                .map_err(|_| failure("Steam manifest authorization request failed"))
        })
        .await?
        .manifest_request_code();
        if code == 0 {
            return Err(failure(format!(
                "Steam denied manifest access for depot {}; its branch or license may be unavailable",
                spec.depot_id
            )));
        }
        let path = format!(
            "depot/{}/manifest/{}/5/{code}",
            spec.depot_id, spec.manifest_id
        );
        let bytes = self.fetch(spec, &path, MAX_MANIFEST, None, cancel).await?;
        let expected = spec.clone();
        let token = cancel.clone();
        let manifest = tokio::task::spawn_blocking(move || {
            if token.is_cancelled() {
                return Err(cancelled());
            }
            let result = formats::manifest(&bytes, &expected, &key);
            if token.is_cancelled() {
                return Err(cancelled());
            }
            result
        })
        .await
        .map_err(|_| failure("manifest decoding worker failed"))??;
        self.cache.lock().await.chunks.insert(
            (spec.depot_id, spec.manifest_id, spec.source_app_id),
            manifest
                .files
                .iter()
                .flat_map(|f| f.chunks.iter().cloned())
                .collect(),
        );
        Ok(manifest)
    }
    pub async fn chunk(
        &self,
        spec: &DepotSpec,
        chunk: &DepotChunk,
        cancel: &CancellationToken,
    ) -> Result<VerifiedChunk> {
        validate_spec(spec)?;
        formats::validate_chunk(chunk)?;
        if !self
            .cache
            .lock()
            .await
            .chunks
            .get(&(spec.depot_id, spec.manifest_id, spec.source_app_id))
            .is_some_and(|chunks| chunks.contains(chunk))
        {
            return Err(failure(
                "chunk does not belong to a validated manifest for this depot/GID",
            ));
        }
        let key = self.key(spec, cancel).await?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut id = String::with_capacity(40);
        for byte in chunk.id {
            id.push(HEX[(byte >> 4) as usize] as char);
            id.push(HEX[(byte & 15) as usize] as char);
        }
        let bytes = self
            .fetch(
                spec,
                &format!("depot/{}/chunk/{id}", spec.depot_id),
                chunk.compressed_size as usize,
                Some(chunk.compressed_size as usize),
                cancel,
            )
            .await?;
        let chunk = chunk.clone();
        let token = cancel.clone();
        tokio::task::spawn_blocking(move || {
            if token.is_cancelled() {
                return Err(cancelled());
            }
            let result =
                formats::chunk(&bytes, &chunk, &key).map(|bytes| VerifiedChunk { chunk, bytes });
            if token.is_cancelled() {
                return Err(cancelled());
            }
            result
        })
        .await
        .map_err(|_| failure("chunk decoding worker failed"))?
    }
}
fn validate_spec(spec: &DepotSpec) -> Result<()> {
    if spec.depot_id == 0 || spec.manifest_id == 0 || spec.source_app_id == 0 {
        return Err(failure(
            "depot, manifest and source app IDs must be nonzero",
        ));
    }
    Ok(())
}
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
/// Steam's checksum differs from RFC Adler32: its initial state is zero.
pub fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (0u32, 0u32);
    for block in bytes.chunks(5552) {
        for byte in block {
            a += u32::from(*byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}
/// Verify reused local bytes with the same checks as downloaded/decrypted bytes.
pub fn verify_chunk(chunk: &DepotChunk, bytes: &[u8]) -> Result<()> {
    use sha1::{Digest, Sha1};
    if bytes.len() != chunk.uncompressed_size as usize {
        return Err(failure("chunk uncompressed length mismatch"));
    }
    if adler32(bytes) != chunk.adler {
        return Err(failure("chunk Adler32 mismatch: content is corrupt"));
    }
    if Sha1::digest(bytes).as_slice() != chunk.id {
        return Err(failure("chunk SHA1 mismatch: content is corrupt"));
    }
    Ok(())
}
