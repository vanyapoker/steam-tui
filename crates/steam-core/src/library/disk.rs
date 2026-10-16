//! Only public product documents are persisted; account licenses and tokens are not.
use crate::installed::{Node, Result, failure, read_bounded};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
fn directory() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .ok_or_else(|| failure("Neither XDG_CACHE_HOME nor HOME is set"))?;
    if !root.is_absolute() {
        return Err(failure("App metadata cache root must be absolute"));
    }
    Ok(root.join("steam-tui/appinfo"))
}

pub(crate) fn cached_app(app_id: u32) -> Result<Option<Node>> {
    let path = directory()?.join(format!("{app_id}.json"));
    let bytes = match read_bounded(&path, 16 * 1024 * 1024) {
        Ok(bytes) => bytes,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let document: Node = serde_json::from_slice(&bytes)?;
    validate(app_id, &document)?;
    Ok(Some(document))
}

fn validate(app_id: u32, document: &Node) -> Result<()> {
    if document.text("appid").and_then(|s| s.parse::<u32>().ok()) != Some(app_id) {
        return Err(failure("Cached PICS app ID mismatch"));
    }
    let mut stack = vec![(document, 0)];
    let mut count = 0;
    while let Some((node, depth)) = stack.pop() {
        count += 1;
        if depth > 64 || count > 500_000 {
            return Err(failure("Cached PICS document exceeds structural limits"));
        }
        stack.extend(node.children.values().map(|child| (child, depth + 1)));
    }
    Ok(())
}

/// Persist public documents. Unchanged documents (byte-identical to the file on
/// disk) are not rewritten or fsynced, so a repeated load costs reads rather than
/// hundreds of durable writes; the directory is fsynced once per batch.
pub(super) fn store<'a>(documents: impl IntoIterator<Item = (u32, &'a Node)>) -> Result<()> {
    let mut documents = documents.into_iter().peekable();
    if documents.peek().is_none() {
        return Ok(());
    }
    let root = directory()?;
    fs::create_dir_all(&root)?;
    let mut changed = false;
    for (app_id, document) in documents {
        validate(app_id, document)?;
        let path = root.join(format!("{app_id}.json"));
        let bytes = serde_json::to_vec(document)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(failure("Cached PICS document exceeds byte limit"));
        }
        if read_bounded(&path, 16 * 1024 * 1024).is_ok_and(|old| old == bytes) {
            continue;
        }
        let temporary = root.join(format!(
            ".{app_id}.{}.{}.tmp",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        changed = true;
    }
    if changed {
        fs::File::open(&root)?.sync_all()?;
    }
    Ok(())
}
