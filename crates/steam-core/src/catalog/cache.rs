//! Last successful owned-catalog snapshot, one file per account:
//! `$XDG_CACHE_HOME/steam-tui/catalog-<account>.json` (`~/.cache` fallback).
//!
//! Stored: app IDs, names, install plans (depot/manifest IDs, sizes, build IDs)
//! and last-known update/blocked status. Not stored: tokens, license lists,
//! package IDs, local installs, play history or paths from the machine. The file
//! is written atomically (private mode, fsync, rename) and read with a byte limit,
//! a format version, an account match, a row limit and unique app IDs; anything
//! else is treated as absent rather than trusted.
use super::{InstallPlan, UpdateStatus};
use crate::installed::{Result, failure, read_bounded};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const VERSION: u32 = 1;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ROWS: usize = 100_000;
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Row {
    pub app_id: u32,
    pub name: String,
    pub plan: Option<InstallPlan>,
    pub update: UpdateStatus,
    pub reason: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    account: String,
    rows: Vec<Row>,
}

/// Steam account names are ASCII alphanumerics and `_`; anything that could
/// escape the cache directory or alias another account is rejected, not escaped.
fn file_name(account: &str) -> Option<String> {
    let valid = !account.is_empty()
        && account.len() <= 64
        && account
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !account.starts_with('.');
    valid.then(|| format!("catalog-{}.json", account.to_ascii_lowercase()))
}

pub(super) fn directory() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .ok_or_else(|| failure("Neither XDG_CACHE_HOME nor HOME is set"))?;
    if !root.is_absolute() {
        return Err(failure("Catalog cache root must be absolute"));
    }
    Ok(root.join("steam-tui"))
}

/// `None` for an absent, oversized, corrupt, foreign-account, wrong-version or
/// internally inconsistent file.
pub(super) fn read(dir: &Path, account: &str) -> Option<Vec<Row>> {
    let path = dir.join(file_name(account)?);
    let bytes = read_bounded(&path, MAX_BYTES).ok()?;
    let file: File = serde_json::from_slice(&bytes).ok()?;
    if file.version != VERSION
        || !file.account.eq_ignore_ascii_case(account)
        || file.rows.len() > MAX_ROWS
    {
        return None;
    }
    let mut seen = BTreeSet::new();
    for row in &file.rows {
        if row.app_id == 0
            || !seen.insert(row.app_id)
            || row
                .plan
                .as_ref()
                .is_some_and(|plan| plan.app_id != row.app_id)
        {
            return None;
        }
    }
    Some(file.rows)
}

/// Replace the account's cache with `rows`. For IDs in `carry` (apps whose
/// metadata could not be resolved this time) the previous cached row is kept when
/// one exists, so a transient PICS failure never overwrites a known plan.
pub(super) fn write(
    dir: &Path,
    account: &str,
    rows: Vec<Row>,
    carry: &BTreeSet<u32>,
) -> Result<()> {
    let name = file_name(account).ok_or_else(|| failure("Account name is not cacheable"))?;
    let mut rows: BTreeMap<u32, Row> = rows.into_iter().map(|row| (row.app_id, row)).collect();
    if !carry.is_empty()
        && let Some(old) = read(dir, account)
    {
        for row in old {
            if carry.contains(&row.app_id) && rows.contains_key(&row.app_id) {
                rows.insert(row.app_id, row);
            }
        }
    }
    let bytes = serde_json::to_vec(&File {
        version: VERSION,
        account: account.to_ascii_lowercase(),
        rows: rows.into_values().collect(),
    })?;
    if bytes.len() > MAX_BYTES {
        return Err(failure("Catalog cache exceeds its byte limit"));
    }
    fs::create_dir_all(dir)?;
    let temporary = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join(&name))?;
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
