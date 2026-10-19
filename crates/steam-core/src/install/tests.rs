use super::*;
use crate::content::DepotChunk;
use std::fs;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Temp(PathBuf);
impl Temp {
    pub(crate) fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "steam-tui-install-unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn depot() -> DepotSpec {
    DepotSpec {
        depot_id: 10,
        manifest_id: 20,
        source_app_id: 30,
    }
}
pub(super) fn file(path: &str, bytes: &[u8]) -> DepotFile {
    DepotFile {
        path: path.into(),
        size: bytes.len() as u64,
        sha1: Some(hash_bytes(bytes)),
        flags: 0,
        chunks: if bytes.is_empty() {
            Vec::new()
        } else {
            vec![DepotChunk {
                id: hash_bytes(bytes),
                offset: 0,
                uncompressed_size: bytes.len() as u32,
                compressed_size: bytes.len() as u32,
                adler: content::adler32(bytes),
            }]
        },
        link_target: None,
    }
}
fn manifest(files: Vec<DepotFile>) -> DepotManifest {
    DepotManifest {
        depot_id: 10,
        manifest_id: 20,
        files,
    }
}

#[test]
fn destination_inspects_new_paths_without_creating_them() {
    let temp = Temp::new();
    let new = temp.path().join("not-created/nested/library");
    let info = destination(&new).unwrap();
    assert_eq!(info.path, new);
    assert!(info.available);
    assert!(info.free_bytes.is_some());
    assert!(!new.exists());
}

