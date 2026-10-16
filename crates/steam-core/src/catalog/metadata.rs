use super::{DepotSpec, InstallPlan, InstallPlatform, Result, failure};
use crate::installed::{InstalledGame, Node};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

/// Everything `for_platform` needs that does not depend on the target OS.
struct Target<'a> {
    app_id: u32,
    existing: Option<&'a InstalledGame>,
    owned: &'a BTreeSet<u32>,
    licensed_depots: &'a BTreeSet<u32>,
    docs: &'a BTreeMap<u32, Node>,
    doc: &'a Node,
    depots: &'a Node,
    branch: &'a str,
    build_id: u64,
    language: &'a str,
}

enum Outcome {
    Plan(InstallPlan),
    /// Nothing downloadable remained for this platform. Only this outcome lets a
    /// new install fall back to the other platform.
    Empty(String),
}

/// Choose a plan. What blocks and what degrades:
///
/// * Blocking: missing app metadata, private/encrypted/invalid branch, invalid
///   build or manifest GID, a source app's metadata missing for a depot that must
///   be installed, ambiguous installed platform/architecture (an update never
///   guesses a target change), unsafe install directory.
/// * Skipped silently: `sharedinstall` depots (Steamworks Common Redistributables,
///   VC/DirectX redistributables; they are installed once per library, never into
///   the game's directory), depots of other OS/language/low-violence/architecture,
///   unowned DLC depots, depots that are not content (no manifest and no source app),
///   and depots without a manifest for a non-public branch that are not installed.
/// * Fallback: a new install prefers Linux; when no Linux depot remains it falls
///   back to Windows (run through Proton). Installed games never change platform.
pub(super) fn select(
    app_id: u32,
    existing: Option<&InstalledGame>,
    owned: &BTreeSet<u32>,
    licensed_depots: &BTreeSet<u32>,
    docs: &BTreeMap<u32, Node>,
) -> Result<InstallPlan> {
    let doc = docs
        .get(&app_id)
        .ok_or_else(|| failure("App metadata is unavailable"))?;
    let depots = doc
        .child("depots")
        .ok_or_else(|| failure("This app has no downloadable depot metadata"))?;
    let branch = existing.map(|g| g.branch.as_str()).unwrap_or("public");
    let branch_node = depots
        .at(&["branches", branch])
        .ok_or_else(|| failure(format!("Branch {branch} is unavailable or private")))?;
    if branch_node
        .text("pwdrequired")
        .is_some_and(|s| s != "0" && !s.eq_ignore_ascii_case("false"))
    {
        return Err(failure(format!(
            "Branch {branch} requires a password; encrypted branches are unsupported"
        )));
    }
    let build_id = number(branch_node, "buildid")
        .filter(|id| *id != 0)
        .ok_or_else(|| failure(format!("Branch {branch} has no valid build ID")))?;
    let target = Target {
        app_id,
        existing,
        owned,
        licensed_depots,
        docs,
        doc,
        depots,
        branch,
        build_id,
        language: existing.map(|g| g.language.as_str()).unwrap_or("english"),
    };
    let mut empty = Vec::new();
    for platform in platforms(existing, doc, depots)? {
        match for_platform(&target, platform)? {
            Outcome::Plan(plan) => return Ok(plan),
            Outcome::Empty(message) => empty.push(message),
        }
    }
    Err(failure(empty.join("; ")))
}

/// Platforms to try, in preference order.
fn platforms(
    existing: Option<&InstalledGame>,
    doc: &Node,
    depots: &Node,
) -> Result<Vec<InstallPlatform>> {
    if let Some(platform) = existing.and_then(|g| g.platform) {
        return Ok(vec![platform]);
    }
    let supports_linux = supports(doc, depots, InstallPlatform::Linux);
    let supports_windows = supports(doc, depots, InstallPlatform::Windows);
    let Some(game) = existing else {
        let list: Vec<_> = [
            (supports_linux, InstallPlatform::Linux),
            (supports_windows, InstallPlatform::Windows),
        ]
        .into_iter()
        .filter_map(|(supported, platform)| supported.then_some(platform))
        .collect();
        if list.is_empty() {
            return Err(failure(
                "No supported Linux or Windows download target is available",
            ));
        }
        return Ok(list);
    };
    if !game.installed_depots.is_empty() {
        // Infer an older ACF's target from platform-exclusive installed
        // depot IDs, not from the OS on which this client is running.
        let mut linux = false;
        let mut windows = false;
        for id in game.installed_depots.keys() {
            if let Some(config) = depots
                .child(&id.to_string())
                .and_then(|d| d.child("config"))
                && let Some(os) = config.text("oslist")
            {
                linux |= token(os, "linux") && !token(os, "windows");
                windows |= token(os, "windows") && !token(os, "linux");
            }
        }
        return match (linux, windows) {
            (true, false) => Ok(vec![InstallPlatform::Linux]),
            (false, true) => Ok(vec![InstallPlatform::Windows]),
            (true, true) => Err(failure(
                "Installed depot platform is ambiguous; no target change will be guessed",
            )),
            _ if supports_linux && !supports_windows => Ok(vec![InstallPlatform::Linux]),
            _ if supports_windows && !supports_linux => Ok(vec![InstallPlatform::Windows]),
            _ => Err(failure(
                "Installed platform is unknown; no Linux/Windows target change will be guessed",
            )),
        };
    }
    match (supports_linux, supports_windows) {
        (true, true) => Err(failure(
            "Installed platform is unknown; no Linux/Windows target change will be guessed",
        )),
        (true, false) => Ok(vec![InstallPlatform::Linux]),
        (false, true) => Ok(vec![InstallPlatform::Windows]),
        (false, false) => Err(failure(
            "No supported Linux or Windows download target is available",
        )),
    }
}

