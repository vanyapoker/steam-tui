//! Resolve the logged-in session's license packages through Steam PICS, without
//! an owned-games Web API key. Missing membership and missing app metadata remain
//! explicit; this is not an installability or compatibility determination.

mod disk;
mod metadata;
use crate::installed::Node;
pub(crate) use disk::cached_app;
use futures_util::{StreamExt, pin_mut, stream};
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::io;
use std::sync::LazyLock;
use std::time::Duration;
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver::CMsgClientLicenseList;
use steam_vent_proto_steam::steammessages_clientserver_appinfo::{
    CMsgClientPICSAccessTokenRequest, CMsgClientPICSAccessTokenResponse,
    CMsgClientPICSProductInfoRequest, CMsgClientPICSProductInfoResponse,
    cmsg_client_picsproduct_info_request,
};
use tokio::time::{Instant, timeout, timeout_at};

// Keep work finite even if a peer continually sends pending responses. Limits
// apply to decoded PICS data; the vendored transport decodes packets before here.
const BATCH_SIZE: usize = 100;
const MAX_IDS: usize = 100_000;
const MAX_LICENSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
const MAX_PARTS: usize = 128;
// Idle limit between multipart responses, and an absolute per-request cap so a
// peer trickling pending parts cannot hold a batch open forever. Concurrent
// batches share one TCP connection, so the cap is not a per-batch latency claim.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TOTAL_TIMEOUT: Duration = Duration::from_secs(180);
const LOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// PICS batches in flight on the one connection. Steam handles interleaved job
/// IDs; more than a handful mostly queues behind the CM's per-session limits.
/// `buffered` (not `buffer_unordered`) keeps results in request order, so output
/// and warnings do not depend on response timing.
pub(crate) const CONCURRENT_REQUESTS: usize = 4;

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

