//! Per-game choice among a game's selectable launch entries, the equivalent of
//! Steam's launch-option dialog (e.g. DirectX 11 / DirectX 12). The choice is a
//! property of the installed game, so it is stored once, outside any account.
use super::*;
use std::io::Read;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOption {
    pub index: u32,
    pub description: String,
}

/// Entries usable on this host that the user may choose between.
pub fn available(game: &InstalledGame) -> Result<Vec<LaunchOption>> {
    Ok(list(&app_document(game)?))
}

pub(super) fn list(app: &Node) -> Vec<LaunchOption> {
    let Ok(entries) = launch_entries(app) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|(_, n)| {
            (is_default_entry(n) || is_option_entry(n)) && is_plain_entry(n) && usable_here(n, app)
        })
        .map(|(index, node)| LaunchOption {
            index,
            description: describe(node),
        })
        .collect()
}

fn describe(node: &Node) -> String {
    let text = |key| node.text(key).filter(|s| !s.is_empty());
    let label = text("description")
        .map(str::to_owned)
        .or_else(|| {
            node.child("description_loc")
                .and_then(|loc| loc.text("english"))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            let executable = text("executable").unwrap_or("launch entry");
            match text("arguments") {
                Some(arguments) => format!("{executable} {arguments}"),
                None => executable.to_owned(),
            }
        });
    label.chars().filter(|c| !c.is_control()).take(80).collect()
}

/// The entry a launch uses now: the stored choice when it is still valid,
/// otherwise the automatic pick.
pub fn effective(game: &InstalledGame) -> Result<LaunchOption> {
    let app = app_document(game)?;
    let (index, entry, _) = select_indexed(game, &app, chosen(game.app_id))?;
    Ok(LaunchOption {
        index,
        description: describe(entry),
    })
}

/// Advances to the next available option and stores it.
pub fn cycle(game: &InstalledGame) -> Result<LaunchOption> {
    let options = available(game)?;
    if options.len() < 2 {
        return Err(failure("This game has no alternative launch options"));
    }
    let current = effective(game)?.index;
    let position = options.iter().position(|o| o.index == current);
    let next = options[position.map_or(0, |p| (p + 1) % options.len())].clone();
    set(game.app_id, Some(next.index))?;
    Ok(next)
}

/// Stores the choice for `index`, or clears it (automatic pick) for `None`.
pub fn choose(game: &InstalledGame, index: Option<u32>) -> Result<LaunchOption> {
    if let Some(index) = index
        && !available(game)?.iter().any(|o| o.index == index)
    {
        return Err(failure("No such launch option for this game on this host"));
    }
    set(game.app_id, index)?;
    effective(game)
}

fn path() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_STATE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .filter(|p| p.is_absolute())
        .ok_or_else(|| failure("Neither XDG_STATE_HOME nor HOME is set"))?;
    Ok(root.join("steam-tui/launch-options.json"))
}

fn load(path: &Path) -> BTreeMap<u32, u32> {
    fs::File::open(path)
        .ok()
        .and_then(|file| {
            let mut bytes = Vec::new();
            file.take(1 << 20).read_to_end(&mut bytes).ok()?;
            serde_json::from_slice(&bytes).ok()
        })
        .unwrap_or_default()
}

/// The stored launch entry index for an app, if any.
pub fn chosen(app_id: u32) -> Option<u32> {
    load(&path().ok()?).get(&app_id).copied()
}

fn set(app_id: u32, index: Option<u32>) -> Result<()> {
    set_at(&path()?, app_id, index)
}

fn set_at(path: &Path, app_id: u32, index: Option<u32>) -> Result<()> {
    let mut map = load(path);
    match index {
        Some(index) => map.insert(app_id, index),
        None => map.remove(&app_id),
    };
    let parent = path
        .parent()
        .ok_or_else(|| failure("Launch option store has no parent"))?;
    fs::create_dir_all(parent)?;
    crate::install::filesystem::atomic_write(path, &serde_json::to_vec(&map)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_persist_per_app_and_clear() {
        let dir = std::env::temp_dir().join(format!("steam-tui-options-{}", std::process::id()));
        let file = dir.join("nested/launch-options.json");
        assert!(load(&file).is_empty());
        set_at(&file, 7, Some(3)).unwrap();
        set_at(&file, 9, Some(2)).unwrap();
        assert_eq!(load(&file), BTreeMap::from([(7, 3), (9, 2)]));
        set_at(&file, 7, None).unwrap();
        assert_eq!(load(&file), BTreeMap::from([(9, 2)]));
        fs::write(&file, b"not json").unwrap();
        assert!(load(&file).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
