//! First-run setup uses verified depot scripts and the game's own Proton prefix.
//! No host shell, system registry, Steam service, or elevation is involved.
mod runner;
mod script;
#[cfg(test)]
mod tests;

use super::*;
use crate::catalog::{DepotSpec, InstallPlatform, shared};
use crate::content::ContentClient;
use crate::install::filesystem as safe;
use crate::installed::read_bounded;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::io::Read;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Receipt {
    path: String,
    sha1: String,
    depot: u32,
    manifest: u64,
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) async fn prepare(
    game: &InstalledGame,
    plan: &LaunchPlan,
    connection: Option<&steam_vent::Connection>,
    events: Option<&crate::backend::EventSender>,
) -> Result<()> {
    let proton = plan.environment.contains_key("STEAM_COMPAT_DATA_PATH");
    let platform = if proton {
        InstallPlatform::Windows
    } else {
        InstallPlatform::Linux
    };
    let app = app_document(game)?;
    let mut requirements = shared::requirements(&app, platform, &game.language)?;
    let (owned, licensed) = connection
        .and_then(crate::library::cached_ownership)
        .unwrap_or_default();
    for ids in requirements.values_mut() {
        ids.retain(|id| {
            app.at(&["depots", &id.to_string()])
                .and_then(|d| d.text("dlcappid"))
                .and_then(|s| s.parse::<u32>().ok())
                .is_none_or(|dlc| owned.contains(&dlc) || licensed.contains(id))
        });
    }
    requirements.retain(|_, ids| !ids.is_empty());
    // The installed plan records the exact shared content needed by that build,
    // even if a later PICS update changed its dependency declarations.
    let acf = read_vdf(
        &game
            .library_root
            .join(format!("steamapps/appmanifest_{}.acf", game.app_id)),
    )?;
    if let Some(shared) = acf.at(&["appstate", "shareddepots"]) {
        for (id, source) in &shared.children {
            let id = id
                .parse::<u32>()
                .ok()
                .filter(|id| *id != 0)
                .ok_or_else(|| failure("Invalid recorded shared depot"))?;
            let source = source
                .value
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|id| *id != 0)
                .ok_or_else(|| failure("Invalid recorded shared source app"))?;
            requirements.entry(source).or_default().insert(id);
        }
    }
    if requirements.len() > 16 || requirements.values().map(BTreeSet::len).sum::<usize>() > 128 {
        return Err(failure("Recorded shared prerequisite graph is too large"));
    }
    let mut sources = vec![(game.clone(), None)];
    for (source, requested) in requirements {
        if source == game.app_id {
            return Err(failure("Shared prerequisite references its own game"));
        }
        let installed = shared::installed(game, source)?;
        let current = crate::library::cached_app(source)?;
        let ready = installed.as_ref().is_some_and(|installed| {
            requested.iter().all(|id| {
                let Some(manifest) = installed.installed_depots.get(id) else {
                    return false;
                };
                current
                    .as_ref()
                    .and_then(|doc| doc.at(&["depots", &id.to_string(), "manifests", "public"]))
                    .and_then(|n| n.text("gid"))
                    .and_then(|s| s.parse::<u64>().ok())
                    .is_none_or(|latest| latest == *manifest)
            })
        });
        if !ready {
            return Err(Box::new(Dependency::Shared {
                app_id: source,
                depots: requested,
                platform,
            }));
        }
        sources.push((installed.expect("checked above"), Some(requested)));
    }
    // Shared payloads also apply to native apps; Windows script execution does
    // not. Never run a Linux host shell for a Windows first-run directive.
    if !proton {
        return Ok(());
    }
    let library = game.library_root.canonicalize()?;
    let state = library.join("steamapps/.steam-tui");
    safe::checked(&library, Path::new("steamapps/.steam-tui"), true)?;
    safe::private_directory(&state)?;
    let _library_lock = safe::Lock::acquire(&state.join("library.lock"))?;
    let directory = state.join("setup");
    safe::private_directory(&directory)?;
    let mut recipes = Vec::new();
    // Shared prerequisites precede game-specific setup.
    sources.rotate_left(1);
    for (source, selected) in sources {
        let scripts = discover(&source, connection, &directory).await?;
        for receipt in scripts {
            if selected
                .as_ref()
                .is_some_and(|ids| !ids.contains(&receipt.depot))
            {
                continue;
            }
            recipes.push(load_recipe(&source, &receipt, &game.language)?);
        }
    }
    if recipes
        .iter()
        .all(|r| r.registry.is_empty() && r.processes.is_empty())
    {
        return Ok(());
    }
    if let Some(events) = events {
        let _ = events.send(crate::backend::Event {
            at: SystemTime::now(),
            app_id: game.app_id,
            direction: crate::backend::Direction::Local,
            channel: "Setup",
            title: "Preparing prerequisites".into(),
            detail: "Running verified first-run scripts in this game's Proton prefix".into(),
        });
    }
    let owned = game.clone();
    let plan = plan.clone();
    let cancel = CancellationToken::new();
    // Aborting the async launch must also stop its blocking setup worker and
    // tagged child processes. A dropped JoinHandle alone would leave them alive.
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let library_lock = _library_lock;
    tokio::task::spawn_blocking(move || {
        // Keep payloads protected until cancellation has reaped the worker's
        // children; dropping the async launch must not unlock them early.
        let _library_lock = library_lock;
        runner::execute(&owned, &plan, &recipes, &directory, cancel)
    })
    .await
    .map_err(|e| failure(format!("First-run worker failed: {e}")))??;
    Ok(())
}

