use super::layout::{self, Os, Roots, Ufs, Unresolved, glob_match, split_name};
use super::net::compose_name;
use super::state::{CloudEntry, FileRecord, State, sha1_hex, state_file};
use super::*;
use crate::installed::{Node, text};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "steam-tui-cloud-unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const DOC: &str = r#"
"ufs"
{
    "quota" "10000"
    "maxnumfiles" "100"
    "savefiles"
    {
        "0" { "root" "WinAppDataLocal" "path" "DDSurvivors/Saved/SaveGames" "pattern" "*.sav" "recursive" "1" }
        "1" { "root" "WinMyDocuments" "path" "Game" "pattern" "*" "platforms" { "0" "MacOS" } }
    }
    "rootoverrides"
    {
        "0"
        {
            "os" "MacOS" "oscompare" "=" "root" "WinAppDataLocal" "useinstead" "MacAppSupport"
        }
        "1"
        {
            "os" "Linux" "oscompare" "=" "root" "WinAppDataLocal" "useinstead" "LinuxXdgDataHome"
            "pathtransforms" { "0" { "find" "DDSurvivors" "replace" "dd" } }
        }
        "2"
        {
            "os" "Linux" "root" "WinMyDocuments" "useinstead" "LinuxHome" "addpath" "x"
        }
    }
}
"#;

fn doc() -> Node {
    text(DOC).unwrap()
}

fn roots(base: &Path, os: Os) -> Roots {
    Roots {
        os,
        install_dir: base.join("game"),
        windows_user: base.join("pfx/drive_c/users/steamuser"),
        home: base.join("home"),
        xdg_data: base.join("home/.local/share"),
        xdg_config: base.join("home/.config"),
        remote: base.join("userdata/1/30/remote"),
        cloud_documents: Some(base.join("home/.SteamCloud/tester/Test Game")),
    }
}

fn app(base: &Path, os: Os) -> CloudApp {
    CloudApp {
        app_id: 30,
        account: "tester".into(),
        roots: roots(base, os),
        ufs: Ufs::parse(&doc(), os).unwrap(),
        state_dir: base.join("state"),
        build_id: 5,
    }
}

const NAME: &str = "%WinAppDataLocal%DDSurvivors/Saved/SaveGames/slot1.sav";

fn local_path(base: &Path, rel: &str) -> PathBuf {
    base.join("pfx/drive_c/users/steamuser/AppData/Local/DDSurvivors/Saved/SaveGames")
        .join(rel)
}

// ---- layout -------------------------------------------------------------

#[test]
fn ufs_rules_overrides_and_platform_filter() {
    let ufs = Ufs::parse(&doc(), Os::Windows).unwrap();
    assert_eq!((ufs.quota, ufs.max_files), (Some(10000), Some(100)));
    // The second rule is for MacOS only.
    assert_eq!(ufs.rules.len(), 1);
    assert_eq!(ufs.rules[0].root.as_deref(), Some("WinAppDataLocal"));
    assert_eq!(ufs.rules[0].pattern, "*.sav");
    assert!(ufs.rules[0].recursive);
    assert!(Ufs::parse(&text("\"common\" { \"name\" \"x\" }").unwrap(), Os::Windows).is_none());
}

#[test]
fn windows_roots_map_into_the_proton_prefix() {
    let base = Path::new("/lib");
    let r = roots(base, Os::Windows);
    let ufs = Ufs::default();
    let user = base.join("pfx/drive_c/users/steamuser");
    for (token, expected) in [
        ("WinMyDocuments", user.join("Documents")),
        ("WinAppDataLocal", user.join("AppData/Local")),
        ("WinAppDataLocalLow", user.join("AppData/LocalLow")),
        ("WinAppDataRoaming", user.join("AppData/Roaming")),
        ("WinSavedGames", user.join("Saved Games")),
        ("GameInstall", base.join("game")),
    ] {
        let target = r.target(&ufs, &format!("%{token}%a/b.sav")).unwrap();
        assert_eq!(target.root_dir, expected, "{token}");
        assert_eq!(target.rel, ["a", "b.sav"]);
    }
    let remote = r.target(&ufs, "plain.dat").unwrap();
    assert_eq!(remote.root_dir, base.join("userdata/1/30/remote"));
    assert_eq!(
        r.target(&ufs, "%LinuxHome%x").unwrap_err(),
        Unresolved::Unsupported("root LinuxHome".into())
    );
    assert!(matches!(
        r.target(&ufs, "%UnknownRoot%x").unwrap_err(),
        Unresolved::Unsupported(_)
    ));
}

#[test]
fn linux_roots_overrides_and_transforms() {
    let base = Path::new("/lib");
    let r = roots(base, Os::Linux);
    let ufs = Ufs::parse(&doc(), Os::Linux).unwrap();
    let target = r
        .target(&ufs, "%WinAppDataLocal%DDSurvivors/Saved/a.sav")
        .unwrap();
    assert_eq!(target.root_dir, base.join("home/.local/share"));
    assert_eq!(target.rel, ["dd", "Saved", "a.sav"]);
    // Only a whole leading component is transformed.
    let other = r.target(&ufs, "%WinAppDataLocal%DDSurvivorsX/a").unwrap();
    assert_eq!(other.rel, ["DDSurvivorsX", "a"]);
    let added = r.target(&ufs, "%WinMyDocuments%a").unwrap();
    assert_eq!(added.root_dir, base.join("home"));
    assert_eq!(added.rel, ["x", "a"]);
    assert_eq!(
        r.target(&ufs, "%LinuxXdgConfigHome%a/b").unwrap().root_dir,
        base.join("home/.config")
    );
    // Windows-only tokens do not exist for a native game without an override.
    assert!(r.target(&ufs, "%WinSavedGames%a").is_err());
}

#[test]
fn unsafe_names_are_rejected() {
    let r = roots(Path::new("/lib"), Os::Windows);
    let ufs = Ufs::default();
    for name in [
        "%WinAppDataLocal%../escape",
        "%WinAppDataLocal%a/../../escape",
        "../escape",
        "/absolute",
        "%WinAppDataLocal%/absolute",
        "%WinAppDataLocal%a//b",
        "%WinAppDataLocal%a/./b",
        "%WinAppDataLocal%a\\b",
        "%WinAppDataLocal%",
        "%WinAppDataLocal",
        "%%x",
        "%Win-Bad%x",
        "",
        "a/b\u{1b}c",
    ] {
        assert!(
            matches!(r.target(&ufs, name), Err(Unresolved::Unsafe(_))),
            "{name:?} -> {:?}",
            r.target(&ufs, name)
        );
    }
    assert_eq!(split_name("%A1%b").unwrap(), (Some("A1"), "b"));
    assert_eq!(split_name("b").unwrap(), (None, "b"));
}

