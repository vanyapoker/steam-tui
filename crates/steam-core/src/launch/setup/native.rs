//! Linux CInstallScriptManagerPosix operations from Ghidra 0139afe0:
//! symlink (0139ac10), chmod (0139a940); defaults is looked up but not
//! executed in this Linux binary (0139b062..0139b082).
use super::*;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
#[derive(Debug)]
pub(super) struct Recipe {
    root: PathBuf,
    links: Vec<(PathBuf, PathBuf)>,
    modes: Vec<(PathBuf, u32)>,
}
fn path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    safe::relative(&path)?;
    Ok(path)
}
pub(super) fn parse(text: &str, root: &Path) -> Result<Recipe> {
    let document = crate::installed::text_preserving_keys(text)?;
    let script = document
        .child("installscript")
        .ok_or_else(|| failure("Native script lacks InstallScript"))?;
    let version = script.text("version").unwrap_or("1").parse::<u32>()?;
    if !matches!(version, 1 | 2) {
        return Err(failure("Unsupported native install-script version"));
    }
    let mut recipe = Recipe {
        root: root.to_owned(),
        links: Vec::new(),
        modes: Vec::new(),
    };
    for (section, entries) in &script.children {
        match section.to_ascii_lowercase().as_str() {
            "version" | "defaults" => {}
            "symlink" | "chmod" => {
                for (name, entry) in &entries.children {
                    let (file, value) = if version == 1 {
                        (
                            name.as_str(),
                            entry
                                .value
                                .as_deref()
                                .ok_or_else(|| failure("Expected scalar native setup v1 action"))?,
                        )
                    } else if section.eq_ignore_ascii_case("symlink") {
                        if entry.children.keys().any(|k| {
                            !k.eq_ignore_ascii_case("link") && !k.eq_ignore_ascii_case("target")
                        }) {
                            return Err(failure("Unknown native symlink field"));
                        }
                        (
                            entry
                                .text("link")
                                .ok_or_else(|| failure("Symlink lacks link"))?,
                            entry
                                .text("target")
                                .ok_or_else(|| failure("Symlink lacks target"))?,
                        )
                    } else {
                        if entry.children.keys().any(|k| {
                            !k.eq_ignore_ascii_case("file") && !k.eq_ignore_ascii_case("mode")
                        }) {
                            return Err(failure("Unknown native chmod field"));
                        }
                        (
                            entry
                                .text("file")
                                .ok_or_else(|| failure("Chmod lacks file"))?,
                            entry
                                .text("mode")
                                .ok_or_else(|| failure("Chmod lacks mode"))?,
                        )
                    };
                    let file = path(file)?;
                    if section.eq_ignore_ascii_case("symlink") {
                        let target = PathBuf::from(value);
                        let resolved = resolve_target(&file, &target)?;
                        safe::checked(root, &resolved, true)?;
                        recipe.links.push((file, target));
                    } else {
                        let mode = u32::from_str_radix(value, 8)?;
                        if mode > 0o777 {
                            return Err(failure(
                                "Native setup chmod may only change ordinary permission bits",
                            ));
                        }
                        recipe.modes.push((file, mode));
                    }
                    if recipe.links.len() + recipe.modes.len() > 4096 {
                        return Err(failure("Native setup exceeds action limit"));
                    }
                }
            }
            _ => {
                return Err(failure(format!(
                    "Unsupported native setup directive {section}"
                )));
            }
        }
    }
    // Validate every target before performing any action.
    for (file, target) in &recipe.links {
        let leaf = safe::checked(root, file, false)?;
        match fs::symlink_metadata(&leaf) {
            Ok(m) if m.file_type().is_symlink() && fs::read_link(&leaf)? == *target => {}
            Ok(_) => {
                return Err(failure(format!(
                    "Native setup would replace existing content: {}",
                    leaf.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let parent = leaf
            .parent()
            .ok_or_else(|| failure("Native link has no parent"))?;
        if !parent.is_dir() {
            return Err(failure("Native link parent is missing"));
        }
    }
    for (file, _) in &recipe.modes {
        let file = safe::checked(root, file, true)?;
        if !fs::metadata(&file)?.is_file() {
            return Err(failure("Native chmod target must be a regular file"));
        }
    }
    Ok(recipe)
}
fn resolve_target(link: &Path, target: &Path) -> Result<PathBuf> {
    if target.is_absolute() || target.as_os_str().is_empty() {
        return Err(failure("Native symlink target must be relative"));
    }
    let mut resolved = link.parent().unwrap_or(Path::new("")).to_path_buf();
    for part in target.components() {
        match part {
            std::path::Component::Normal(p) => resolved.push(p),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if resolved.pop() => {}
            _ => return Err(failure("Native symlink target escapes installation")),
        }
    }
    safe::relative(&resolved)?;
    Ok(resolved)
}
pub(super) fn execute(recipes: &[Recipe], cancel: &CancellationToken) -> Result<()> {
    if recipes
        .iter()
        .map(|r| r.links.len() + r.modes.len())
        .sum::<usize>()
        > 4096
    {
        return Err(failure("Native setup exceeds total action limit"));
    }
    for recipe in recipes {
        for (link, target) in &recipe.links {
            if cancel.is_cancelled() {
                return Err(failure("Native setup cancelled"));
            }
            let leaf = safe::checked(&recipe.root, link, false)?;
            safe::checked(&recipe.root, &resolve_target(link, target)?, true)?;
            match fs::symlink_metadata(&leaf) {
                Ok(m) if m.file_type().is_symlink() && fs::read_link(&leaf)? == *target => continue,
                Ok(_) => return Err(failure("Native symlink destination changed during setup")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            symlink(target, &leaf)?;
            fs::File::open(leaf.parent().unwrap())?.sync_all()?;
        }
        for (file, mode) in &recipe.modes {
            if cancel.is_cancelled() {
                return Err(failure("Native setup cancelled"));
            }
            let path = safe::checked(&recipe.root, file, true)?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            if !file.metadata()?.is_file() {
                return Err(failure("Native chmod target changed type"));
            }
            file.set_permissions(fs::Permissions::from_mode(*mode))?;
            file.sync_all()?;
        }
    }
    Ok(())
}