fn load_recipe(
    source: &InstalledGame,
    receipt: &Receipt,
    language: &str,
) -> Result<script::Recipe> {
    validate_receipt(source, receipt)?;
    let path = contained(&source.install_dir, &receipt.path)?;
    let bytes = read_bounded(&path, 4 * 1024 * 1024)?;
    if hex(&Sha1::digest(&bytes)) != receipt.sha1.to_ascii_lowercase() {
        return Err(failure(format!(
            "First-run script was modified: {}; verify/update the installation",
            path.display()
        )));
    }
    script::parse(&decode(&bytes)?, &source.install_dir, language)
}

fn decode(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        if !bytes.len().is_multiple_of(2) {
            return Err(failure("Invalid UTF-16 install script"));
        }
        return String::from_utf16(
            &bytes[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| u16::from_le_bytes([s[0], s[1]]))
                .collect::<Vec<_>>(),
        )
        .map_err(Into::into);
    }
    Ok(std::str::from_utf8(bytes)?.to_owned())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn validate_receipt(game: &InstalledGame, receipt: &Receipt) -> Result<()> {
    safe::checked(&game.install_dir, Path::new(&receipt.path), true)?;
    if receipt.sha1.len() != 40
        || !receipt.sha1.bytes().all(|b| b.is_ascii_hexdigit())
        || game.installed_depots.get(&receipt.depot) != Some(&receipt.manifest)
    {
        return Err(failure("Invalid or stale first-run script receipt"));
    }
    Ok(())
}

fn recorded(game: &InstalledGame) -> Result<Option<Vec<Receipt>>> {
    let acf = read_vdf(
        &game
            .library_root
            .join(format!("steamapps/appmanifest_{}.acf", game.app_id)),
    )?;
    let Some(record) = acf.at(&["appstate", "steamtuiinstallscripts"]) else {
        return Ok(None);
    };
    if record.text("version") != Some("1") {
        return Err(failure("Unsupported first-run script receipt version"));
    }
    let mut scripts = Vec::new();
    for node in record
        .child("files")
        .into_iter()
        .flat_map(|n| n.children.values())
    {
        let receipt = Receipt {
            path: node
                .text("path")
                .ok_or_else(|| failure("Script receipt lacks path"))?
                .into(),
            sha1: node
                .text("sha1")
                .ok_or_else(|| failure("Script receipt lacks hash"))?
                .into(),
            depot: node
                .text("depot")
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| failure("Script receipt lacks depot"))?,
            manifest: node
                .text("manifest")
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| failure("Script receipt lacks manifest"))?,
        };
        validate_receipt(game, &receipt)?;
        scripts.push(receipt);
        if scripts.len() > 128 {
            return Err(failure("Too many first-run scripts"));
        }
    }
    Ok(Some(scripts))
}