#[test]
fn names_compose_from_path_prefixes() {
    let prefixes: Vec<String> = vec![
        String::new(),
        "%WinAppDataLocal%DDSurvivors/Saved/".into(),
        "%WinAppDataLocal%DDSurvivors".into(),
        "%GameInstall%".into(),
    ];
    assert_eq!(compose_name(&prefixes, Some(0), "x.dat"), "x.dat");
    assert_eq!(
        compose_name(&prefixes, Some(1), "slot.sav"),
        "%WinAppDataLocal%DDSurvivors/Saved/slot.sav"
    );
    assert_eq!(
        compose_name(&prefixes, Some(2), "slot.sav"),
        "%WinAppDataLocal%DDSurvivors/slot.sav"
    );
    assert_eq!(compose_name(&prefixes, Some(3), "s"), "%GameInstall%s");
    assert_eq!(compose_name(&prefixes, None, "plain.dat"), "plain.dat");
    assert_eq!(compose_name(&prefixes, Some(9), "plain.dat"), "plain.dat");
    assert_eq!(
        compose_name(&prefixes, Some(1), "%GameInstall%full"),
        "%GameInstall%full"
    );
}

#[test]
fn glob_is_case_insensitive_with_star_and_question() {
    assert!(glob_match("*.sav", "Slot1.SAV"));
    assert!(glob_match("*", "anything"));
    assert!(glob_match("save?.dat", "save1.dat"));
    assert!(!glob_match("save?.dat", "save12.dat"));
    assert!(glob_match("a*b*c", "aXXbYYc"));
    assert!(!glob_match("*.sav", "slot.sav.bak"));
    assert!(glob_match("*", ""));
}

#[test]
fn symlinks_may_not_leave_their_root() {
    let temp = Temp::new();
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    fs::create_dir_all(root.join("real")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("victim"), b"secret").unwrap();
    symlink(&outside, root.join("escape")).unwrap();
    symlink(root.join("real"), root.join("inside")).unwrap();
    symlink(temp.path().join("missing"), root.join("dangling")).unwrap();
    let target = |rel: &[&str]| layout::Target {
        root_dir: root.clone(),
        rel: rel.iter().map(|s| (*s).to_owned()).collect(),
    };
    assert!(layout::locate(&target(&["escape", "victim"]), false).is_err());
    assert!(layout::locate(&target(&["escape", "new"]), true).is_err());
    assert!(layout::locate(&target(&["dangling", "x"]), true).is_err());
    assert!(!outside.join("new").exists());
    let inside = layout::locate(&target(&["inside", "f"]), true)
        .unwrap()
        .unwrap();
    assert!(inside.starts_with(root.join("real")));
    // A symlink or directory in the final position is never touched.
    symlink(outside.join("victim"), root.join("real/link")).unwrap();
    fs::create_dir(root.join("real/dir")).unwrap();
    assert!(layout::locate(&target(&["real", "link"]), true).is_err());
    assert!(layout::locate(&target(&["real", "dir"]), false).is_err());
    // A file where a directory is needed.
    fs::write(root.join("file"), b"x").unwrap();
    assert!(layout::locate(&target(&["file", "x"]), true).is_err());
    // Absent paths are reported as absent without creating anything.
    assert_eq!(layout::locate(&target(&["a", "b"]), false).unwrap(), None);
    assert!(!root.join("a").exists());
}

#[test]
fn atomic_writes_set_mtime_keep_mode_and_leave_no_temporary() {
    let temp = Temp::new();
    let path = temp.path().join("save.dat");
    layout::write_atomic(&path, b"one", Some(1_600_000_000)).unwrap();
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(
        layout::unix_seconds(meta.modified().unwrap()),
        1_600_000_000
    );
    assert_eq!(meta.permissions().mode() & 0o777, 0o644);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    layout::write_atomic(&path, b"two", None).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"two");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

// ---- state --------------------------------------------------------------

