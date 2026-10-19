//! DLC membership is public metadata; a selection or receipt is never a license.
use crate::catalog::InstallPlatform;
use crate::install::filesystem as safe;
use crate::installed::{self, InstalledGame, Node, Result, failure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
#[cfg(test)]
mod tests;

pub(crate) const MAX_DLC: usize = 4096;

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub disabled: BTreeSet<u32>,
    #[serde(default)]
    pub pending: BTreeMap<u32, bool>,
}

impl Selection {
    fn path(_library: &Path, account: u64, app: u32) -> Result<PathBuf> {
        if account == 0 || app == 0 {
            return Err(failure("DLC settings require an account and game ID"));
        }
        Ok(PathBuf::from(format!(
            "steamapps/.steam-tui/dlc/{account}/{app}.json"
        )))
    }

    pub(crate) fn read(library: &Path, account: u64, app: u32) -> Result<Self> {
        let path = safe::checked(library, &Self::path(library, account, app)?, true)?;
        let bytes = match installed::read_bounded(&path, 128 * 1024) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(Self::default());
            }
            Err(error) => return Err(error),
        };
        let value: Self = serde_json::from_slice(&bytes)?;
        if value.disabled.len() > MAX_DLC
            || value.disabled.contains(&0)
            || value.pending.len() > MAX_DLC
            || value.pending.contains_key(&0)
        {
            return Err(failure("Invalid DLC selection"));
        }
        Ok(value)
    }

    /// The caller holds the DLC settings lock. Installer locks are independent:
    /// settings describe intent, while the appmanifest records committed files.
    pub(crate) fn write(&self, library: &Path, account: u64, app: u32) -> Result<()> {
        if self.disabled.len() > MAX_DLC
            || self.pending.len() > MAX_DLC
            || self.disabled.contains(&0)
            || self.pending.contains_key(&0)
        {
            return Err(failure("Invalid DLC selection"));
        }
        let relative = Self::path(library, account, app)?;
        safe::checked(library, &relative, true)?;
        for path in [
            "steamapps/.steam-tui".to_owned(),
            "steamapps/.steam-tui/dlc".to_owned(),
            format!("steamapps/.steam-tui/dlc/{account}"),
        ] {
            safe::private_directory(&library.join(path))?;
        }
        safe::atomic_write(&library.join(relative), &serde_json::to_vec(self)?)?;
        Ok(())
    }

    pub(crate) fn update(
        library: &Path,
        account: u64,
        app: u32,
        change: impl FnOnce(&mut Self),
    ) -> Result<Self> {
        safe::checked(library, Path::new("steamapps/.steam-tui/dlc.lock"), true)?;
        safe::private_directory(&library.join("steamapps/.steam-tui"))?;
        let _lock = safe::Lock::acquire(&library.join("steamapps/.steam-tui/dlc.lock"))?;
        let mut selection = Self::read(library, account, app)?;
        change(&mut selection);
        selection.write(library, account, app)?;
        Ok(selection)
    }
}

fn id(text: Option<&str>) -> Option<u32> {
    text?.parse().ok().filter(|id| *id != 0)
}

pub(crate) fn ids(parent: u32, doc: &Node) -> Result<BTreeSet<u32>> {
    let mut ids = BTreeSet::new();
    if let Some(list) = doc.at(&["extended"]).and_then(|n| n.text("listofdlc")) {
        for value in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            ids.insert(id(Some(value)).ok_or_else(|| failure("Invalid DLC list ID"))?);
            if ids.len() > MAX_DLC {
                return Err(failure("Too many DLC entries"));
            }
        }
    }
    for (key, depot) in doc.child("depots").into_iter().flat_map(|n| &n.children) {
        if let Some(dlc) = id(depot.text("dlcappid")) {
            ids.insert(dlc);
        }
        if depot.text("nocontentdlc") == Some("1")
            && let Some(dlc) = id(Some(key))
        {
            ids.insert(dlc);
        }
    }
    ids.remove(&parent);
    if ids.len() > MAX_DLC {
        return Err(failure("Too many DLC entries"));
    }
    Ok(ids)
}

/// A disabled DLC must also be excluded when a package directly grants its depot.
pub(crate) fn filter(doc: &mut Node, selection: &Selection) {
    if let Some(depots) = doc
        .children
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("depots"))
        .map(|(_, v)| v)
    {
        depots.children.retain(|_, depot| {
            id(depot.text("dlcappid")).is_none_or(|dlc| !selection.disabled.contains(&dlc))
        });
    }
}

pub(crate) fn depot_scope(doc: &Node, target: u32) -> BTreeSet<u32> {
    doc.child("depots")
        .into_iter()
        .flat_map(|n| &n.children)
        .filter(|(_, d)| id(d.text("dlcappid")) == Some(target))
        .filter(|(_, d)| !crate::catalog::metadata::shared_install(d))
        .filter_map(|(key, _)| id(Some(key)))
        .collect()
}