async fn discover(
    game: &InstalledGame,
    connection: Option<&steam_vent::Connection>,
    directory: &Path,
) -> Result<Vec<Receipt>> {
    if let Some(scripts) = recorded(game)? {
        return Ok(scripts);
    }
    // Legacy Steam installs have no verified receipt. Obtain authoritative file
    // flags/hashes from their installed manifests, never by scanning *.vdf files.
    let identity = serde_json::to_vec(&(game.app_id, &game.branch, &game.installed_depots))?;
    let cache = directory.join(format!("scripts-{}.json", hex(&Sha1::digest(identity))));
    if cache.exists() {
        safe::checked(
            directory,
            Path::new(cache.file_name().expect("cache name")),
            true,
        )?;
        let scripts: Vec<Receipt> = serde_json::from_slice(&read_bounded(&cache, 1024 * 1024)?)?;
        if scripts.len() > 128 {
            return Err(failure("Too many cached first-run scripts"));
        }
        for receipt in &scripts {
            validate_receipt(game, receipt)?;
        }
        return Ok(scripts);
    }
    if game.installed_depots.is_empty() {
        return Ok(Vec::new());
    }
    let connection = connection.ok_or_else(|| {
        failure("Connect once to verify first-run scripts for this existing installation")
    })?;
    let metadata = match app_document(game) {
        Ok(document) => document,
        Err(_) => {
            crate::library::inspect_apps(connection, &BTreeSet::from([game.app_id]))
                .await
                .map_err(|e| failure(format!("Could not fetch prerequisite metadata: {e}")))?;
            app_document(game)?
        }
    };
    let acf = read_vdf(
        &game
            .library_root
            .join(format!("steamapps/appmanifest_{}.acf", game.app_id)),
    )?;
    let client = ContentClient::new(connection.clone())
        .await?
        .with_branch(&game.branch);
    let cancel = CancellationToken::new();
    let mut overlay = BTreeMap::<PathBuf, Option<Receipt>>::new();
    for (&id, &manifest_id) in &game.installed_depots {
        // Some older appmanifests listed shared depots among installed game
        // depots. Their payloads/scripts belong to the separate source root.
        if acf
            .at(&["appstate", "shareddepots", &id.to_string()])
            .is_some()
            || metadata
                .at(&["depots", &id.to_string()])
                .and_then(|d| d.text("sharedinstall"))
                .is_some_and(|flag| flag == "1" || flag.eq_ignore_ascii_case("true"))
        {
            continue;
        }
        let source = metadata
            .at(&["depots", &id.to_string()])
            .and_then(|d| d.text("depotfromapp"))
            .and_then(|id| id.parse().ok())
            .unwrap_or(game.app_id);
        let manifest = client
            .manifest(
                &DepotSpec {
                    depot_id: id,
                    manifest_id,
                    source_app_id: source,
                },
                &cancel,
            )
            .await?;
        for file in manifest.files {
            let receipt = if file.flags & 256 != 0 {
                Some(Receipt {
                    path: file.path.to_string_lossy().into_owned(),
                    sha1: hex(&file
                        .sha1
                        .ok_or_else(|| failure("Install script has no content hash"))?),
                    depot: id,
                    manifest: manifest_id,
                })
            } else {
                None
            };
            overlay.insert(file.path, receipt);
        }
    }
    let scripts: Vec<_> = overlay.into_values().flatten().collect();
    if scripts.len() > 128 {
        return Err(failure("Too many first-run scripts"));
    }
    let temporary = cache.with_extension(format!("{}.tmp", reaper::new_token()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(&scripts)?)?;
    file.sync_all()?;
    fs::rename(&temporary, &cache)?;
    fs::File::open(directory)?.sync_all()?;
    Ok(scripts)
}