#[test]
fn state_round_trips_privately_and_tolerates_bad_files() {
    let temp = Temp::new();
    let path = state_file(&temp.path().join("cloud"), "we ird/acct", 30);
    assert_eq!(path.file_name().unwrap(), "we_ird_acct-30.json");
    assert_eq!(State::load(&path).unwrap(), State::default());
    let mut state = State {
        change_number: 42,
        ..State::default()
    };
    state.cloud.insert(
        NAME.into(),
        CloudEntry {
            sha1: sha1_hex(b"x"),
            time: 7,
            size: 1,
        },
    );
    state.files.insert(
        NAME.into(),
        FileRecord {
            sha1: sha1_hex(b"x"),
            time: 7,
            size: 1,
        },
    );
    state.save(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let loaded = State::load(&path).unwrap();
    assert_eq!(
        (loaded.change_number, loaded.cloud, loaded.files),
        (42, state.cloud, state.files)
    );
    fs::write(&path, b"{ not json").unwrap();
    assert!(State::load(&path).is_err());
}

// ---- decisions ----------------------------------------------------------

fn entry(content: &[u8], time: u64) -> CloudEntry {
    CloudEntry {
        sha1: sha1_hex(content),
        time,
        size: content.len() as u64,
    }
}
fn record(content: &[u8]) -> FileRecord {
    FileRecord {
        sha1: sha1_hex(content),
        time: 0,
        size: content.len() as u64,
    }
}
fn local(content: &[u8], mtime: u64) -> Local {
    Local {
        sha1: sha1_hex(content),
        mtime,
        size: content.len() as u64,
    }
}

#[test]
fn download_decisions() {
    let cloud = entry(b"new", 100);
    assert_eq!(decide_down(&cloud, None, None), Down::Missing);
    assert_eq!(
        decide_down(&cloud, Some(&local(b"new", 1)), None),
        Down::Identical
    );
    // Unchanged since the last sync: the cloud copy is newer.
    assert_eq!(
        decide_down(&cloud, Some(&local(b"old", 999)), Some(&record(b"old"))),
        Down::Overwrite
    );
    // Edited locally since the last sync: conflict, whatever the timestamps say.
    assert_eq!(
        decide_down(&cloud, Some(&local(b"mine", 1)), Some(&record(b"old"))),
        Down::Conflict
    );
    // Never synced: keep both copies regardless of timestamps.
    assert_eq!(
        decide_down(&cloud, Some(&local(b"old", 50)), None),
        Down::Conflict
    );
    assert_eq!(
        decide_down(&cloud, Some(&local(b"old", 100)), None),
        Down::Conflict
    );
    assert_eq!(
        decide_down(&cloud, Some(&local(b"old", 200)), None),
        Down::Conflict
    );
}

#[test]
fn upload_decisions() {
    let sha = |c: &[u8]| sha1_hex(c);
    // Identical to the cloud.
    assert_eq!(
        decide_up(&sha(b"a"), None, Some(&entry(b"a", 1))),
        Up::Unchanged
    );
    // New local file.
    assert_eq!(decide_up(&sha(b"a"), None, None), Up::Upload);
    // Untouched since the sync (the cloud may be newer: that is a download).
    assert_eq!(
        decide_up(&sha(b"a"), Some(&record(b"a")), Some(&entry(b"b", 1))),
        Up::Unchanged
    );
    // Edited locally, cloud still as synced.
    assert_eq!(
        decide_up(&sha(b"mine"), Some(&record(b"a")), Some(&entry(b"a", 1))),
        Up::Upload
    );
    // Edited on both sides.
    assert_eq!(
        decide_up(
            &sha(b"mine"),
            Some(&record(b"a")),
            Some(&entry(b"theirs", 1))
        ),
        Up::Conflict
    );
    // Never synced but the cloud already has something else.
    assert_eq!(
        decide_up(&sha(b"mine"), None, Some(&entry(b"theirs", 1))),
        Up::Conflict
    );
    // Deleted in the cloud since the sync; local still has edits.
    assert_eq!(
        decide_up(&sha(b"mine"), Some(&record(b"a")), None),
        Up::Upload
    );
}

#[test]
fn quota_counts_files_and_replaced_bytes() {
    let ufs = Ufs {
        quota: Some(100),
        max_files: Some(3),
        ..Ufs::default()
    };
    assert!(within_quota(&ufs, 2, 50, 1, 50, 0));
    assert!(!within_quota(&ufs, 3, 50, 1, 10, 0));
    assert!(!within_quota(&ufs, 1, 50, 1, 51, 0));
    assert!(
        within_quota(&ufs, 3, 90, 0, 60, 50),
        "replacing frees its old size"
    );
    assert!(within_quota(&Ufs::default(), 1000, 1 << 40, 1, 1 << 40, 0));
}

// ---- download decoding --------------------------------------------------

fn fetched(body: &[u8], raw: &[u8]) -> Fetched {
    Fetched {
        file_size: body.len() as u64,
        raw_file_size: raw.len() as u64,
        sha1: Some(sha1_hex(raw)),
        time: 1,
        explicit_delete: false,
        encrypted: false,
        body: body.to_vec(),
    }
}

fn zipped(content: &[u8]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file(
            "save",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
    writer.write_all(content).unwrap();
    writer.finish().unwrap().into_inner()
}

#[test]
fn downloads_are_decompressed_and_sha_verified() {
    let content = b"save data ".repeat(100);
    let sha = sha1_hex(&content);
    assert_eq!(
        decode_download(&fetched(&content, &content), &sha).unwrap(),
        content
    );
    let zip = zipped(&content);
    assert!(zip.len() < content.len());
    assert_eq!(
        decode_download(&fetched(&zip, &content), &sha).unwrap(),
        content
    );
    // Wrong hash, wrong announced size, tampered body.
    assert!(decode_download(&fetched(&content, &content), &sha1_hex(b"other")).is_err());
    let mut bad = fetched(&content, &content);
    bad.file_size += 1;
    assert!(decode_download(&bad, &sha).is_err());
    let mut flipped = content.clone();
    flipped[0] ^= 1;
    let mut tampered = fetched(&flipped, &content);
    tampered.sha1 = Some(sha.clone());
    assert!(decode_download(&tampered, &sha).is_err());
    // The server's own hash must agree too.
    let mut disagreeing = fetched(&content, &content);
    disagreeing.sha1 = Some(sha1_hex(b"x"));
    assert!(decode_download(&disagreeing, &sha).is_err());
    // A zip that inflates beyond its announced raw size is refused.
    let mut lying = fetched(&zip, &content);
    lying.raw_file_size = 10;
    assert!(decode_download(&lying, &sha).is_err());
    assert!(decode_download(&fetched(b"not a zip at all", &content), &sha).is_err());
}

// ---- flows with a fake service ------------------------------------------

/// Content, timestamp and platform mask of a fake cloud file.
type Stored = (Vec<u8>, u64, Option<u32>);
#[derive(Default)]
struct Fake {
    /// name -> (content, time, platforms)
    files: RefCell<BTreeMap<String, Stored>>,
    deleted: RefCell<Vec<String>>,
    compress: bool,
    tamper: bool,
    fail_upload: RefCell<Vec<String>>,
    fail_begin: bool,
    next_batch: Cell<u64>,
    completed: RefCell<Vec<(u64, bool)>>,
    uploads: RefCell<Vec<String>>,
    deletions: RefCell<Vec<String>>,
    fetches: Cell<u32>,
    mutate_on_fetch: Option<PathBuf>,
    fail_complete: bool,
    batch_change: Option<u64>,
}

impl Fake {
    fn with(files: &[(&str, &[u8], u64)]) -> Self {
        let fake = Self::default();
        for (name, content, time) in files {
            fake.files
                .borrow_mut()
                .insert((*name).into(), (content.to_vec(), *time, None));
        }
        fake
    }
}

impl CloudApi for Fake {
    async fn changelist(&self, _app_id: u32, _since: u64) -> Result<Changelist> {
        let files = self
            .files
            .borrow()
            .iter()
            .map(|(name, (content, time, platforms))| RemoteFile {
                name: name.clone(),
                sha1: Some(sha1_hex(content)),
                time: *time,
                size: content.len() as u64,
                persisted: true,
                deleted: false,
                platforms: *platforms,
            })
            .chain(self.deleted.borrow().iter().map(|name| RemoteFile {
                name: name.clone(),
                sha1: Some(sha1_hex(b"gone")),
                time: 1,
                size: 4,
                persisted: false,
                deleted: true,
                platforms: None,
            }))
            .collect();
        Ok(Changelist {
            current: 9,
            delta: false,
            files,
        })
    }

    async fn fetch(&self, _app_id: u32, name: &str) -> Result<Fetched> {
        self.fetches.set(self.fetches.get() + 1);
        if let Some(path) = &self.mutate_on_fetch {
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, b"written while downloading")?;
        }
        let files = self.files.borrow();
        let (content, time, _) = files.get(name).ok_or_else(|| failure("no such file"))?;
        let mut body = if self.compress {
            zipped(content)
        } else {
            content.clone()
        };
        if self.tamper {
            body[0] ^= 0xff;
        }
        let mut served = fetched(&body, content);
        served.time = *time;
        Ok(served)
    }

    async fn begin_batch(
        &self,
        _app_id: u32,
        upload: &[String],
        delete: &[String],
        _build_id: u64,
    ) -> Result<Batch> {
        if self.fail_begin {
            return Err(failure("begin failed"));
        }
        self.uploads.borrow_mut().extend(upload.iter().cloned());
        self.deletions.borrow_mut().extend(delete.iter().cloned());
        self.next_batch.set(self.next_batch.get() + 1);
        Ok(Batch {
            id: 100 + self.next_batch.get(),
            change_number: Some(self.batch_change.unwrap_or(10)),
        })
    }

    async fn upload(&self, file: &UploadFile<'_>) -> Result<bool> {
        if self.fail_upload.borrow().iter().any(|n| n == file.name) {
            return Err(failure("transfer failed"));
        }
        assert_eq!(sha1_hex(file.bytes), hex(&file.sha1));
        self.files
            .borrow_mut()
            .insert(file.name.into(), (file.bytes.to_vec(), file.time, None));
        Ok(true)
    }

    async fn delete(&self, _app_id: u32, _batch: u64, name: &str) -> Result<()> {
        self.files.borrow_mut().remove(name);
        Ok(())
    }

    async fn complete_batch(&self, _app_id: u32, batch_id: u64, success: bool) -> Result<()> {
        self.completed.borrow_mut().push((batch_id, success));
        if self.fail_complete {
            return Err(failure("completion failed"));
        }
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    super::state::hex(bytes)
}

async fn down(fake: &Fake, app: &CloudApp) -> SyncReport {
    sync_down(fake, app, &mut |_| {}).await.unwrap()
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn first_sync_downloads_into_the_prefix_and_later_syncs_are_quiet() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[
            (NAME, b"slot one", 1_700_000_000),
            (
                "%WinAppDataLocal%DDSurvivors/Saved/SaveGames/sub/slot2.sav",
                b"two",
                5,
            ),
            ("plain.dat", b"remote file", 6),
        ]);
        let mut messages = Vec::new();
        let report = sync_down(&fake, &app, &mut |m| messages.push(m))
            .await
            .unwrap();
        assert_eq!(report.downloaded.len(), 3, "{report:?}");
        assert!(report.failed.is_empty() && report.conflicts.is_empty());
        assert!(messages.iter().any(|m| m.contains("downloading 3 files")));
        let slot = local_path(temp.path(), "slot1.sav");
        assert_eq!(fs::read(&slot).unwrap(), b"slot one");
        assert_eq!(
            layout::unix_seconds(fs::metadata(&slot).unwrap().modified().unwrap()),
            1_700_000_000
        );
        assert_eq!(
            fs::read(local_path(temp.path(), "sub/slot2.sav")).unwrap(),
            b"two"
        );
        assert_eq!(
            fs::read(temp.path().join("userdata/1/30/remote/plain.dat")).unwrap(),
            b"remote file"
        );
        assert_eq!(report.summary(), "Steam Cloud: downloaded 3 files");
        let state = State::load(&state_file(&app.state_dir, "tester", 30)).unwrap();
        assert_eq!(
            (state.change_number, state.cloud.len(), state.files.len()),
            (9, 3, 3)
        );
        // Nothing changed: nothing happens, nothing is fetched.
        let fetches = fake.fetches.get();
        let again = down(&fake, &app).await;
        assert!(again.downloaded.is_empty() && again.conflicts.is_empty());
        assert_eq!((again.unchanged, fake.fetches.get()), (3, fetches));
        assert_eq!(again.summary(), "Steam Cloud: up to date");
    });
}