/// A snapshot of licensed package membership and the available app metadata.
#[derive(Debug)]
pub struct Library {
    pub package_count: usize,
    /// Sorted, deduplicated IDs derived only from successfully resolved packages.
    /// Unavailable app metadata has `None` fields and an entry in `unresolved_apps`.
    pub apps: Vec<LibraryApp>,
    /// Unknown, token-denied, omitted, or content-less packages. These can hide
    /// additional apps, so a nonempty list means membership is incomplete.
    pub unresolved_packages: Vec<u32>,
    /// Known member IDs with unknown, token-denied, omitted, or content-less PICS.
    pub unresolved_apps: Vec<u32>,
    pub owned_depots: BTreeSet<u32>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryApp {
    pub app_id: u32,
    pub name: Option<String>,
    pub app_type: Option<String>,
    pub(crate) document: Option<Node>,
}

#[derive(Default)]
struct SessionCache {
    licenses: Option<CMsgClientLicenseList>,
    packages: BTreeMap<u32, BTreeSet<u32>>,
    depots: BTreeMap<u32, BTreeSet<u32>>,
    apps: BTreeMap<u32, LibraryApp>,
}

type SessionKey = (u64, i32);
static CACHE: LazyLock<Mutex<BTreeMap<SessionKey, SessionCache>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn with_cache<T>(connection: &Connection, f: impl FnOnce(&mut SessionCache) -> T) -> T {
    let key = (connection.steam_id().steam64(), connection.session_id());
    let mut caches = CACHE.lock();
    // Bound metadata retention; never share ownership across account/session keys.
    if caches.len() >= 8
        && !caches.contains_key(&key)
        && let Some(old) = caches.keys().next().copied()
    {
        caches.remove(&old);
    }
    f(caches.entry(key).or_default())
}

pub(crate) fn cached_ownership(connection: &Connection) -> Option<(BTreeSet<u32>, BTreeSet<u32>)> {
    with_cache(connection, |cache| {
        let licenses = cache.licenses.as_ref()?;
        let packages: BTreeSet<_> = licenses
            .licenses
            .iter()
            .filter_map(|l| l.package_id)
            .collect();
        Some((
            cache
                .packages
                .iter()
                .filter(|(id, _)| packages.contains(id))
                .flat_map(|(_, apps)| apps.iter().copied())
                .collect(),
            cache
                .depots
                .iter()
                .filter(|(id, _)| packages.contains(id))
                .flat_map(|(_, depots)| depots.iter().copied())
                .collect(),
        ))
    })
}

/// Resolve session license membership and refresh PICS. Repeated calls retain
/// the initial license push and last known membership, while fetching fresh app
/// documents. Buffered license changes replace the grants on the next load.
pub async fn load(connection: &Connection) -> Result<Library> {
    let result = timeout(LOAD_TIMEOUT, load_inner(connection))
        .await
        .map_err(|_| failure("library loading exceeded its 10 minute deadline"))
        .and_then(|result| result);
    match result {
        Ok(library) => Ok(library),
        Err(error) => {
            let Some((ids, owned_depots)) =
                cached_ownership(connection).filter(|(ids, _)| !ids.is_empty())
            else {
                return Err(error);
            };
            let (apps, packages) = with_cache(connection, |cache| {
                (
                    ids.iter()
                        .map(|id| {
                            cache.apps.get(id).cloned().unwrap_or(LibraryApp {
                                app_id: *id,
                                name: None,
                                app_type: None,
                                document: None,
                            })
                        })
                        .collect(),
                    cache
                        .licenses
                        .as_ref()
                        .map(|l| {
                            l.licenses
                                .iter()
                                .filter_map(|l| l.package_id)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                )
            });
            Ok(Library {
                package_count: packages.len(),
                apps,
                owned_depots,
                unresolved_packages: packages,
                unresolved_apps: ids.into_iter().collect(),
                warnings: vec![format!(
                    "Steam catalog refresh failed; known licensed membership retained: {error}"
                )],
            })
        }
    }
}

async fn load_inner(connection: &Connection) -> Result<Library> {
    let licenses = license_list(connection).await?;
    let mut package_tokens: BTreeMap<u32, u64> = BTreeMap::new();
    for license in licenses.licenses {
        let id = license
            .package_id
            .ok_or_else(|| failure("license entry has no package ID"))?;
        let token = license.access_token.unwrap_or(0);
        // Duplicate grants can occur; do not let a token-less grant erase a token.
        package_tokens
            .entry(id)
            .and_modify(|old| *old = (*old).max(token))
            .or_insert(token);
    }
    let package_count = package_tokens.len();
    let mut warnings = Vec::new();
    let missing_tokens: Vec<_> = package_tokens
        .iter()
        .filter_map(|(&id, &token)| (token == 0).then_some(id))
        .collect();
    let (extra_tokens, mut unresolved_packages) =
        match tokens(connection, &missing_tokens, Kind::Package).await {
            Ok(result) => result,
            Err(error) => {
                warnings.push(format!("Package access tokens unavailable: {error}"));
                (BTreeMap::new(), BTreeSet::new())
            }
        };
    package_tokens.extend(extra_tokens);
    let package_ids: Vec<_> = package_tokens
        .keys()
        .copied()
        .filter(|id| !unresolved_packages.contains(id))
        .collect();
    let mut app_ids = BTreeSet::new();
    {
        let chunks = stream::iter(
            package_ids
                .chunks(BATCH_SIZE)
                .map(|ids| package_chunk(connection, ids, &package_tokens)),
        )
        .buffered(CONCURRENT_REQUESTS);
        pin_mut!(chunks);
        while let Some(chunk) = chunks.next().await {
            if let Some(error) = chunk.error {
                warnings.push(format!("Package metadata unavailable: {error}"));
            }
            unresolved_packages.extend(chunk.expected.difference(&chunk.resolved).copied());
            app_ids.extend(chunk.apps);
            if app_ids.len() > MAX_IDS {
                return Err(failure("library exceeds the 100000 app ID limit"));
            }
        }
    }
    with_cache(connection, |cache| {
        for (&id, members) in &cache.packages {
            if package_tokens.contains_key(&id) {
                app_ids.extend(members.iter().copied());
            }
        }
    });
    let app_ids: Vec<_> = app_ids.into_iter().collect();
    if package_count > 0 && app_ids.is_empty() && !unresolved_packages.is_empty() {
        return Err(failure(
            "No licensed package membership could be resolved; the library is unavailable, not empty",
        ));
    }
    let (mut apps, unresolved_apps) = match fetch_apps(connection, &app_ids).await {
        Ok(result) => result,
        Err(error) => {
            warnings.push(format!("App metadata unavailable: {error}"));
            let apps = with_cache(connection, |cache| {
                app_ids
                    .iter()
                    .map(|id| {
                        cache.apps.get(id).cloned().unwrap_or(LibraryApp {
                            app_id: *id,
                            name: None,
                            app_type: None,
                            document: None,
                        })
                    })
                    .collect()
            });
            (apps, app_ids.iter().copied().collect())
        }
    };
    with_cache(connection, |cache| {
        for app in &mut apps {
            if unresolved_apps.contains(&app.app_id)
                && let Some(old) = cache.apps.get(&app.app_id)
            {
                app.name = app.name.take().or_else(|| old.name.clone());
                app.app_type = app.app_type.take().or_else(|| old.app_type.clone());
            }
        }
    });
    with_cache(connection, |cache| {
        for app in &apps {
            if app.document.is_some() {
                cache.apps.insert(
                    app.app_id,
                    LibraryApp {
                        app_id: app.app_id,
                        name: app.name.clone(),
                        app_type: app.app_type.clone(),
                        document: None,
                    },
                );
            }
        }
    });
    let owned_depots = with_cache(connection, |cache| {
        cache
            .depots
            .iter()
            .filter(|(id, _)| package_tokens.contains_key(*id))
            .flat_map(|(_, depots)| depots.iter().copied())
            .collect()
    });
    Ok(Library {
        package_count,
        apps,
        unresolved_packages: unresolved_packages.into_iter().collect(),
        unresolved_apps: unresolved_apps.into_iter().collect(),
        warnings,
        owned_depots,
    })
}

/// Resolve one app through the same PICS token, multipart, bounds, and text parser
/// path as `load`. Useful on an anonymous connection without account credentials.
/// `None` means Steam did not provide usable metadata (including token denial).
/// This says nothing about ownership or Linux compatibility.
pub async fn inspect_app(connection: &Connection, app_id: u32) -> Result<Option<LibraryApp>> {
    if app_id == 0 {
        return Err(failure("app ID must be nonzero"));
    }
    let (mut apps, unresolved) = timeout(LOAD_TIMEOUT, fetch_apps(connection, &[app_id]))
        .await
        .map_err(|_| failure("app inspection deadline exceeded"))??;
    if unresolved.contains(&app_id) {
        Ok(None)
    } else {
        Ok(apps.pop())
    }
}

/// Resolve several apps with one deduplicated, batched, concurrent PICS pass.
/// Returns the resolved apps by ID; every other requested ID is in the unresolved
/// set (unknown, token-denied, omitted, or content-less). Output is independent
/// of response timing.
pub(crate) async fn inspect_apps(
    connection: &Connection,
    ids: &BTreeSet<u32>,
) -> Result<(BTreeMap<u32, LibraryApp>, BTreeSet<u32>)> {
    if ids.contains(&0) {
        return Err(failure("app ID must be nonzero"));
    }
    let ids: Vec<_> = ids.iter().copied().collect();
    let (apps, unresolved) = timeout(LOAD_TIMEOUT, fetch_apps(connection, &ids))
        .await
        .map_err(|_| failure("app inspection deadline exceeded"))??;
    Ok((
        apps.into_iter()
            .filter(|app| !unresolved.contains(&app.app_id))
            .map(|app| (app.app_id, app))
            .collect(),
        unresolved,
    ))
}

/// One package PICS batch. Failure keeps what was decoded before it; the caller
/// reports every expected package that did not resolve as unresolved.
struct PackageChunk {
    expected: BTreeSet<u32>,
    resolved: BTreeSet<u32>,
    apps: BTreeSet<u32>,
    error: Option<String>,
}

async fn package_chunk(
    connection: &Connection,
    ids: &[u32],
    package_tokens: &BTreeMap<u32, u64>,
) -> PackageChunk {
    let expected: BTreeSet<_> = ids.iter().copied().collect();
    let mut seen = BTreeSet::new();
    let mut resolved = BTreeSet::new();
    let mut apps_found = BTreeSet::new();
    let request = CMsgClientPICSProductInfoRequest {
        packages: ids
            .iter()
            .map(|id| cmsg_client_picsproduct_info_request::PackageInfo {
                packageid: Some(*id),
                access_token: Some(package_tokens[id]),
                ..Default::default()
            })
            .collect(),
        meta_data_only: Some(false),
        single_response: Some(false),
        ..Default::default()
    };
    let result = product_info(connection, request, |response| {
        if !response.apps.is_empty() || !response.unknown_appids.is_empty() {
            return Err(failure("unexpected app data in package PICS response"));
        }
        for id in response.unknown_packageids {
            record_id(&expected, &mut seen, id)?;
        }
        for package in response.packages {
            let id = package
                .packageid
                .ok_or_else(|| failure("PICS package entry has no ID"))?;
            record_id(&expected, &mut seen, id)?;
            if package.missing_token() || response.meta_data_only.unwrap_or(false) {
                continue;
            }
            let Some(buffer) = package.buffer.filter(|buffer| !buffer.is_empty()) else {
                continue;
            };
            let apps = metadata::package_apps(id, &buffer)
                .map_err(|error| failure(format!("package {id}: {error}")))?;
            let depots = metadata::package_depots(id, &buffer)
                .map_err(|error| failure(format!("package {id}: {error}")))?
                .unwrap_or_default();
            with_cache(connection, |cache| {
                cache.depots.insert(id, depots);
            });
            if let Some(apps) = apps {
                with_cache(connection, |cache| {
                    cache.packages.insert(id, apps.clone());
                });
                apps_found.extend(apps);
                if apps_found.len() > MAX_IDS {
                    return Err(failure("library exceeds the 100000 app ID limit"));
                }
                resolved.insert(id);
            }
        }
        Ok(())
    })
    .await;
    PackageChunk {
        expected,
        resolved,
        apps: apps_found,
        error: result.err().map(|error| error.to_string()),
    }
}

async fn license_list(connection: &Connection) -> Result<CMsgClientLicenseList> {
    // on() registers synchronously. Register BEFORE draining, closing the race
    // where the initial push arrives between taking the buffer and subscribing.
    let stream = connection.on::<CMsgClientLicenseList>();
    pin_mut!(stream);
    let mut buffered = None;
    for raw in connection.take_unprocessed_kind::<CMsgClientLicenseList>() {
        if raw.data.len() > MAX_LICENSE_BYTES {
            return Err(failure("license list exceeds the 64 MiB limit"));
        }
        buffered = Some(raw.into_message::<CMsgClientLicenseList>()?);
    }
    if buffered.is_none() {
        buffered = with_cache(connection, |cache| cache.licenses.clone());
    }
    let mut licenses = match buffered {
        Some(licenses) => licenses,
        None => timeout(REQUEST_TIMEOUT, stream.next()).await
            .map_err(|_| failure("initial license list not received within 30 seconds (it may have been evicted from steam-vent's 32-message buffer); reconnect and load promptly"))?
            .ok_or_else(|| failure("license list stream ended before the initial list"))??,
    };
    if licenses.eresult() != 1 {
        return Err(failure(format!(
            "Steam license list failed with EResult {}",
            licenses.eresult()
        )));
    }
    if licenses.licenses.len() > MAX_IDS {
        return Err(failure("license list exceeds the 100000 entry limit"));
    }
    licenses
        .licenses
        .retain(|license| active_license(license.flags.unwrap_or(0)));
    with_cache(connection, |cache| {
        cache.licenses = Some(licenses.clone());
    });
    Ok(licenses)
}

// ELicenseFlags: pending/expired/cancelled/not-activated grants must not turn
// into owned DLC or depot access. Borrowed and nonpermanent active grants are
// real current licenses and remain eligible; the CM still authorizes content.
fn active_license(flags: u32) -> bool {
    flags & (0x04 | 0x08 | 0x10 | 0x20 | 0x400 | 0x800 | 0x40000) == 0
}

#[derive(Clone, Copy)]
enum Kind {
    Package,
    App,
}

async fn tokens(
    connection: &Connection,
    ids: &[u32],
    kind: Kind,
) -> Result<(BTreeMap<u32, u64>, BTreeSet<u32>)> {
    let mut tokens = BTreeMap::new();
    let mut unresolved = BTreeSet::new();
    let chunks = stream::iter(
        ids.chunks(BATCH_SIZE)
            .map(|ids| token_chunk(connection, ids, kind)),
    )
    .buffered(CONCURRENT_REQUESTS);
    pin_mut!(chunks);
    while let Some(chunk) = chunks.next().await {
        let (found, missing) = chunk?;
        tokens.extend(found);
        unresolved.extend(missing);
    }
    Ok((tokens, unresolved))
}

async fn token_chunk(
    connection: &Connection,
    ids: &[u32],
    kind: Kind,
) -> Result<(BTreeMap<u32, u64>, BTreeSet<u32>)> {
    let mut tokens = BTreeMap::new();
    let expected = ids.iter().copied().collect();
    let request = match kind {
        Kind::Package => CMsgClientPICSAccessTokenRequest {
            packageids: ids.to_vec(),
            ..Default::default()
        },
        Kind::App => CMsgClientPICSAccessTokenRequest {
            appids: ids.to_vec(),
            ..Default::default()
        },
    };
    let response: CMsgClientPICSAccessTokenResponse =
        timeout(REQUEST_TIMEOUT, connection.job(request))
            .await
            .map_err(|_| failure("PICS access token request exceeded its 30 second deadline"))??;
    let mut seen = BTreeSet::new();
    let mut found = BTreeSet::new();
    match kind {
        Kind::Package => {
            if !response.app_access_tokens.is_empty()
                || !response.app_denied_tokens.is_empty()
                || response.package_access_tokens.len() + response.package_denied_tokens.len()
                    > BATCH_SIZE
            {
                return Err(failure("invalid package access token response"));
            }
            for token in response.package_access_tokens {
                let id = token
                    .packageid
                    .ok_or_else(|| failure("package access token has no ID"))?;
                record_id(&expected, &mut seen, id)?;
                tokens.insert(id, token.access_token());
                found.insert(id);
            }
            for id in response.package_denied_tokens {
                record_id(&expected, &mut seen, id)?;
            }
        }
        Kind::App => {
            if !response.package_access_tokens.is_empty()
                || !response.package_denied_tokens.is_empty()
                || response.app_access_tokens.len() + response.app_denied_tokens.len() > BATCH_SIZE
            {
                return Err(failure("invalid app access token response"));
            }
            for token in response.app_access_tokens {
                let id = token
                    .appid
                    .ok_or_else(|| failure("app access token has no ID"))?;
                record_id(&expected, &mut seen, id)?;
                tokens.insert(id, token.access_token());
                found.insert(id);
            }
            for id in response.app_denied_tokens {
                record_id(&expected, &mut seen, id)?;
            }
        }
    }
    Ok((tokens, expected.difference(&found).copied().collect()))
}

/// Keeps only what `resolve` reads, so the documents held for a whole library load
/// stay small: the full PICS trees of ~1,400 apps peaked near 100 MB. The complete
/// documents are persisted for launch before `fetch_chunk` prunes them.
pub(crate) fn prune(document: &mut Node) {
    const KEPT: &[(&str, &[&str])] = &[
        ("appid", &[]),
        ("common", &["name", "oslist", "type", "parent"]),
        (
            "extended",
            &["listofdlc", "dlcavailableonstore", "dlcforappid"],
        ),
        ("config", &["installdir"]),
        ("depots", &[]),
    ];
    document
        .children
        .retain(|key, _| KEPT.iter().any(|(kept, _)| kept.eq_ignore_ascii_case(key)));
    // Node lookups fall back to a case-insensitive match, so pruning must too.
    for (key, child) in &mut document.children {
        let Some((_, child_keys)) = KEPT.iter().find(|(kept, _)| kept.eq_ignore_ascii_case(key))
        else {
            continue;
        };
        if !child_keys.is_empty() {
            child.children.retain(|name, _| {
                child_keys
                    .iter()
                    .any(|kept| kept.eq_ignore_ascii_case(name))
            });
        }
    }
}

/// Fetch app documents in `BATCH_SIZE` chunks, up to `CONCURRENT_REQUESTS` chunks
/// in flight. Each chunk does its own token request, product-info request, and
/// disk persistence, so no chunk waits for a global barrier. A chunk whose
/// product-info request fails leaves only its own apps unresolved; a token-request
/// failure aborts the call. Results are merged in chunk order.
async fn fetch_apps(
    connection: &Connection,
    ids: &[u32],
) -> Result<(Vec<LibraryApp>, BTreeSet<u32>)> {
    let chunks = stream::iter(
        ids.chunks(BATCH_SIZE)
            .map(|ids| fetch_chunk(connection, ids)),
    )
    .buffered(CONCURRENT_REQUESTS);
    pin_mut!(chunks);
    let mut results = Vec::new();
    while let Some(chunk) = chunks.next().await {
        results.push(chunk?);
    }
    Ok(merge_chunks(ids, results))
}

/// Combine chunk results into the requested-ID order. Independent of the order in
/// which chunks completed or are listed: apps are keyed by ID and every requested
/// ID without a resolved document yields a placeholder row.
fn merge_chunks(
    ids: &[u32],
    chunks: impl IntoIterator<Item = (BTreeMap<u32, LibraryApp>, BTreeSet<u32>)>,
) -> (Vec<LibraryApp>, BTreeSet<u32>) {
    let mut apps = BTreeMap::new();
    let mut unresolved = BTreeSet::new();
    for (resolved, missing) in chunks {
        apps.extend(resolved);
        unresolved.extend(missing);
    }
    let apps = ids
        .iter()
        .map(|id| {
            apps.remove(id).unwrap_or(LibraryApp {
                app_id: *id,
                name: None,
                app_type: None,
                document: None,
            })
        })
        .collect();
    (apps, unresolved)
}

async fn fetch_chunk(
    connection: &Connection,
    ids: &[u32],
) -> Result<(BTreeMap<u32, LibraryApp>, BTreeSet<u32>)> {
    let (tokens, mut unresolved) = token_chunk(connection, ids, Kind::App).await?;
    let available: Vec<_> = ids
        .iter()
        .copied()
        .filter(|id| !unresolved.contains(id))
        .collect();
    if available.is_empty() {
        return Ok((BTreeMap::new(), unresolved));
    }
    let mut batch = AppBatch::new(&available);
    let request = CMsgClientPICSProductInfoRequest {
        apps: available
            .iter()
            .map(|id| cmsg_client_picsproduct_info_request::AppInfo {
                appid: Some(*id),
                access_token: Some(tokens[id]),
                only_public_obsolete: Some(false),
                ..Default::default()
            })
            .collect(),
        meta_data_only: Some(false),
        single_response: Some(false),
        ..Default::default()
    };
    if product_info(connection, request, |response| batch.consume(response))
        .await
        .is_err()
    {
        unresolved.extend(available);
        return Ok((BTreeMap::new(), unresolved));
    }
    let (resolved, missing) = batch.finish();
    let resolved = tokio::task::spawn_blocking(move || -> crate::installed::Result<_> {
        let mut resolved = resolved;
        disk::store(
            resolved
                .values()
                .filter_map(|app| app.document.as_ref().map(|document| (app.app_id, document))),
        )?;
        for document in resolved
            .values_mut()
            .filter_map(|app| app.document.as_mut())
        {
            prune(document);
        }
        Ok(resolved)
    })
    .await?
    .map_err(|error| failure(format!("Could not persist launch metadata: {error}")))?;
    unresolved.extend(missing);
    Ok((resolved, unresolved))
}

/// The production multipart accumulator, kept separate from I/O so the unknown,
/// omitted, and token-restricted cases can be exercised without credentials.
struct AppBatch {
    expected: BTreeSet<u32>,
    seen: BTreeSet<u32>,
    apps: BTreeMap<u32, LibraryApp>,
}

impl AppBatch {
    fn new(ids: &[u32]) -> Self {
        Self {
            expected: ids.iter().copied().collect(),
            seen: BTreeSet::new(),
            apps: BTreeMap::new(),
        }
    }

    fn consume(&mut self, response: CMsgClientPICSProductInfoResponse) -> Result<()> {
        if !response.packages.is_empty() || !response.unknown_packageids.is_empty() {
            return Err(failure("unexpected package data in app PICS response"));
        }
        for id in response.unknown_appids {
            record_id(&self.expected, &mut self.seen, id)?;
        }
        for app in response.apps {
            let id = app
                .appid
                .ok_or_else(|| failure("PICS app entry has no ID"))?;
            record_id(&self.expected, &mut self.seen, id)?;
            if app.missing_token() || response.meta_data_only.unwrap_or(false) {
                continue;
            }
            let Some(buffer) = app.buffer.filter(|buffer| !buffer.is_empty()) else {
                continue;
            };
            let app = metadata::app(id, &buffer)
                .map_err(|error| failure(format!("app {id}: {error}")))?;
            self.apps.insert(id, app);
        }
        Ok(())
    }

    fn finish(self) -> (BTreeMap<u32, LibraryApp>, BTreeSet<u32>) {
        let unresolved = self
            .expected
            .into_iter()
            .filter(|id| !self.apps.contains_key(id))
            .collect();
        (self.apps, unresolved)
    }
}

fn record_id(expected: &BTreeSet<u32>, seen: &mut BTreeSet<u32>, id: u32) -> Result<()> {
    if !expected.contains(&id) {
        return Err(failure(format!("PICS returned an unrequested ID {id}")));
    }
    if !seen.insert(id) {
        return Err(failure(format!(
            "PICS returned duplicate/conflicting data for ID {id}"
        )));
    }
    Ok(())
}

async fn product_info(
    connection: &Connection,
    request: CMsgClientPICSProductInfoRequest,
    mut consume: impl FnMut(CMsgClientPICSProductInfoResponse) -> Result<()>,
) -> Result<()> {
    let stream = connection.job_multi::<_, CMsgClientPICSProductInfoResponse>(request);
    pin_mut!(stream);
    let overall = Instant::now() + REQUEST_TOTAL_TIMEOUT;
    let mut parts = 0;
    let mut bytes = 0;
    loop {
        // Each part must arrive within the idle limit (parts of concurrent
        // batches interleave on one connection), and the request within the cap.
        let deadline = (Instant::now() + REQUEST_TIMEOUT).min(overall);
        let response = timeout_at(deadline, stream.next()).await
            .map_err(|_| failure("multipart PICS request stalled past its 30 second idle or 3 minute total deadline; library snapshot is incomplete"))?
            .ok_or_else(|| failure("multipart PICS stream ended before a final response"))??;
        parts += 1;
        if parts > MAX_PARTS {
            return Err(failure("PICS response exceeds the 128-part limit"));
        }
        if response.apps.len()
            + response.packages.len()
            + response.unknown_appids.len()
            + response.unknown_packageids.len()
            > BATCH_SIZE
        {
            return Err(failure("PICS response exceeds its requested batch size"));
        }
        for (buffer, sha) in response
            .apps
            .iter()
            .map(|app| (&app.buffer, &app.sha))
            .chain(
                response
                    .packages
                    .iter()
                    .map(|package| (&package.buffer, &package.sha)),
            )
        {
            let size = buffer.as_ref().map_or(0, Vec::len);
            if size > metadata::MAX_PRODUCT_BYTES || sha.as_ref().map_or(0, Vec::len) > 64 {
                return Err(failure(
                    "PICS product buffer or hash exceeds its size limit",
                ));
            }
            bytes += size;
            if bytes > MAX_BATCH_BYTES {
                return Err(failure("PICS batch exceeds the 64 MiB metadata limit"));
            }
        }
        let pending = response.response_pending();
        consume(response)?;
        if !pending {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use steam_vent_proto_steam::steammessages_clientserver_appinfo::cmsg_client_picsproduct_info_response::AppInfo;

    fn app_info(id: u32) -> AppInfo {
        AppInfo {
            appid: Some(id),
            buffer: Some(format!("\"appinfo\"\n{{\n\"appid\" \"{id}\"\n\"common\"\n{{\n\"name\" \"App {id}\"\n\"type\" \"Game\"\n}}\n}}\0").into_bytes()),
            ..Default::default()
        }
    }

    #[test]
    fn multipart_apps_preserve_unknown_omitted_and_missing_token_ids() {
        let mut batch = AppBatch::new(&[20, 10, 30, 40, 50, 60]);
        batch
            .consume(CMsgClientPICSProductInfoResponse {
                apps: vec![app_info(20)],
                response_pending: Some(true),
                ..Default::default()
            })
            .unwrap();
        let mut restricted = app_info(40);
        restricted.missing_token = Some(true);
        batch
            .consume(CMsgClientPICSProductInfoResponse {
                apps: vec![
                    app_info(10),
                    restricted,
                    AppInfo {
                        appid: Some(60),
                        ..Default::default()
                    },
                ],
                unknown_appids: vec![30],
                response_pending: Some(false),
                ..Default::default()
            })
            .unwrap();
        let (apps, unresolved) = batch.finish();
        assert_eq!(apps.keys().copied().collect::<Vec<_>>(), [10, 20]);
        assert_eq!(apps[&10].name.as_deref(), Some("App 10"));
        assert_eq!(apps[&20].app_type.as_deref(), Some("Game"));
        assert_eq!(unresolved, BTreeSet::from([30, 40, 50, 60]));
    }

    #[test]
    fn metadata_only_response_does_not_claim_app_resolution() {
        let mut batch = AppBatch::new(&[10]);
        batch
            .consume(CMsgClientPICSProductInfoResponse {
                apps: vec![app_info(10)],
                meta_data_only: Some(true),
                ..Default::default()
            })
            .unwrap();
        let (apps, unresolved) = batch.finish();
        assert!(apps.is_empty());
        assert_eq!(unresolved, BTreeSet::from([10]));
    }

    #[test]
    fn unsolicited_and_conflicting_app_responses_are_errors() {
        let mut batch = AppBatch::new(&[10]);
        assert!(
            batch
                .consume(CMsgClientPICSProductInfoResponse {
                    apps: vec![app_info(20)],
                    ..Default::default()
                })
                .is_err()
        );
        let mut batch = AppBatch::new(&[10]);
        batch
            .consume(CMsgClientPICSProductInfoResponse {
                unknown_appids: vec![10],
                response_pending: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert!(
            batch
                .consume(CMsgClientPICSProductInfoResponse {
                    apps: vec![app_info(10)],
                    ..Default::default()
                })
                .is_err()
        );
    }

    fn chunk(ids: &[u32], missing: &[u32]) -> (BTreeMap<u32, LibraryApp>, BTreeSet<u32>) {
        (
            ids.iter()
                .map(|id| {
                    (
                        *id,
                        LibraryApp {
                            app_id: *id,
                            name: Some(format!("App {id}")),
                            app_type: Some("game".into()),
                            document: Some(Node::default()),
                        },
                    )
                })
                .collect(),
            missing.iter().copied().collect(),
        )
    }

    #[test]
    fn merged_chunks_do_not_depend_on_completion_order() {
        let ids = [5, 1, 9, 3, 7];
        let chunks = [chunk(&[5, 1], &[]), chunk(&[9], &[3]), chunk(&[], &[7])];
        let expected = merge_chunks(&ids, chunks.clone());
        assert_eq!(expected.0.iter().map(|a| a.app_id).collect::<Vec<_>>(), ids);
        assert_eq!(expected.1, BTreeSet::from([3, 7]));
        assert!(expected.0[3].document.is_none());
        for order in [[2, 1, 0], [1, 0, 2], [0, 2, 1], [2, 0, 1], [1, 2, 0]] {
            let permuted = order.map(|i| chunks[i].clone());
            assert_eq!(merge_chunks(&ids, permuted), expected);
        }
    }

    #[test]
    fn buffered_requests_yield_in_request_order_when_completed_out_of_order() {
        async fn finish_after(id: u32, yields: u32) -> u32 {
            for _ in 0..yields {
                tokio::task::yield_now().await;
            }
            id
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let order = runtime.block_on(async {
            // Request 0 finishes last, request 3 first: completion order is reversed.
            stream::iter(
                [(0, 30), (1, 20), (2, 10), (3, 0)].map(|(id, yields)| finish_after(id, yields)),
            )
            .buffered(CONCURRENT_REQUESTS)
            .collect::<Vec<_>>()
            .await
        });
        assert_eq!(order, [0, 1, 2, 3]);
    }

    #[test]
    fn cancelled_expired_and_pending_grants_are_not_ownership() {
        for flags in [0x04, 0x08, 0x10, 0x20, 0x400, 0x800, 0x40000] {
            assert!(!active_license(flags));
            assert!(!active_license(flags | 0x4000));
        }
        assert!(active_license(0));
        assert!(active_license(0x4000)); // active family borrowing
        assert!(active_license(0x80000)); // active nonpermanent/free-weekend grant
    }

    fn node(children: &[(&str, Node)]) -> Node {
        Node {
            value: None,
            children: children
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        }
    }
    fn leaf(value: &str) -> Node {
        Node {
            value: Some(value.to_owned()),
            children: BTreeMap::new(),
        }
    }

    #[test]
    fn prune_keeps_only_what_resolve_reads() {
        let mut document = node(&[
            ("appid", leaf("440")),
            (
                "Common",
                node(&[
                    ("name", leaf("TF2")),
                    ("OSList", leaf("windows,linux")),
                    ("store_tags", leaf("x")),
                ]),
            ),
            (
                "config",
                node(&[
                    ("installdir", leaf("tf")),
                    ("launch", node(&[("0", leaf("x"))])),
                ]),
            ),
            (
                "depots",
                node(&[("441", node(&[("manifests", leaf("m"))]))]),
            ),
            ("extended", node(&[("developer", leaf("Valve"))])),
            ("ufs", node(&[("quota", leaf("1"))])),
        ]);
        prune(&mut document);
        let keys: Vec<_> = document.children.keys().map(String::as_str).collect();
        assert_eq!(keys, ["Common", "appid", "config", "depots", "extended"]);
        assert_eq!(document.at(&["common"]).unwrap().children.len(), 2);
        assert_eq!(
            document.at(&["common"]).unwrap().text("oslist"),
            Some("windows,linux")
        );
        assert_eq!(
            document.at(&["config"]).unwrap().text("installdir"),
            Some("tf")
        );
        assert!(document.at(&["config", "launch"]).is_none());
        assert!(document.at(&["depots", "441", "manifests"]).is_some());
    }
}
