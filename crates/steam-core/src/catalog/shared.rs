//! Shared redistributables have their own app installation, never a directory
//! overlay inside the referring game. Retain the union required by the library.
use super::*;

pub(crate) fn requirements(
    app: &Node,
    platform: InstallPlatform,
    language: &str,
) -> installed::Result<BTreeMap<u32, BTreeSet<u32>>> {
    let mut result = BTreeMap::<u32, BTreeSet<u32>>::new();
    let os = match platform {
        InstallPlatform::Linux => "linux",
        InstallPlatform::Windows => "windows",
    };
    for (id, depot) in app.child("depots").into_iter().flat_map(|d| &d.children) {
        if !metadata::shared_install(depot) {
            continue;
        }
        let config = depot.child("config");
        let applies = |key: &str, value: &str| {
            config.and_then(|c| c.text(key)).is_none_or(|s| {
                s.is_empty() || s.split(',').any(|v| v.trim().eq_ignore_ascii_case(value))
            })
        };
        if !applies("oslist", os) || !applies("language", language) {
            continue;
        }
        let depot_id = id
            .parse::<u32>()
            .ok()
            .filter(|id| *id != 0)
            .ok_or_else(|| installed::failure("Invalid shared redistributable depot ID"))?;
        let source = depot
            .text("depotfromapp")
            .and_then(|id| id.parse::<u32>().ok())
            .filter(|id| *id != 0)
            .ok_or_else(|| installed::failure("Shared redistributable has no source app"))?;
        result.entry(source).or_default().insert(depot_id);
        if result.len() > 16 || result.values().map(BTreeSet::len).sum::<usize>() > 128 {
            return Err(installed::failure("Shared prerequisite graph is too large"));
        }
    }
    Ok(result)
}

pub(crate) fn installed(
    game: &InstalledGame,
    app_id: u32,
) -> installed::Result<Option<InstalledGame>> {
    let file = game
        .library_root
        .join(format!("steamapps/appmanifest_{app_id}.acf"));
    match fs_metadata(&file) {
        Ok(()) => installed::manifest_game(
            &file,
            &game.library_root,
            &game.steam_root,
            &BTreeMap::new(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn fs_metadata(path: &std::path::Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "Shared appmanifest is not a regular file",
        ));
    }
    Ok(())
}

pub(crate) async fn plan(
    connection: &Connection,
    app_id: u32,
    requested: &BTreeSet<u32>,
    platform: InstallPlatform,
    existing: Option<&InstalledGame>,
) -> Result<InstallPlan> {
    let (mut found, _) = library::inspect_apps(connection, &BTreeSet::from([app_id])).await?;
    let document = found
        .remove(&app_id)
        .and_then(|app| app.document)
        .ok_or_else(|| failure("Shared redistributable app metadata is unavailable"))?;
    plan_document(app_id, requested, platform, existing, document)
}

fn plan_document(
    app_id: u32,
    requested: &BTreeSet<u32>,
    platform: InstallPlatform,
    existing: Option<&InstalledGame>,
    document: Node,
) -> Result<InstallPlan> {
    let mut selected = requested.clone();
    if let Some(existing) = existing {
        selected.extend(existing.installed_depots.keys().copied());
    }
    let docs = BTreeMap::from([(app_id, document)]);
    let mut plan = resolve(app_id, existing, &BTreeSet::new(), &selected, &docs)?;
    if plan.platform != platform
        || selected
            .iter()
            .any(|id| !plan.depots.iter().any(|d| d.depot_id == *id))
    {
        return Err(failure(
            "Steam did not provide all required shared redistributable depots for the selected platform",
        ));
    }
    // The shared source may advertise tools not requested by this game/library.
    plan.depots.retain(|d| selected.contains(&d.depot_id));
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn document() -> Node {
        installed::text(r#""appid" "228980" "common" { "name" "Shared" "oslist" "windows" }
            "config" { "installdir" "Shared" } "depots" {
            "10" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "30" "size" "7" } } }
            "11" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "40" "size" "9" } } }
            "branches" { "public" { "buildid" "100" } }
        }"#).unwrap()
    }
    #[test]
    fn shared_payload_plan_downloads_only_requested_depots_and_retains_library_union() {
        let first = plan_document(
            228980,
            &BTreeSet::from([10]),
            InstallPlatform::Windows,
            None,
            document(),
        )
        .unwrap();
        assert_eq!(
            first.depots.iter().map(|d| d.depot_id).collect::<Vec<_>>(),
            [10]
        );
        let existing = InstalledGame {
            app_id: 228980,
            name: "Shared".into(),
            size_bytes: None,
            last_played: None,
            playtime_minutes: None,
            install_dir: "/library/steamapps/common/Shared".into(),
            library_root: "/library".into(),
            steam_root: "/root".into(),
            launch_options: None,
            build_id: Some(90),
            branch: "public".into(),
            language: "english".into(),
            platform: Some(InstallPlatform::Windows),
            installed_depots: BTreeMap::from([(10, 20)]),
        };
        let second = plan_document(
            228980,
            &BTreeSet::from([11]),
            InstallPlatform::Windows,
            Some(&existing),
            document(),
        )
        .unwrap();
        assert_eq!(
            second
                .depots
                .iter()
                .map(|d| (d.depot_id, d.manifest_id))
                .collect::<Vec<_>>(),
            [(10, 30), (11, 40)]
        );
        assert_eq!(second.installed_bytes, 16);
        assert!(
            plan_document(
                228980,
                &BTreeSet::from([12]),
                InstallPlatform::Windows,
                Some(&existing),
                document()
            )
            .is_err()
        );
    }
}