#[test]
fn compressed_downloads_work_and_sha_mismatches_never_reach_disk() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let compressed = Fake {
            compress: true,
            ..Fake::with(&[(NAME, &b"x".repeat(500), 1)])
        };
        assert_eq!(down(&compressed, &app).await.downloaded.len(), 1);
        assert_eq!(
            fs::read(local_path(temp.path(), "slot1.sav")).unwrap(),
            b"x".repeat(500)
        );

        let other = Temp::new();
        let app = super::tests::app(other.path(), Os::Windows);
        let tampered = Fake {
            tamper: true,
            ..Fake::with(&[(NAME, b"payload", 1)])
        };
        let report = down(&tampered, &app).await;
        assert!(report.downloaded.is_empty());
        assert!(report.failed[0].contains("SHA1 mismatch"), "{report:?}");
        assert!(!local_path(other.path(), "slot1.sav").exists());
        assert!(report.summary().contains("1 failed"));
    });
}

#[test]
fn cloud_newer_than_untouched_local_overwrites_after_a_backup() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[(NAME, b"v1", 10)]);
        down(&fake, &app).await;
        fake.files
            .borrow_mut()
            .insert(NAME.into(), (b"v2".to_vec(), 20, None));
        let report = down(&fake, &app).await;
        assert_eq!(report.downloaded, [NAME]);
        let slot = local_path(temp.path(), "slot1.sav");
        assert_eq!(fs::read(&slot).unwrap(), b"v2");
        assert_eq!(report.backups.len(), 1);
        let backup = &report.backups[0];
        assert!(
            backup.starts_with(app.state_dir.join("backup/30")),
            "{backup:?}"
        );
        assert!(
            backup.ends_with("overwritten/WinAppDataLocal/DDSurvivors/Saved/SaveGames/slot1.sav")
        );
        assert_eq!(fs::read(backup).unwrap(), b"v1");
    });
}

#[test]
fn local_changes_are_never_overwritten() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[(NAME, b"v1", 10)]);
        down(&fake, &app).await;
        let slot = local_path(temp.path(), "slot1.sav");
        fs::write(&slot, b"my progress").unwrap();
        fake.files
            .borrow_mut()
            .insert(NAME.into(), (b"v2".to_vec(), 20, None));
        let report = down(&fake, &app).await;
        assert_eq!(report.conflicts, [NAME]);
        assert!(report.downloaded.is_empty() && report.backups.is_empty());
        assert_eq!(fs::read(&slot).unwrap(), b"my progress");
        assert!(report.summary().contains("1 conflict kept local"));

        // A pre-existing save without a baseline is a conflict, whatever its age.
        for cloud_time in [u64::MAX / 2, 1] {
            let other = Temp::new();
            let app = super::tests::app(other.path(), Os::Windows);
            let slot = local_path(other.path(), "slot1.sav");
            fs::create_dir_all(slot.parent().unwrap()).unwrap();
            fs::write(&slot, b"local").unwrap();
            let cloud = Fake::with(&[(NAME, b"cloud", cloud_time)]);
            let report = down(&cloud, &app).await;
            assert_eq!(report.conflicts, [NAME]);
            assert!(report.downloaded.is_empty());
            assert_eq!(fs::read(&slot).unwrap(), b"local");
        }
    });
}

#[test]
fn unsupported_other_platform_deleted_and_unsafe_files_are_not_written() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[
            ("%UnknownRoot%shot.png", b"a", 1),
            ("%WinAppDataLocal%../../escape.sav", b"b", 1),
            (NAME, b"ok", 1),
            ("%WinAppDataLocal%linux-only.sav", b"c", 1),
        ]);
        fake.files
            .borrow_mut()
            .get_mut("%WinAppDataLocal%linux-only.sav")
            .unwrap()
            .2 = Some(8);
        fake.deleted
            .borrow_mut()
            .push("%WinAppDataLocal%deleted.sav".into());
        let report = down(&fake, &app).await;
        assert_eq!(report.downloaded, [NAME]);
        assert_eq!(report.skipped.len(), 2, "{:?}", report.skipped);
        assert!(
            report
                .skipped
                .iter()
                .any(|s| s.contains("unsupported root UnknownRoot"))
        );
        assert!(report.skipped.iter().any(|s| s.contains("unsafe path")));
        assert!(!temp.path().join("escape.sav").exists());
        assert!(
            !temp
                .path()
                .join("pfx/drive_c/users/steamuser/AppData/Local/linux-only.sav")
                .exists()
        );
        assert!(
            !temp
                .path()
                .join("pfx/drive_c/users/steamuser/AppData/Local/deleted.sav")
                .exists()
        );
    });
}

#[test]
fn no_cloud_files_and_summary_texts() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let report = down(&Fake::default(), &app).await;
        assert_eq!(report.summary(), "Steam Cloud: no cloud files");
        let report = SyncReport {
            uploaded: vec!["a".into()],
            conflicts: vec!["b".into(), "c".into()],
            pending_upload: 1,
            ..SyncReport::default()
        };
        assert_eq!(
            report.summary(),
            "Steam Cloud: uploaded 1 file · 2 conflicts kept local (changed in both places) · 1 local save not uploaded — press C"
        );
    });
}

fn write_local(base: &Path, rel: &str, content: &[u8]) -> PathBuf {
    let path = local_path(base, rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn download_alone_reports_pending_local_saves_but_never_uploads() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::default();
        write_local(temp.path(), "new.sav", b"fresh");
        let report = down(&fake, &app).await;
        assert_eq!(report.pending_upload, 1);
        assert!(fake.uploads.borrow().is_empty() && fake.completed.borrow().is_empty());
        assert!(fake.files.borrow().is_empty());
        assert!(
            report
                .summary()
                .contains("1 local save not uploaded — press C")
        );
    });
}