fn for_platform(target: &Target<'_>, platform: InstallPlatform) -> Result<Outcome> {
    let Target {
        app_id,
        existing,
        owned,
        licensed_depots,
        docs,
        doc,
        depots,
        branch,
        build_id,
        language,
    } = *target;
    let os = match platform {
        InstallPlatform::Linux => "linux",
        InstallPlatform::Windows => "windows",
    };
    let candidates: Vec<_> = depots
        .children
        .iter()
        .filter_map(|(id, depot)| {
            let id = id.parse::<u32>().ok().filter(|id| *id != 0)?;
            if shared_install(depot) {
                return None;
            }
            let config = depot.child("config");
            if config
                .and_then(|n| n.text("oslist"))
                .is_some_and(|v| !v.is_empty() && !token(v, os))
            {
                return None;
            }
            if config
                .and_then(|n| n.text("language"))
                .is_some_and(|v| !v.is_empty() && !token(v, language))
            {
                return None;
            }
            if config
                .and_then(|n| n.text("lowviolence"))
                .is_some_and(|v| v == "1")
            {
                return None;
            }
            if number(depot, "dlcappid")
                .and_then(|id| u32::try_from(id).ok())
                .is_some_and(|dlc| !owned.contains(&dlc) && !licensed_depots.contains(&id))
            {
                return None;
            }
            // Packages explicitly enumerate content grants. A restricted DLC depot
            // is never selected just because it appears in its parent app's PICS.
            if !licensed_depots.is_empty() && !licensed_depots.contains(&id) && !owned.contains(&id)
            {
                return None;
            }
            Some((id, depot))
        })
        .collect();
    let installed_arches: BTreeSet<_> = candidates
        .iter()
        .filter(|(id, _)| existing.is_some_and(|g| g.installed_depots.contains_key(id)))
        .filter_map(|(_, d)| d.at(&["config"]).and_then(|n| n.text("osarch")))
        .filter(|arch| matches!(*arch, "32" | "64"))
        .collect();
    if installed_arches.len() > 1 {
        return Err(failure(
            "Installed architecture is ambiguous; no target change will be guessed",
        ));
    }
    let arch = installed_arches.iter().next().copied().unwrap_or_else(|| {
        if candidates
            .iter()
            .any(|(_, d)| d.at(&["config"]).and_then(|n| n.text("osarch")) == Some("64"))
        {
            "64"
        } else {
            "32"
        }
    });
    let mut selected = Vec::new();
    let mut download_bytes = 0u64;
    let mut installed_bytes = 0u64;
    for (depot_id, depot) in candidates {
        if depot
            .at(&["config"])
            .and_then(|n| n.text("osarch"))
            .is_some_and(|v| !v.is_empty() && !token(v, arch))
        {
            continue;
        }
        let installed = existing.is_some_and(|g| g.installed_depots.contains_key(&depot_id));
        let (source_app_id, manifest) = manifest(app_id, depot_id, branch, installed, depot, docs)?;
        let Some(manifest) = manifest else {
            continue;
        };
        let gid = manifest
            .value
            .as_deref()
            .and_then(|v| v.parse::<u64>().ok())
            .or_else(|| number(manifest, "gid"))
            .filter(|gid| *gid != 0 && *gid != u64::MAX)
            .ok_or_else(|| failure(format!("Depot {depot_id} has an invalid manifest GID")))?;
        installed_bytes = installed_bytes
            .checked_add(number(manifest, "size").unwrap_or(0))
            .ok_or_else(|| failure("Installed size overflow"))?;
        download_bytes = download_bytes
            .checked_add(number(manifest, "download").unwrap_or(0))
            .ok_or_else(|| failure("Download size overflow"))?;
        selected.push(DepotSpec {
            depot_id,
            manifest_id: gid,
            source_app_id,
        });
    }
    if selected.is_empty() {
        return Ok(Outcome::Empty(format!(
            "No licensed {os}/{language} depots have manifests for branch {branch}"
        )));
    }
    let install_dir_name = existing
        .and_then(|g| g.install_dir.file_name())
        .and_then(|s| s.to_str())
        .or_else(|| doc.at(&["config"]).and_then(|n| n.text("installdir")))
        .ok_or_else(|| failure("Steam did not provide an installation directory name"))?;
    let path = Path::new(install_dir_name);
    if install_dir_name.is_empty()
        || install_dir_name.contains(['\\', '\0'])
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(failure(
            "Steam installation directory is not a safe single path component",
        ));
    }
    Ok(Outcome::Plan(InstallPlan {
        app_id,
        name: doc
            .at(&["common"])
            .and_then(|n| n.text("name"))
            .or_else(|| existing.map(|g| g.name.as_str()))
            .unwrap_or("Unnamed game")
            .to_owned(),
        install_dir_name: install_dir_name.to_owned(),
        build_id,
        platform,
        branch: branch.to_owned(),
        language: language.to_owned(),
        depots: selected,
        download_bytes,
        installed_bytes,
    }))
}