fn matches(config: Option<&Node>, field: &str, expected: &str) -> bool {
    config.and_then(|n| n.text(field)).is_none_or(|s| {
        s.is_empty()
            || s.split(',')
                .any(|s| s.trim().eq_ignore_ascii_case(expected))
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub app_id: u32,
    pub name: String,
    pub available: bool,
    pub depots: BTreeSet<u32>,
    /// False when applicable content/metadata is unavailable. An empty depot
    /// list alone is insufficient to establish that content is bundled.
    pub known: bool,
}

pub(crate) fn entries(
    game: &InstalledGame,
    doc: &Node,
    children: &BTreeMap<u32, Node>,
) -> Result<Vec<Entry>> {
    let known_ids = ids(game.app_id, doc)?;
    if known_ids.is_empty() {
        return Ok(Vec::new());
    }
    let Some(depots) = doc.child("depots") else {
        return Err(failure("Missing parent depot metadata"));
    };
    let platforms = crate::catalog::metadata::platforms(Some(game), doc, depots)
        .map_err(|e| failure(e.to_string()))?;
    let platform = platforms
        .first()
        .ok_or_else(|| failure("Unknown installed platform"))?;
    let os = match platform {
        InstallPlatform::Linux => "linux",
        InstallPlatform::Windows => "windows",
    };
    let arches: BTreeSet<_> = game
        .installed_depots
        .keys()
        .filter_map(|id| {
            depots
                .at(&[&id.to_string(), "config"])
                .and_then(|n| n.text("osarch"))
        })
        .filter(|s| matches!(*s, "32" | "64"))
        .collect();
    let default_arch = if depots.children.values().any(|d| {
        matches(d.child("config"), "oslist", os)
            && matches(d.child("config"), "language", &game.language)
            && d.child("config").and_then(|n| n.text("osarch")) == Some("64")
    }) {
        "64"
    } else {
        "32"
    };
    let mut result = Vec::new();
    for app_id in known_ids {
        let child = children.get(&app_id);
        let mut entry = Entry {
            app_id,
            name: child
                .and_then(|n| n.child("common"))
                .and_then(|n| n.text("name"))
                .unwrap_or("")
                .to_owned(),
            available: child
                .and_then(|n| n.child("extended"))
                .and_then(|n| n.text("dlcavailableonstore"))
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|v| v > 0),
            depots: BTreeSet::new(),
            known: false,
        };
        let explicit_bundled = depots
            .child(&app_id.to_string())
            .is_some_and(|n| n.text("nocontentdlc") == Some("1"));
        let mut declared = false;
        let mut unknown_manifest = false;
        for (key, depot) in &depots.children {
            if id(depot.text("dlcappid")) != Some(app_id) {
                continue;
            }
            if crate::catalog::metadata::shared_install(depot) {
                continue;
            }
            declared = true;
            let config = depot.child("config");
            if !matches(config, "oslist", os)
                || !matches(config, "language", &game.language)
                || !(if arches.is_empty() {
                    matches(config, "osarch", default_arch)
                } else {
                    arches.iter().any(|arch| matches(config, "osarch", arch))
                })
                || config.and_then(|n| n.text("lowviolence")) == Some("1")
            {
                continue;
            }
            if depot.text("nocontentdlc") == Some("1") {
                continue;
            }
            let Some(depot_id) = id(Some(key)) else {
                return Err(failure("Invalid DLC depot ID"));
            };
            // Source-app depots are still installed into the parent game's
            // content tree. Resolve their manifests during planning, not here.
            if depot.at(&["manifests", &game.branch]).is_some()
                || depot.text("depotfromapp").is_some()
                || game.installed_depots.contains_key(&depot_id)
            {
                entry.depots.insert(depot_id);
            } else if depot.child("encryptedmanifests").is_some() {
                unknown_manifest = true;
            }
        }
        // A fully described DLC with no independently downloadable content is
        // shipped in the base game. Missing child PICS remains unknown.
        let child_bundled = child.is_some_and(|child| {
            child
                .at(&["common"])
                .and_then(|n| n.text("type"))
                .is_some_and(|t| t.eq_ignore_ascii_case("dlc"))
                && child.child("depots").is_none_or(|depots| {
                    !depots
                        .children
                        .values()
                        .any(|n| n.child("manifests").is_some() || n.text("depotfromapp").is_some())
                })
        });
        entry.known = !unknown_manifest
            && (explicit_bundled || !entry.depots.is_empty() || (!declared && child_bundled));
        result.push(entry);
    }
    Ok(result)
}

pub(crate) fn snapshot(
    game: &InstalledGame,
    entries: &[Entry],
    selection: &Selection,
) -> steam_ipc::AppsSnapshot {
    steam_ipc::AppsSnapshot {
        build_id: game
            .build_id
            .and_then(|id| u32::try_from(id).ok())
            .unwrap_or(0),
        branch: game.branch.clone(),
        depots: game.installed_depots.keys().copied().collect(),
        dlc: entries
            .iter()
            .map(|entry| {
                let enabled = !selection.disabled.contains(&entry.app_id);
                steam_ipc::DlcInfo {
                    app_id: entry.app_id,
                    name: entry.name.clone(),
                    available: entry.available,
                    enabled,
                    installed: enabled
                        && entry.known
                        && entry
                            .depots
                            .iter()
                            .all(|id| game.installed_depots.get(id).is_some_and(|gid| *gid != 0)),
                    depots: entry.depots.iter().copied().collect(),
                    ..Default::default()
                }
            })
            .collect(),
    }
}