#[test]
fn live_dlc_permission_keeps_valve_client_guards_and_normal_updates_reject_games() {
    let temp = Temp::new();
    let processes = temp.path().join("proc");
    let process = processes.join("100");
    fs::create_dir_all(&process).unwrap();
    fs::write(process.join("comm"), "game\n").unwrap();
    fs::write(process.join("environ"), b"SteamAppId=30\0").unwrap();
    let install = temp.path().join("game");
    symlink(install.join("game.exe"), process.join("exe")).unwrap();
    assert!(filesystem::check_processes_in(temp.path(), 30, &install, false, &processes).is_err());
    assert!(filesystem::check_processes_in(temp.path(), 30, &install, true, &processes).is_ok());
    for name in ["steam", "steamwebhelper", "steam.exe"] {
        fs::write(process.join("comm"), name).unwrap();
        assert!(
            filesystem::check_processes_in(temp.path(), 30, &install, true, &processes).is_err()
        );
    }
    fs::write(process.join("comm"), "game").unwrap();
    fs::write(
        temp.path().join("steam.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    assert!(filesystem::check_processes_in(temp.path(), 30, &install, true, &processes).is_err());
}

#[test]
fn destination_rejects_non_directory_and_parent_traversal() {
    let temp = Temp::new();
    fs::write(temp.path().join("file"), b"x").unwrap();
    assert!(!destination(&temp.path().join("file")).unwrap().available);
    assert!(destination(&temp.path().join("../escape")).is_err());
}

#[test]
fn depot_paths_and_symlink_escapes_are_rejected() {
    for path in ["../escape", "/absolute", "", "dir/../../escape"] {
        assert!(
            Layout::new(
                vec![(depot(), manifest(vec![file(path, b"content")]))],
                vec![]
            )
            .is_err()
        );
    }
    let mut link = file("lib/link", b"");
    link.flags = 512;
    link.link_target = Some("../../escape".into());
    assert!(Layout::new(vec![(depot(), manifest(vec![link.clone()]))], vec![]).is_err());
    link.link_target = Some("../data/file".into());
    assert!(
        Layout::new(
            vec![(depot(), manifest(vec![link, file("data/file", b"data")]))],
            vec![]
        )
        .is_ok()
    );
}

#[test]
fn symlink_chains_are_safe_and_cycles_are_rejected() {
    let mut one = file("lib/one", b"");
    one.flags = 512;
    one.link_target = Some("two".into());
    let mut two = file("lib/two", b"");
    two.flags = 512;
    two.link_target = Some("actual".into());
    let actual = file("lib/actual", b"library");
    assert!(
        Layout::new(
            vec![(depot(), manifest(vec![one.clone(), two.clone(), actual]))],
            vec![]
        )
        .is_ok()
    );
    two.link_target = Some("one".into());
    assert!(Layout::new(vec![(depot(), manifest(vec![one, two]))], vec![]).is_err());
}

#[test]
fn cross_depot_symlink_parent_escape_is_rejected_after_expansion() {
    let mut root_link = file("a", b"");
    root_link.flags = 512;
    root_link.link_target = Some(".".into());
    let mut escape = file("b", b"");
    escape.flags = 512;
    escape.link_target = Some("a/../escape".into());
    assert!(
        Layout::new(
            vec![(
                depot(),
                manifest(vec![root_link.clone(), file("escape", b"safe")])
            )],
            vec![]
        )
        .is_ok()
    );
    assert!(
        Layout::new(
            vec![
                (depot(), manifest(vec![root_link])),
                (depot(), manifest(vec![escape, file("escape", b"safe")]))
            ],
            vec![]
        )
        .is_err()
    );
}

#[test]
fn incomplete_chunk_coverage_and_tree_collisions_are_rejected() {
    let mut broken = file("game", b"native binary");
    broken.chunks[0].offset = 1;
    assert!(Layout::new(vec![(depot(), manifest(vec![broken]))], vec![]).is_err());
    assert!(
        Layout::new(
            vec![(
                depot(),
                manifest(vec![file("data", b"a"), file("data/file", b"b")])
            )],
            vec![]
        )
        .is_err()
    );
}

#[test]
fn later_depot_overlay_has_one_verified_output() {
    let first = file("localized", b"old");
    let second = file("localized", b"translated");
    let layout = Layout::new(
        vec![
            (depot(), manifest(vec![first])),
            (depot(), manifest(vec![second])),
        ],
        vec![],
    )
    .unwrap();
    assert_eq!(layout.files.len(), 1);
    assert_eq!(layout.bytes, b"translated".len() as u64);
}

#[test]
fn staged_chunks_are_reused_only_after_hash_and_adler_verification() {
    let temp = Temp::new();
    let descriptor = file("game", b"verified");
    let chunk = &descriptor.chunks[0];
    let path = temp.path().join("game");
    fs::write(&path, b"tampered").unwrap();
    assert!(filesystem::read_chunk(&path, chunk, 0).unwrap().is_none());
    fs::write(&path, b"verified").unwrap();
    assert_eq!(
        filesystem::read_chunk(&path, chunk, 0).unwrap().unwrap(),
        b"verified"
    );
    let mut wrong = chunk.clone();
    wrong.adler ^= 1;
    assert!(filesystem::read_chunk(&path, &wrong, 0).unwrap().is_none());
}

#[test]
fn existing_symlink_ancestors_cannot_redirect_writes() {
    let temp = Temp::new();
    fs::create_dir(temp.path().join("outside")).unwrap();
    fs::create_dir(temp.path().join("base")).unwrap();
    symlink(temp.path().join("outside"), temp.path().join("base/link")).unwrap();
    assert!(filesystem::checked(&temp.path().join("base"), Path::new("link/file"), false).is_err());
    assert!(!temp.path().join("outside/file").exists());
}

#[test]
fn cancellation_and_disk_full_are_actionable_errors() {
    let token = CancellationToken::new();
    assert!(check_cancel(&token).is_ok());
    token.cancel();
    assert!(
        check_cancel(&token)
            .unwrap_err()
            .to_string()
            .contains("retained for resume")
    );
    let error = filesystem::disk_error(
        Path::new("staging/file"),
        std::io::Error::from_raw_os_error(libc::ENOSPC),
    );
    assert!(error.to_string().contains("Disk full"));
    assert!(error.to_string().contains("previous install is retained"));
}

#[test]
fn per_app_lock_is_exclusive_and_released() {
    let temp = Temp::new();
    let path = temp.path().join("app.lock");
    let first = filesystem::Lock::acquire(&path).unwrap();
    assert!(filesystem::Lock::acquire(&path).is_err());
    drop(first);
    assert!(filesystem::Lock::acquire(&path).is_ok());
}

#[tokio::test]
async fn unpaused_gate_never_waits() {
    let gate = PauseGate::default();
    assert!(!gate.is_paused());
    wait_while_paused(&gate, &CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn pause_blocks_until_resume() {
    let gate = PauseGate::default();
    let cancel = CancellationToken::new();
    gate.set(true);
    assert!(gate.clone().is_paused());
    let waiter = tokio::spawn({
        let (gate, cancel) = (gate.clone(), cancel.clone());
        async move { wait_while_paused(&gate, &cancel).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    gate.set(false);
    waiter.await.unwrap().unwrap();
    assert!(!cancel.is_cancelled());
}

#[tokio::test]
async fn cancel_releases_a_paused_gate_with_the_cancelled_error() {
    let gate = PauseGate::default();
    let cancel = CancellationToken::new();
    gate.set(true);
    let waiter = tokio::spawn({
        let (gate, cancel) = (gate.clone(), cancel.clone());
        async move { wait_while_paused(&gate, &cancel).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    cancel.cancel();
    let error = waiter.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert!(gate.is_paused());
}