/// Redistributable depots (`sharedinstall`), e.g. Steamworks Common Redistributables,
/// are shared between games and installed by Steam into `steamapps/common/Steamworks
/// Shared`; this installer never installs them and never treats them as blockers.
pub(super) fn shared_install(depot: &Node) -> bool {
    depot
        .text("sharedinstall")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn supports(doc: &Node, depots: &Node, platform: InstallPlatform) -> bool {
    let os = match platform {
        InstallPlatform::Linux => "linux",
        InstallPlatform::Windows => "windows",
    };
    if let Some(list) = doc.at(&["common"]).and_then(|n| n.text("oslist")) {
        return token(list, os);
    }
    let mut declared = false;
    for (id, depot) in &depots.children {
        if id.parse::<u32>().is_err() {
            continue;
        }
        if let Some(list) = depot
            .at(&["config"])
            .and_then(|c| c.text("oslist"))
            .filter(|list| !list.is_empty())
        {
            declared = true;
            if token(list, os) {
                return true;
            }
        }
    }
    // [INFERENCE] Apps that declare no OS at all (no `common.oslist`, no depot
    // `oslist`) are treated as Windows content, Steam's historical default,
    // rather than blocked as "no supported target".
    !declared && platform == InstallPlatform::Windows
}

fn manifest<'a>(
    app: u32,
    depot_id: u32,
    branch: &str,
    installed: bool,
    initial: &'a Node,
    docs: &'a BTreeMap<u32, Node>,
) -> Result<(u32, Option<&'a Node>)> {
    let mut source = app;
    let mut depot = initial;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(source) {
            return Err(failure(format!(
                "Shared depot {depot_id} contains a source-app cycle"
            )));
        }
        if depot.at(&["encryptedmanifests", branch]).is_some()
            && depot.at(&["manifests", branch]).is_none()
        {
            return Err(failure(format!(
                "Depot {depot_id} has an encrypted manifest for branch {branch}"
            )));
        }
        if let Some(manifests) = depot.child("manifests") {
            if !branch.eq_ignore_ascii_case("public") && manifests.child(branch).is_none() {
                // Never fall back to the public manifest. A depot that is not in
                // this branch is skipped unless it is already installed, in which
                // case an update would silently drop it and must block instead.
                if installed {
                    return Err(failure(format!(
                        "Installed depot {depot_id} has no manifest for branch {branch}; refusing an automatic public-branch fallback"
                    )));
                }
                return Ok((source, None));
            }
            let key_source = number(depot, "depotfromapp")
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(source);
            return Ok((key_source, manifests.child(branch)));
        }
        let Some(next) = number(depot, "depotfromapp")
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n != 0)
        else {
            return Ok((source, None));
        };
        source = next;
        depot = docs
            .get(&source)
            .and_then(|d| d.at(&["depots"]))
            .and_then(|d| d.child(&depot_id.to_string()))
            .ok_or_else(|| {
                failure(format!(
                    "Shared depot {depot_id} source app {source} metadata is unavailable"
                ))
            })?;
    }
}

fn number(node: &Node, key: &str) -> Option<u64> {
    node.text(key)?.parse().ok()
}
fn token(value: &str, expected: &str) -> bool {
    value
        .split(',')
        .any(|s| s.trim().eq_ignore_ascii_case(expected))
}
