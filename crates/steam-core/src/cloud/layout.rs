//! Where Steam Cloud files live on this machine: `ufs` save rules from the app's
//! PICS document, root-token mapping (Proton prefix or native Linux), name
//! composition, containment-checked paths, atomic writes and the save-file scan.
//! Unknown roots are reported and skipped, never guessed.

use crate::installed::{Node, Result, failure};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// Most files and directory depth the save scan visits.
const MAX_SCAN_FILES: usize = 5000;
const MAX_SCAN_DEPTH: usize = 8;

/// The operating system the game believes it runs on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Os {
    /// A Windows build run through Proton.
    Windows,
    Linux,
}

impl Os {
    /// Name used by `ufs` `platforms` and `rootoverrides`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Linux => "Linux",
        }
    }

    /// `ERemoteStoragePlatform` bit (Windows 1, Linux 8); not part of the generated protos.
    pub fn platform_bit(self) -> u32 {
        match self {
            Self::Windows => 1,
            Self::Linux => 8,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SaveRule {
    /// `None` is the unrooted `userdata/<account>/<app>/remote` directory.
    pub root: Option<String>,
    pub path: String,
    pub pattern: String,
    pub recursive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RootOverride {
    os: String,
    negate: bool,
    root: String,
    use_instead: String,
    transforms: Vec<(String, String)>,
    addpath: String,
}

/// The app's `ufs` section.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Ufs {
    pub quota: Option<u64>,
    pub max_files: Option<u64>,
    pub rules: Vec<SaveRule>,
    pub(super) overrides: Vec<RootOverride>,
}

fn numbered(node: &Node) -> Vec<&Node> {
    let mut entries: Vec<_> = node
        .children
        .iter()
        .filter_map(|(key, child)| key.parse::<u32>().ok().map(|index| (index, child)))
        .collect();
    entries.sort_by_key(|(index, _)| *index);
    entries.into_iter().map(|(_, child)| child).collect()
}

impl Ufs {
    pub fn expand_account(&mut self, account_id: u32) {
        let expand = |text: &mut String| {
            *text = text
                .replace("{Steam3AccountID}", &account_id.to_string())
                .replace(
                    "{64BitSteamID}",
                    &(76561197960265728u64 + u64::from(account_id)).to_string(),
                );
        };
        for rule in &mut self.rules {
            expand(&mut rule.path);
        }
        for over in &mut self.overrides {
            expand(&mut over.addpath);
            for (find, replace) in &mut over.transforms {
                expand(find);
                expand(replace);
            }
        }
    }
    /// `None` when the app has no `ufs` section: Steam Cloud is not configured.
    pub fn parse(app: &Node, os: Os) -> Option<Self> {
        let ufs = app.child("ufs")?;
        let mut parsed = Self {
            quota: ufs.text("quota").and_then(|v| v.parse().ok()),
            max_files: ufs.text("maxnumfiles").and_then(|v| v.parse().ok()),
            ..Self::default()
        };
        if let Some(savefiles) = ufs.child("savefiles") {
            for rule in numbered(savefiles) {
                let platforms_ok = rule.child("platforms").is_none_or(|platforms| {
                    platforms.children.values().any(|value| {
                        value.value.as_deref().is_some_and(|name| {
                            name.eq_ignore_ascii_case(os.name()) || name.eq_ignore_ascii_case("all")
                        })
                    })
                });
                let Some(pattern) = rule.text("pattern").filter(|p| !p.is_empty()) else {
                    continue;
                };
                if !platforms_ok {
                    continue;
                }
                parsed.rules.push(SaveRule {
                    root: rule
                        .text("root")
                        .filter(|r| !r.is_empty())
                        .map(str::to_owned),
                    path: rule.text("path").unwrap_or("").to_owned(),
                    pattern: pattern.to_owned(),
                    recursive: rule.text("recursive").is_some_and(|v| v != "0"),
                });
            }
        }
        if let Some(overrides) = ufs.child("rootoverrides") {
            for node in numbered(overrides) {
                let (Some(os_name), Some(root), Some(instead)) =
                    (node.text("os"), node.text("root"), node.text("useinstead"))
                else {
                    continue;
                };
                let transforms = node
                    .child("pathtransforms")
                    .map(|t| {
                        numbered(t)
                            .into_iter()
                            .filter_map(|t| {
                                Some((t.text("find")?.to_owned(), t.text("replace")?.to_owned()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                parsed.overrides.push(RootOverride {
                    os: os_name.to_owned(),
                    negate: node.text("oscompare") == Some("!="),
                    root: root.to_owned(),
                    use_instead: instead.to_owned(),
                    transforms,
                    addpath: node
                        .text("addpath")
                        .unwrap_or("")
                        .trim_matches('/')
                        .to_owned(),
                });
            }
        }
        Some(parsed)
    }

    fn override_for(&self, os: Os, token: &str) -> Option<&RootOverride> {
        self.overrides
            .iter()
            .find(|o| o.root == token && o.os.eq_ignore_ascii_case(os.name()) != o.negate)
    }
}

/// Base directories for each root token on this machine.
#[derive(Clone, Debug)]
pub struct Roots {
    pub os: Os,
    pub install_dir: PathBuf,
    /// Windows: the Proton prefix's `drive_c/users/steamuser`.
    pub windows_user: PathBuf,
    pub home: PathBuf,
    pub xdg_data: PathBuf,
    pub xdg_config: PathBuf,
    /// `<steam root>/userdata/<account id>/<app id>/remote`.
    pub remote: PathBuf,
    pub cloud_documents: Option<PathBuf>,
}

impl Roots {
    fn dir(&self, token: &str) -> Option<PathBuf> {
        match (self.os, token) {
            (_, "GameInstall") => Some(self.install_dir.clone()),
            (_, "SteamCloudDocuments") => self.cloud_documents.clone(),
            (Os::Windows, "WinMyDocuments") => Some(self.windows_user.join("Documents")),
            (Os::Windows, "WinAppDataLocal") => Some(self.windows_user.join("AppData/Local")),
            (Os::Windows, "WinAppDataLocalLow") => Some(self.windows_user.join("AppData/LocalLow")),
            (Os::Windows, "WinAppDataRoaming") => Some(self.windows_user.join("AppData/Roaming")),
            (Os::Windows, "WinSavedGames") => Some(self.windows_user.join("Saved Games")),
            (Os::Windows, "WinPictures") => Some(self.windows_user.join("Pictures")),
            (Os::Windows, "WinDesktop") => Some(self.windows_user.join("Desktop")),
            (Os::Windows, "WinMusic") => Some(self.windows_user.join("Music")),
            (Os::Windows, "WinVideos") => Some(self.windows_user.join("Videos")),
            (Os::Linux, "LinuxHome") => Some(self.home.clone()),
            (Os::Linux, "LinuxXdgDataHome") => Some(self.xdg_data.clone()),
            (Os::Linux, "LinuxXdgConfigHome") => Some(self.xdg_config.clone()),
            _ => None,
        }
    }
}

/// Why a cloud file cannot be mapped to a local path.
#[derive(Debug, Eq, PartialEq)]
pub enum Unresolved {
    Unsupported(String),
    Unsafe(String),
}

impl std::fmt::Display for Unresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "unsupported {what}"),
            Self::Unsafe(why) => write!(f, "unsafe path ({why})"),
        }
    }
}

/// A cloud path resolved against its root directory.
#[derive(Debug, Eq, PartialEq)]
pub struct Target {
    pub root_dir: PathBuf,
    pub rel: Vec<String>,
}

/// Splits `%Root%rest` into its token and the remainder; no token means unrooted.
pub fn split_name(name: &str) -> std::result::Result<(Option<&str>, &str), Unresolved> {
    let Some(body) = name.strip_prefix('%') else {
        return Ok((None, name));
    };
    let Some(end) = body.find('%') else {
        return Err(Unresolved::Unsafe("unterminated root token".into()));
    };
    let token = &body[..end];
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(Unresolved::Unsafe("invalid root token".into()));
    }
    Ok((Some(token), &body[end + 1..]))
}

fn components(rest: &str, allow_empty: bool) -> std::result::Result<Vec<String>, Unresolved> {
    let bad = |why: &str| Err(Unresolved::Unsafe(why.into()));
    if rest.is_empty() {
        return if allow_empty {
            Ok(Vec::new())
        } else {
            bad("empty file name")
        };
    }
    let mut parts = Vec::new();
    for part in rest.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return bad("empty, `.` or `..` component");
        }
        if part.contains(['\\', '\0']) || part.chars().any(char::is_control) {
            return bad("backslash or control character");
        }
        parts.push(part.to_owned());
    }
    Ok(parts)
}

/// Applies a root override's path transform on a component boundary.
fn transform(rest: &str, transforms: &[(String, String)]) -> String {
    for (find, replace) in transforms {
        let find = find.trim_matches('/');
        if find.is_empty() {
            continue;
        }
        if rest == find {
            return replace.trim_matches('/').to_owned();
        }
        if let Some(tail) = rest.strip_prefix(find).and_then(|t| t.strip_prefix('/')) {
            let replace = replace.trim_matches('/');
            return if replace.is_empty() {
                tail.to_owned()
            } else {
                format!("{replace}/{tail}")
            };
        }
    }
    rest.to_owned()
}

impl Roots {
    fn resolve(
        &self,
        ufs: &Ufs,
        token: Option<&str>,
        rest: &str,
        allow_empty: bool,
    ) -> std::result::Result<Target, Unresolved> {
        let (dir, rest) = match token {
            None => (self.remote.clone(), rest.to_owned()),
            Some(token) => match ufs.override_for(self.os, token) {
                Some(over) => {
                    let dir = self.dir(&over.use_instead).ok_or_else(|| {
                        Unresolved::Unsupported(format!("root {}", over.use_instead))
                    })?;
                    let rest = transform(rest, &over.transforms);
                    (
                        dir,
                        if over.addpath.is_empty() {
                            rest
                        } else if rest.is_empty() {
                            over.addpath.clone()
                        } else {
                            format!("{}/{rest}", over.addpath)
                        },
                    )
                }
                None => (
                    self.dir(token)
                        .ok_or_else(|| Unresolved::Unsupported(format!("root {token}")))?,
                    rest.to_owned(),
                ),
            },
        };
        Ok(Target {
            root_dir: dir,
            rel: components(&rest, allow_empty)?,
        })
    }

    /// Local location of a cloud file name.
    pub fn target(&self, ufs: &Ufs, name: &str) -> std::result::Result<Target, Unresolved> {
        let (token, rest) = split_name(name)?;
        self.resolve(ufs, token, rest, false)
    }
}

/// Walks `parts` below `root`, requiring every step to stay inside it. Missing
/// directories are created when `create`, otherwise the walk ends with `None`.
fn walk(root: &Path, parts: &[String], create: bool) -> Result<Option<PathBuf>> {
    let canonical_root = if create {
        fs::create_dir_all(root)?;
        root.canonicalize()?
    } else {
        match root.canonicalize() {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    };
    let mut current = canonical_root.clone();
    for part in parts {
        let next = current.join(part);
        match fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let real = next.canonicalize().map_err(|e| {
                    failure(format!("symlink {} is unresolved: {e}", next.display()))
                })?;
                if !real.starts_with(&canonical_root) || !real.is_dir() {
                    return Err(failure(format!(
                        "symlink {} leaves its cloud root",
                        next.display()
                    )));
                }
                current = real;
            }
            Ok(meta) if meta.is_dir() => current = next,
            Ok(_) => {
                return Err(failure(format!("{} is not a directory", next.display())));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                fs::create_dir(&next)?;
                fs::set_permissions(&next, fs::Permissions::from_mode(0o755))?;
                current = next;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(current))
}

/// The regular file at `target`, `None` when absent. With `create`, parent
/// directories are created and the (possibly new) path is always returned.
/// A symlink or directory at the final component is refused.
pub fn locate(target: &Target, create: bool) -> Result<Option<PathBuf>> {
    let (last, dirs) = target
        .rel
        .split_last()
        .ok_or_else(|| failure("empty cloud file name"))?;
    let Some(parent) = walk(&target.root_dir, dirs, create)? else {
        return Ok(None);
    };
    let path = parent.join(last);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(Some(path)),
        Ok(_) => Err(failure(format!(
            "{} is not a regular file; refusing to touch it",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(create.then_some(path)),
        Err(e) => Err(e.into()),
    }
}

/// Writes `bytes` next to `destination`, fsyncs, applies `mtime` (Unix seconds)
/// and renames into place. An existing file keeps its permission bits.
pub fn write_atomic(destination: &Path, bytes: &[u8], mtime: Option<u64>) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| failure("destination has no parent"))?;
    let mode = fs::metadata(destination)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o644);
    let temporary = parent.join(format!(
        ".steam-tui-cloud-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(bytes)?;
        if let Some(seconds) = mtime {
            let time = UNIX_EPOCH
                .checked_add(Duration::from_secs(seconds))
                .ok_or_else(|| failure("cloud timestamp is outside the supported range"))?;
            file.set_modified(time)?;
        }
        file.sync_all()?;
        fs::rename(&temporary, destination)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Case-insensitive glob with `*` and `?`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// A regular save file found by the scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFile {
    /// The cloud file name this file maps to.
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    /// Unix seconds.
    pub mtime: u64,
}

pub fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Finds local files matched by the app's save rules (plus the remote
/// directory). Symlinks are never followed; unresolved roots add a warning.
pub fn scan(roots: &Roots, ufs: &Ufs, warnings: &mut Vec<String>) -> Vec<LocalFile> {
    let mut rules = ufs.rules.clone();
    rules.push(SaveRule {
        root: None,
        path: String::new(),
        pattern: "*".into(),
        recursive: true,
    });
    let mut found: std::collections::BTreeMap<String, LocalFile> = Default::default();
    for rule in rules {
        let path = if rule.path == "." {
            String::new()
        } else {
            rule.path.trim_matches('/').to_owned()
        };
        let prefix = match (&rule.root, path.is_empty()) {
            (Some(root), true) => format!("%{root}%"),
            (Some(root), false) => format!("%{root}%{path}/"),
            (None, true) => String::new(),
            (None, false) => format!("{path}/"),
        };
        let target = match roots.resolve(ufs, rule.root.as_deref(), &path, true) {
            Ok(target) => target,
            Err(error) => {
                warnings.push(format!("save rule {prefix}{}: {error}", rule.pattern));
                continue;
            }
        };
        let base = match walk(&target.root_dir, &target.rel, false) {
            Ok(Some(base)) => base,
            Ok(None) => continue,
            Err(error) => {
                warnings.push(format!("save rule {prefix}{}: {error}", rule.pattern));
                continue;
            }
        };
        let mut stack = vec![(base, Vec::<String>::new())];
        while let Some((directory, rel)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if meta.file_type().is_symlink() || file_name.starts_with(".steam-tui-cloud-") {
                    continue;
                }
                let mut child = rel.clone();
                child.push(file_name.clone());
                if meta.is_dir() {
                    if rule.recursive && child.len() < MAX_SCAN_DEPTH {
                        stack.push((entry.path(), child));
                    }
                } else if meta.is_file() && glob_match(&rule.pattern, &file_name) {
                    let name = format!("{prefix}{}", child.join("/"));
                    if components(&child.join("/"), false).is_err() || found.len() >= MAX_SCAN_FILES
                    {
                        continue;
                    }
                    found.entry(name.clone()).or_insert(LocalFile {
                        name,
                        path: entry.path(),
                        size: meta.len(),
                        mtime: meta.modified().map_or(0, unix_seconds),
                    });
                }
            }
        }
    }
    found.into_values().collect()
}