#[test]
fn upload_sends_new_and_changed_files_only_and_completes_the_batch() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[(NAME, b"v1", 10)]);
        down(&fake, &app).await;
        write_local(temp.path(), "slot1.sav", b"v1 edited");
        write_local(temp.path(), "new/deep.sav", b"new");
        // Outside every pattern: never uploaded.
        write_local(temp.path(), "notes.txt", b"private");
        fs::write(
            temp.path()
                .join("pfx/drive_c/users/steamuser/AppData/Local/other.sav"),
            b"elsewhere",
        )
        .unwrap();
        let report = sync_up(&fake, &app, false, &mut |_| {}).await.unwrap();
        let mut uploaded = report.uploaded.clone();
        uploaded.sort();
        assert_eq!(
            uploaded,
            [
                "%WinAppDataLocal%DDSurvivors/Saved/SaveGames/new/deep.sav",
                NAME
            ]
        );
        assert_eq!(fake.files.borrow()[NAME].0, b"v1 edited");
        assert_eq!(fake.files.borrow().len(), 2);
        assert_eq!(*fake.completed.borrow(), [(101, true)]);
        assert_eq!(report.summary(), "Steam Cloud: uploaded 2 files");
        // State knows the new baseline: the next sync has nothing to do.
        let again = sync_up(&fake, &app, false, &mut |_| {}).await.unwrap();
        assert!(again.uploaded.is_empty());
        assert_eq!(fake.completed.borrow().len(), 1, "no empty batch is opened");
    });
}

#[test]
fn a_failed_transfer_still_completes_the_batch_with_a_failure() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        write_local(temp.path(), "a.sav", b"a");
        write_local(temp.path(), "b.sav", b"b");
        let fake = Fake::default();
        fake.fail_upload
            .borrow_mut()
            .push("%WinAppDataLocal%DDSurvivors/Saved/SaveGames/a.sav".into());
        let report = sync_up(&fake, &app, false, &mut |_| {}).await.unwrap();
        assert_eq!(report.uploaded.len(), 1);
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].contains("transfer failed"));
        assert_eq!(*fake.completed.borrow(), [(101, false)]);
        // The failed file stays pending; the local file is untouched.
        assert_eq!(fs::read(local_path(temp.path(), "a.sav")).unwrap(), b"a");
        let retry = Fake::default();
        let report = sync_up(&retry, &app, false, &mut |_| {}).await.unwrap();
        assert_eq!(report.uploaded.len(), 1, "only the failed one is retried");
        assert_eq!(*retry.completed.borrow(), [(101, true)]);
    });
}

#[test]
fn a_batch_that_cannot_begin_uploads_nothing() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        write_local(temp.path(), "a.sav", b"a");
        let fake = Fake {
            fail_begin: true,
            ..Fake::default()
        };
        assert!(sync_up(&fake, &app, false, &mut |_| {}).await.is_err());
        assert!(fake.completed.borrow().is_empty() && fake.files.borrow().is_empty());
    });
}

#[test]
fn upload_conflicts_keep_both_versions_and_do_not_overwrite_the_cloud() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[(NAME, b"v1", 10)]);
        down(&fake, &app).await;
        let slot = write_local(temp.path(), "slot1.sav", b"mine");
        fake.files
            .borrow_mut()
            .insert(NAME.into(), (b"theirs".to_vec(), 20, None));
        let report = sync_up(&fake, &app, false, &mut |_| {}).await.unwrap();
        assert_eq!(report.conflicts, [NAME]);
        assert!(report.uploaded.is_empty());
        assert!(fake.completed.borrow().is_empty(), "nothing to batch");
        assert_eq!(fake.files.borrow()[NAME].0, b"theirs");
        assert_eq!(fs::read(&slot).unwrap(), b"mine");
        assert_eq!(report.backups.len(), 1);
        assert!(
            report.backups[0]
                .to_string_lossy()
                .contains("cloud-conflict")
        );
        assert_eq!(fs::read(&report.backups[0]).unwrap(), b"theirs");
    });
}

#[test]
fn quota_skips_files_with_a_warning_instead_of_failing() {
    block_on(async {
        let temp = Temp::new();
        let mut app = app(temp.path(), Os::Windows);
        app.ufs.quota = Some(10);
        write_local(temp.path(), "a.sav", b"12345");
        write_local(temp.path(), "b.sav", b"123456");
        let fake = Fake::default();
        let report = sync_up(&fake, &app, false, &mut |_| {}).await.unwrap();
        assert_eq!(report.uploaded.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].contains("quota"), "{report:?}");
        app.ufs.quota = None;
        app.ufs.max_files = Some(1);
        let more = sync_up(&fake, &app, false, &mut |_| {}).await.unwrap();
        assert!(more.uploaded.is_empty());
        assert!(more.skipped[0].contains("quota"));
    });
}

#[test]
fn deletions_propagate_only_when_allowed_and_only_for_synced_files() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[(NAME, b"v1", 10)]);
        down(&fake, &app).await;
        fs::remove_file(local_path(temp.path(), "slot1.sav")).unwrap();
        let kept = sync_up(&fake, &app, false, &mut |_| {}).await.unwrap();
        assert!(kept.deleted_in_cloud.is_empty() && fake.files.borrow().contains_key(NAME));
        let removed = sync_up(&fake, &app, true, &mut |_| {}).await.unwrap();
        assert_eq!(removed.deleted_in_cloud, [NAME]);
        assert!(fake.files.borrow().is_empty());
        assert_eq!(*fake.completed.borrow(), [(101, true)]);
        // A download after that does not resurrect it.
        assert!(down(&fake, &app).await.downloaded.is_empty());
    });
}

#[test]
fn full_sync_downloads_then_uploads_and_never_touches_symlinked_saves() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[(
            "%WinAppDataLocal%DDSurvivors/Saved/SaveGames/cloud.sav",
            b"c",
            3,
        )]);
        write_local(temp.path(), "local.sav", b"l");
        // A symlinked save file must not be read or uploaded.
        let secret = temp.path().join("secret.sav");
        fs::write(&secret, b"top secret").unwrap();
        symlink(&secret, local_path(temp.path(), "link.sav")).unwrap();
        let report = sync_full(&fake, &app, &mut |_| {}).await.unwrap();
        assert_eq!(report.downloaded.len(), 1);
        assert_eq!(report.uploaded.len(), 1);
        assert!(!fake.files.borrow().keys().any(|k| k.contains("link.sav")));
        assert_eq!(report.pending_upload, 0);
    });
}

#[test]
fn native_linux_games_use_xdg_roots_and_overrides() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Linux);
        let fake = Fake::with(&[(NAME, b"linux save", 1)]);
        let report = down(&fake, &app).await;
        assert_eq!(report.downloaded, [NAME], "{report:?}");
        assert_eq!(
            fs::read(
                temp.path()
                    .join("home/.local/share/dd/Saved/SaveGames/slot1.sav")
            )
            .unwrap(),
            b"linux save"
        );
    });
}

// ---- library badge -------------------------------------------------------

fn lf(name: &str, content: &[u8], mtime: u64) -> layout::LocalFile {
    layout::LocalFile {
        name: name.into(),
        path: PathBuf::from(name),
        size: content.len() as u64,
        mtime,
    }
}

