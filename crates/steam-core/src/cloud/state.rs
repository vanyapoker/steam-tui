//! Per-account, per-app sync state: the last change number, the known cloud
//! listing, and what each synced local file looked like right after sync (used to
//! tell local edits from cloud edits). Stored as mode-0600 JSON.

use crate::installed::{Result, failure};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE: AtomicU64 = AtomicU64::new(0);
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;

/// One file in the cloud listing; `sha1` is lowercase hex.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CloudEntry {
    pub sha1: String,
    pub time: u64,
    pub size: u64,
}

/// A local file as it was when it last matched the cloud.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    pub sha1: String,
    pub time: u64,
    pub size: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub change_number: u64,
    #[serde(default)]
    pub cloud: BTreeMap<String, CloudEntry>,
    #[serde(default)]
    pub files: BTreeMap<String, FileRecord>,
    #[serde(default)]
    pub pending_batch: Option<u64>,
    #[serde(default)]
    pub home_root: Option<PathBuf>,
    #[serde(default)]
    pub deleted_cloud: std::collections::BTreeSet<String>,
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha1_hex(bytes: &[u8]) -> String {
    hex(&Sha1::digest(bytes))
}

/// `<account>-<app>.json` with the account reduced to safe characters.
pub fn state_file(directory: &Path, account: &str, app_id: u32) -> PathBuf {
    let safe: String = account
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    directory.join(format!("{safe}-{app_id}.json"))
}

impl State {
    /// A missing file is a fresh state; an unreadable one is an error so the
    /// caller can warn and continue conservatively.
    pub fn load(path: &Path) -> Result<Self> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(failure("cloud state file is too large"));
        }
        let state: Self = serde_json::from_slice(&bytes)?;
        if state.version > 1 {
            return Err(failure("unsupported cloud state version"));
        }
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| failure("cloud state path has no parent"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let temporary = parent.join(format!(
            ".state-{}-{}.tmp",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(&Self {
                version: 1,
                ..self.clone()
            })?)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}
