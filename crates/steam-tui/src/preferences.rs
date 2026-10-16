//! Only the account login name is stored here. Tokens stay in Secret Service.
use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
const MAX_BYTES: u64 = 4096;

pub(crate) fn valid_account(name: &str) -> bool {
    !name.is_empty()
        && name.is_ascii()
        && !name.starts_with('-')
        && !name.chars().any(|ch| ch.is_control() || ch.is_whitespace())
}

fn path() -> Result<PathBuf, Box<dyn Error>> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or("HOME or an absolute XDG_CONFIG_HOME is required for account preferences")?;
    Ok(config.join("steam-tui/account"))
}

pub(crate) fn latest() -> Result<Option<String>, Box<dyn Error>> {
    Ok(read_at(&path()?)?)
}

pub(crate) fn remember(account: &str) -> Result<(), Box<dyn Error>> {
    let path = path()?;
    remember_at(&path, account)
        .map_err(|error| format!("Cannot remember account in {}: {error}", path.display()).into())
}

fn read_at(path: &Path) -> io::Result<Option<String>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut name = String::new();
    file.take(MAX_BYTES + 1).read_to_string(&mut name)?;
    let account = name.trim();
    if name.len() as u64 > MAX_BYTES || !valid_account(account) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Invalid account preference in {}; replace it using --account NAME",
                path.display()
            ),
        ));
    }
    Ok(Some(account.to_ascii_lowercase()))
}

fn remember_at(path: &Path, account: &str) -> io::Result<()> {
    if !valid_account(account) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Steam account name",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("account preference needs a parent directory"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".account-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        writeln!(file, "{}", account.to_ascii_lowercase())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn settings_path() -> Option<PathBuf> {
    path()
        .ok()
        .map(|account| account.with_file_name("settings"))
}

pub(crate) fn load_settings() -> Option<String> {
    let file = File::open(settings_path()?).ok()?;
    let mut text = String::new();
    file.take(64 * 1024).read_to_string(&mut text).ok()?;
    Some(text)
}

/// Best effort: a failed save must never disturb the TUI.
pub(crate) fn save_settings(text: &str) {
    let Some(path) = settings_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = fs::create_dir_all(parent);
    let temporary = parent.join(format!(".settings-{}.tmp", std::process::id()));
    if fs::write(&temporary, text).is_ok() && fs::rename(&temporary, &path).is_err() {
        let _ = fs::remove_file(&temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(env::temp_dir().join(format!(
                "steam-tui-account-test-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            )))
        }
        fn account(&self) -> PathBuf {
            self.0.join("config/account")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn latest_successful_account_replaces_previous_selection() {
        let fixture = Fixture::new();
        let path = fixture.account();
        assert_eq!(read_at(&path).unwrap(), None);
        remember_at(&path, "FirstAccount").unwrap();
        assert_eq!(read_at(&path).unwrap().as_deref(), Some("firstaccount"));
        remember_at(&path, "SecondAccount").unwrap();
        assert_eq!(read_at(&path).unwrap().as_deref(), Some("secondaccount"));
        assert!(remember_at(&path, "bad\naccount").is_err());
        assert_eq!(read_at(&path).unwrap().as_deref(), Some("secondaccount"));
    }

    #[test]
    fn corrupt_or_oversized_preference_is_not_used_as_an_account() {
        let fixture = Fixture::new();
        let path = fixture.account();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for content in [
            "one\ntwo".to_owned(),
            String::new(),
            "x".repeat(MAX_BYTES as usize + 1),
        ] {
            fs::write(&path, content).unwrap();
            assert_eq!(
                read_at(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