fn synced_state(content: &[u8], time: u64) -> State {
    let mut state = State::default();
    state.cloud.insert(NAME.into(), entry(content, time));
    state.files.insert(
        NAME.into(),
        FileRecord {
            sha1: sha1_hex(content),
            time,
            size: content.len() as u64,
        },
    );
    state
}

#[test]
fn badge_derivation_uses_stat_first_and_hashes_only_stat_mismatches() {
    let never = |_: &layout::LocalFile| -> Option<String> { panic!("must not hash") };
    assert_eq!(
        derive_badge(None, &[lf(NAME, b"a", 1)], &mut { never }),
        CloudBadge::Never
    );
    let state = synced_state(b"abc", 10);
    // Same size and mtime as the record: no read at all.
    assert_eq!(
        derive_badge(Some(&state), &[lf(NAME, b"abc", 10)], &mut { never }),
        CloudBadge::Synced
    );
    // No local files: nothing pending.
    assert_eq!(
        derive_badge(Some(&state), &[], &mut { never }),
        CloudBadge::Synced
    );
    // mtime differs but content is the recorded one: hashed, still synced.
    let mut hashed = 0;
    let mut same = |_: &layout::LocalFile| {
        hashed += 1;
        Some(sha1_hex(b"abc"))
    };
    assert_eq!(
        derive_badge(Some(&state), &[lf(NAME, b"abc", 99)], &mut same),
        CloudBadge::Synced
    );
    assert_eq!(hashed, 1);
    // Edited content, a new file, and an unreadable one are all pending.
    let mut edited = |_: &layout::LocalFile| Some(sha1_hex(b"mine"));
    assert_eq!(
        derive_badge(Some(&state), &[lf(NAME, b"mine", 99)], &mut edited),
        CloudBadge::Pending
    );
    let mut fresh = |_: &layout::LocalFile| Some(sha1_hex(b"new"));
    assert_eq!(
        derive_badge(
            Some(&state),
            &[lf("%WinAppDataLocal%other.sav", b"new", 5)],
            &mut fresh
        ),
        CloudBadge::Pending
    );
    let mut unreadable = |_: &layout::LocalFile| None;
    assert_eq!(
        derive_badge(Some(&state), &[lf(NAME, b"abd", 99)], &mut unreadable),
        CloudBadge::Pending
    );
    // Local equals the cloud even though the record is stale: nothing to upload.
    let mut stale = synced_state(b"old", 10);
    stale.cloud.insert(NAME.into(), entry(b"cloud", 20));
    let mut equals_cloud = |_: &layout::LocalFile| Some(sha1_hex(b"cloud"));
    assert_eq!(
        derive_badge(Some(&stale), &[lf(NAME, b"cloud", 30)], &mut equals_cloud),
        CloudBadge::Synced
    );
}

#[test]
fn badge_for_reflects_state_file_presence_and_local_edits() {
    let temp = Temp::new();
    let base = temp.path();
    // HOME/XDG drive the state directory; this test builds the app by hand
    // instead, so it exercises the same building blocks `badge_for` composes.
    let app = app(base, Os::Windows);
    assert!(!app.state_path().is_file());
    block_on(async {
        let fake = Fake::with(&[(NAME, b"v1", 10)]);
        down(&fake, &app).await;
    });
    let state = State::load(&app.state_path()).unwrap();
    let mut warnings = Vec::new();
    let locals = layout::scan(&app.roots, &app.ufs, &mut warnings);
    let mut read = |f: &layout::LocalFile| fs::read(&f.path).ok().map(|b| sha1_hex(&b));
    assert_eq!(
        derive_badge(Some(&state), &locals, &mut read),
        CloudBadge::Synced
    );
    write_local(base, "slot1.sav", b"edited");
    let locals = layout::scan(&app.roots, &app.ufs, &mut warnings);
    assert_eq!(
        derive_badge(Some(&state), &locals, &mut read),
        CloudBadge::Pending
    );
}

#[test]
fn full_sync_uploads_local_only_edits_without_reporting_a_conflict() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[(NAME, b"v1", 10)]);
        down(&fake, &app).await;
        write_local(temp.path(), "slot1.sav", b"local progress");
        let report = sync_full(&fake, &app, &mut |_| {}).await.unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(report.uploaded, [NAME]);
        assert_eq!(fake.files.borrow()[NAME].0, b"local progress");
    });
}

#[test]
fn unreadable_baseline_stops_sync_and_preserves_original_state() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        fs::create_dir_all(&app.state_dir).unwrap();
        fs::write(app.state_path(), b"broken state").unwrap();
        let fake = Fake::with(&[(NAME, b"remote", 10)]);
        write_local(temp.path(), "slot1.sav", b"my progress");
        assert!(sync_full(&fake, &app, &mut |_| {}).await.is_err());
        assert_eq!(fs::read(app.state_path()).unwrap(), b"broken state");
        assert_eq!(fake.fetches.get(), 0);
        assert!(fake.uploads.borrow().is_empty());
        assert_eq!(
            fs::read(local_path(temp.path(), "slot1.sav")).unwrap(),
            b"my progress"
        );
    });
}

#[test]
fn changed_or_newly_created_save_during_fetch_is_never_replaced() {
    block_on(async {
        for existing in [false, true] {
            let temp = Temp::new();
            let app = app(temp.path(), Os::Windows);
            let mut fake = Fake::with(&[(NAME, b"remote", 10)]);
            if existing {
                down(&fake, &app).await;
                fake.files
                    .borrow_mut()
                    .insert(NAME.into(), (b"new remote".to_vec(), 20, None));
            }
            fake.mutate_on_fetch = Some(local_path(temp.path(), "slot1.sav"));
            let report = down(&fake, &app).await;
            assert_eq!(report.failed.len(), 1);
            assert!(report.downloaded.is_empty());
            assert_eq!(
                fs::read(fake.mutate_on_fetch.as_ref().unwrap()).unwrap(),
                b"written while downloading"
            );
        }
    });
}

#[test]
fn incomplete_batch_does_not_advance_the_baseline_or_claim_success() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let mut fake = Fake::with(&[(NAME, b"v1", 10)]);
        down(&fake, &app).await;
        write_local(temp.path(), "slot1.sav", b"local progress");
        fake.fail_complete = true;
        let report = sync_up(&fake, &app, false, &mut |_| {}).await.unwrap();
        assert!(report.uploaded.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert!(report.summary().contains("completion failed"));
        let state = State::load(&app.state_path()).unwrap();
        assert_eq!(state.change_number, 0);
        assert_eq!(state.files[NAME].sha1, sha1_hex(b"v1"));
        // If the server did commit, the retry reconciles by hash without rewriting it.
        fake.fail_complete = false;
        let again = sync_full(&fake, &app, &mut |_| {}).await.unwrap();
        assert!(again.conflicts.is_empty() && again.failed.is_empty());
        assert_eq!(
            State::load(&app.state_path()).unwrap().files[NAME].sha1,
            sha1_hex(b"local progress")
        );
    });
}

#[test]
fn simultaneous_sync_is_refused_and_lock_releases_on_drop() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let lock = app.lock().unwrap();
        assert!(
            sync_full(&Fake::default(), &app, &mut |_| {})
                .await
                .is_err()
        );
        drop(lock);
        assert!(sync_full(&Fake::default(), &app, &mut |_| {}).await.is_ok());
    });
}

#[test]
fn encrypted_cloud_download_uses_valve_iv_format_and_validates_key_and_hash() {
    // Independent OpenSSL AES-256 ECB(IV) + CBC(PKCS7 plaintext) vector.
    let body =
        steam_ipc::hex_decode("c9528ec420a9fd241307b37f43a891e1dcd2869364a5c562eb4d0cd98343d707")
            .unwrap();
    let key = [0x42u8; 32];
    let plain = net::decrypt_cloud(&body, &key, crc32fast::hash(&key)).unwrap();
    assert_eq!(plain, b"cloud save");
    assert_eq!(
        decode_download(&fetched(&plain, &plain), &sha1_hex(b"cloud save")).unwrap(),
        plain
    );
    assert!(net::decrypt_cloud(&body, &key, 0).is_err());
    assert!(net::decrypt_cloud(&body, &[1; 31], 0).is_err());
    assert!(net::decrypt_cloud(&body[..15], &key, crc32fast::hash(&key)).is_err());
    let mut encrypted = fetched(&body, b"cloud save");
    encrypted.encrypted = true;
    assert!(decode_download(&encrypted, &sha1_hex(b"cloud save")).is_err());
}

#[test]
fn account_placeholders_addpath_and_dot_rules_map_and_scan_symmetrically() {
    let temp = Temp::new();
    let roots = roots(temp.path(), Os::Linux);
    let doc = text(r#""ufs" {
        "savefiles" { "0" { "root" "WinMyDocuments" "path" "." "pattern" "*.sav" } }
        "rootoverrides" { "0" { "root" "WinMyDocuments" "useinstead" "LinuxHome" "os" "Linux" "addpath" "game/{Steam3AccountID}/{64BitSteamID}" } }
    }"#).unwrap();
    let mut ufs = Ufs::parse(&doc, Os::Linux).unwrap();
    ufs.expand_account(42);
    let target = roots.target(&ufs, "%WinMyDocuments%slot.sav").unwrap();
    assert_eq!(target.rel, ["game", "42", "76561197960265770", "slot.sav"]);
    let path = layout::locate(&target, true).unwrap().unwrap();
    layout::write_atomic(&path, b"save", None).unwrap();
    let files = layout::scan(&roots, &ufs, &mut Vec::new());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "%WinMyDocuments%slot.sav");
    // Override addpath cannot escape the original root through a symlink.
    let external = temp.path().join("outside");
    fs::create_dir_all(&external).unwrap();
    fs::remove_dir_all(roots.home.join("game")).unwrap();
    symlink(&external, roots.home.join("game")).unwrap();
    assert!(layout::locate(&target, true).is_err());
}

#[test]
fn native_mode_migration_copies_missing_saves_and_backs_up_replaced_baselines() {
    let temp = Temp::new();
    let mut app = app(temp.path(), Os::Linux);
    let old_home = temp.path().join("old-home");
    let old_save = old_home.join("game/save.sav");
    fs::create_dir_all(old_save.parent().unwrap()).unwrap();
    fs::write(&old_save, b"v1").unwrap();
    let name = "%LinuxHome%game/save.sav";
    app.ufs.rules = vec![layout::SaveRule {
        root: Some("LinuxHome".into()),
        path: "game".into(),
        pattern: "*.sav".into(),
        recursive: true,
    }];
    let mut state = State {
        home_root: Some(old_home.clone()),
        ..Default::default()
    };
    let mut report = SyncReport::default();
    migrate_home(&app, &mut state, &mut report).unwrap();
    let path = app.roots.home.join("game/save.sav");
    assert_eq!(fs::read(&path).unwrap(), b"v1");
    assert_eq!(fs::read(&old_save).unwrap(), b"v1");
    state.files.insert(name.into(), record(b"v1"));
    state.home_root = Some(old_home.clone());
    fs::write(&old_save, b"v2").unwrap();
    migrate_home(&app, &mut state, &mut report).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"v2");
    assert_eq!(fs::read(&report.backups[0]).unwrap(), b"v1");
    state.home_root = Some(old_home);
    fs::write(&old_save, b"other").unwrap();
    fs::write(&path, b"mine").unwrap();
    migrate_home(&app, &mut state, &mut report).unwrap();
    assert!(!report.warnings.is_empty());
    assert_eq!(fs::read(&path).unwrap(), b"mine");
}

#[test]
fn game_storage_delete_and_forget_intents_are_synced_and_never_resurrected() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[
            ("slot.dat", b"v1", 10),
            ("forgot.dat", b"retained locally", 10),
        ]);
        down(&fake, &app).await;
        let store = steam_ipc::storage::Store::open(&app.roots.remote).unwrap();
        let mut meta = store.metadata().unwrap();
        meta.deleted.insert("slot.dat".into());
        meta.forgotten.insert("forgot.dat".into());
        store.save_metadata(&meta).unwrap();
        store.delete("slot.dat").unwrap();
        let report = sync_full(&fake, &app, &mut |_| {}).await.unwrap();
        assert_eq!(report.deleted_in_cloud.len(), 2);
        assert!(!app.roots.remote.join("slot.dat").exists());
        assert_eq!(store.read("forgot.dat").unwrap(), b"retained locally");
        assert!(
            !fake.files.borrow().contains_key("slot.dat")
                && !fake.files.borrow().contains_key("forgot.dat")
        );
        assert!(
            sync_full(&fake, &app, &mut |_| {})
                .await
                .unwrap()
                .uploaded
                .is_empty()
        );
    });
}

#[test]
fn conflict_resolution_keeps_the_chosen_version_and_backs_up_the_other() {
    block_on(async {
        for choice in [Choice::Cloud, Choice::Local] {
            let temp = Temp::new();
            let app = app(temp.path(), Os::Windows);
            let fake = Fake::with(&[(NAME, b"remote", 10)]);
            let slot = write_local(temp.path(), "slot1.sav", b"local");
            down(&fake, &app).await;
            let state = State::load(&app.state_path()).unwrap();
            let expected = conflict::inspect(&app, &state, NAME).unwrap();
            let report = conflict::resolve(&fake, &app, &expected, choice)
                .await
                .unwrap();
            let wanted = if choice == Choice::Cloud {
                b"remote".as_slice()
            } else {
                b"local".as_slice()
            };
            assert_eq!(fs::read(&slot).unwrap(), wanted);
            assert_eq!(fake.files.borrow()[NAME].0, wanted);
            assert_eq!(report.backups.len(), 1);
            assert_eq!(
                fs::read(&report.backups[0]).unwrap(),
                if choice == Choice::Cloud {
                    b"local".as_slice()
                } else {
                    b"remote".as_slice()
                }
            );
        }
    });
}

#[test]
fn stale_conflict_choices_never_overwrite_either_version() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[(NAME, b"remote", 10)]);
        let slot = write_local(temp.path(), "slot1.sav", b"local");
        down(&fake, &app).await;
        let expected =
            conflict::inspect(&app, &State::load(&app.state_path()).unwrap(), NAME).unwrap();
        fs::write(&slot, b"new progress").unwrap();
        assert!(
            conflict::resolve(&fake, &app, &expected, Choice::Cloud)
                .await
                .is_err()
        );
        assert!(fake.completed.borrow().is_empty());
        assert_eq!(fs::read(&slot).unwrap(), b"new progress");
        assert_eq!(fake.files.borrow()[NAME].0, b"remote");
    });
}

#[test]
fn interrupted_batches_are_closed_before_any_further_sync() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::default();
        let mut state = State {
            pending_batch: Some(12345),
            ..Default::default()
        };
        state.save(&app.state_path()).unwrap();
        refresh_listing(&fake, &app, &mut state).await.unwrap();
        assert_eq!(*fake.completed.borrow(), [(12345, false)]);
        assert!(state.pending_batch.is_none());
        assert!(batch_matches(
            Batch {
                id: 1,
                change_number: Some(10)
            },
            9
        ));
        assert!(!batch_matches(
            Batch {
                id: 1,
                change_number: Some(11)
            },
            9
        ));
        assert!(!batch_matches(
            Batch {
                id: 1,
                change_number: None
            },
            u64::MAX
        ));
    });
}

#[test]
fn cloud_deletion_backs_up_unchanged_local_and_requires_choice_for_local_edits() {
    block_on(async {
        for edited in [false, true] {
            let temp = Temp::new();
            let app = app(temp.path(), Os::Windows);
            let fake = Fake::with(&[(NAME, b"v1", 10)]);
            down(&fake, &app).await;
            let slot = local_path(temp.path(), "slot1.sav");
            if edited {
                fs::write(&slot, b"local progress").unwrap();
            }
            fake.files.borrow_mut().clear();
            fake.deleted.borrow_mut().push(NAME.into());
            let report = sync_full(&fake, &app, &mut |_| {}).await.unwrap();
            assert!(
                fake.uploads.borrow().is_empty(),
                "remote deletion must not resurrect an edited local file without a choice"
            );
            if edited {
                assert_eq!(report.conflicts, [NAME]);
                assert!(slot.exists());
                let expected =
                    conflict::inspect(&app, &State::load(&app.state_path()).unwrap(), NAME)
                        .unwrap();
                assert!(expected.cloud.is_none());
                let report = conflict::resolve(&fake, &app, &expected, Choice::Cloud)
                    .await
                    .unwrap();
                assert_eq!(report.deleted_locally, [NAME]);
                assert_eq!(fs::read(&report.backups[0]).unwrap(), b"local progress");
            } else {
                assert_eq!(report.deleted_locally, [NAME]);
                assert_eq!(fs::read(&report.backups[0]).unwrap(), b"v1");
            }
            assert!(!slot.exists());
        }
    });
}

#[test]
fn remote_changes_before_batch_creation_cancel_the_batch_without_uploading() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake {
            batch_change: Some(11),
            ..Fake::default()
        };
        write_local(temp.path(), "new.sav", b"local progress");
        assert!(sync_up(&fake, &app, false, &mut |_| {}).await.is_err());
        assert!(fake.files.borrow().is_empty());
        assert_eq!(*fake.completed.borrow(), [(101, false)]);
        assert!(
            State::load(&app.state_path())
                .unwrap()
                .pending_batch
                .is_none()
        );
    });
}

#[test]
fn encryption_roundtrip_uses_fresh_ivs_and_catches_tampered_plaintext_hash() {
    let key = [7u8; 32];
    let plain = b"private cloud save";
    let a = steam_vent_crypto::symmetric_encrypt_without_hmac(
        bytes::BytesMut::from(plain.as_slice()),
        &key,
    );
    let b = steam_vent_crypto::symmetric_encrypt_without_hmac(
        bytes::BytesMut::from(plain.as_slice()),
        &key,
    );
    assert_ne!(&a[..16], &b[..16]);
    let decoded = net::decrypt_cloud(&a, &key, crc32fast::hash(&key)).unwrap();
    assert_eq!(decoded, plain);
    assert!(decode_download(&fetched(&decoded, &decoded), &sha1_hex(b"different save")).is_err());
}

#[test]
fn manual_plain_file_deletion_requires_a_choice_and_keeps_a_remote_backup() {
    block_on(async {
        for choice in [Choice::Local, Choice::Cloud] {
            let temp = Temp::new();
            let app = app(temp.path(), Os::Windows);
            let fake = Fake::with(&[(NAME, b"v1", 10)]);
            down(&fake, &app).await;
            let path = local_path(temp.path(), "slot1.sav");
            fs::remove_file(&path).unwrap();
            let report = sync_full(&fake, &app, &mut |_| {}).await.unwrap();
            assert_eq!(report.conflicts, [NAME]);
            assert!(!path.exists());
            let expected =
                conflict::inspect(&app, &State::load(&app.state_path()).unwrap(), NAME).unwrap();
            let report = conflict::resolve(&fake, &app, &expected, choice)
                .await
                .unwrap();
            if choice == Choice::Local {
                assert!(!path.exists() && !fake.files.borrow().contains_key(NAME));
                assert_eq!(fs::read(&report.backups[0]).unwrap(), b"v1");
            } else {
                assert_eq!(fs::read(&path).unwrap(), b"v1");
            }
        }
    });
}

#[test]
fn identical_game_api_rewrites_become_persisted_and_disabled_cloud_never_uploads() {
    block_on(async {
        let temp = Temp::new();
        let app = app(temp.path(), Os::Windows);
        let fake = Fake::with(&[("slot.dat", b"v1", 10)]);
        down(&fake, &app).await;
        let store = steam_ipc::storage::Store::open(&app.roots.remote).unwrap();
        let mut meta = store.metadata().unwrap();
        meta.dirty.insert("slot.dat".into());
        store.save_metadata(&meta).unwrap();
        sync_full(&fake, &app, &mut |_| {}).await.unwrap();
        assert!(store.metadata().unwrap().dirty.is_empty());
        let mut meta = store.metadata().unwrap();
        meta.enabled = false;
        store.save_metadata(&meta).unwrap();
        store.write("slot.dat", b"offline progress").unwrap();
        let report = sync_full(&fake, &app, &mut |_| {}).await.unwrap();
        assert!(fake.uploads.borrow().is_empty());
        assert_eq!(fake.files.borrow()["slot.dat"].0, b"v1");
        assert_eq!(store.read("slot.dat").unwrap(), b"offline progress");
        assert_eq!(report.pending_upload, 0);
    });
}

#[test]
fn steam_cloud_documents_uses_account_and_display_name_and_rejects_path_components() {
    let home = Path::new("/home/tester");
    assert_eq!(
        cloud_documents(home, "alice", "Test Game").unwrap(),
        home.join(".SteamCloud/alice/Test Game")
    );
    assert!(cloud_documents(home, "../escape", "Test Game").is_none());
    assert!(cloud_documents(home, "alice", "../escape").is_none());
    let roots = roots(Path::new("/tmp/test-root"), Os::Linux);
    let target = roots
        .target(&Ufs::default(), "%SteamCloudDocuments%save.dat")
        .unwrap();
    assert_eq!(
        target.root_dir,
        Path::new("/tmp/test-root/home/.SteamCloud/tester/Test Game")
    );
    assert_eq!(target.rel, ["save.dat"]);
}

#[test]
fn invalid_remote_timestamps_do_not_panic_or_replace_a_local_file() {
    let temp = Temp::new();
    let file = temp.path().join("save");
    fs::write(&file, b"progress").unwrap();
    assert!(layout::write_atomic(&file, b"cloud", Some(u64::MAX)).is_err());
    assert_eq!(fs::read(&file).unwrap(), b"progress");
}
