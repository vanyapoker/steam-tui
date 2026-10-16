use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "steam-tui-host-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn lock(&self) -> PathBuf {
        self.0.join("host.lock")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn account() -> HostAccount {
    HostAccount {
        name: "test_account".into(),
        steam_id: 76561198000000000,
    }
}
fn record() -> Record {
    Record {
        root: PathBuf::from("/fixture/Steam"),
        account: HostAccountRecord {
            name: account().name,
            steam_id: account().steam_id,
        },
        process: Process {
            pid: i32::MAX,
            start: 1,
        },
        token: None,
    }
}
#[test]
fn identity_requires_authenticated_exact_account_not_a_log_line() {
    assert!(
        validate(
            &account(),
            &probe::Status {
                steam_id: account().steam_id,
                logged_on: true
            }
        )
        .is_ok()
    );
    assert!(
        validate(
            &account(),
            &probe::Status {
                steam_id: account().steam_id,
                logged_on: false
            }
        )
        .is_err()
    );
    assert!(
        validate(
            &account(),
            &probe::Status {
                steam_id: account().steam_id + 1,
                logged_on: true
            }
        )
        .unwrap_err()
        .to_string()
        .contains("mismatch")
    );
}
#[test]
fn cross_process_style_shared_leases_prevent_exclusive_cleanup_until_last_holder() {
    let fixture = Fixture::new();
    let mut owner = open_lock(&fixture.lock()).unwrap();
    flock(&owner, libc::LOCK_EX).unwrap();
    write_record(&mut owner, &record()).unwrap();
    let next = open_lock(&fixture.lock()).unwrap();
    assert!(flock(&next, libc::LOCK_EX | libc::LOCK_NB).is_err());
    flock(&owner, libc::LOCK_SH).unwrap();
    flock(&next, libc::LOCK_SH | libc::LOCK_NB).unwrap();
    let contender = open_lock(&fixture.lock()).unwrap();
    assert!(flock(&contender, libc::LOCK_EX | libc::LOCK_NB).is_err());
    drop(owner);
    assert!(flock(&contender, libc::LOCK_EX | libc::LOCK_NB).is_err());
    drop(next);
    flock(&contender, libc::LOCK_EX | libc::LOCK_NB).unwrap();
}
#[test]
fn lease_error_unwinds_unlock_and_borrowed_or_stale_host_is_never_shutdown() {
    let fixture = Fixture::new();
    let mut file = open_lock(&fixture.lock()).unwrap();
    write_record(&mut file, &record()).unwrap();
    flock(&file, libc::LOCK_SH).unwrap();
    {
        let _lease = Lease {
            file,
            path: fixture.lock(),
        };
    }
    let mut file = open_lock(&fixture.lock()).unwrap();
    flock(&file, libc::LOCK_EX | libc::LOCK_NB).unwrap();
    assert!(read_record(&mut file).is_err());
    let mut stale = record();
    stale.token = Some("stale-test-token".into());
    shutdown_owned(&stale); // nonexistent PID cannot cause global Steam shutdown
}
#[test]
fn lock_record_is_bounded_and_symlinks_are_rejected() {
    let fixture = Fixture::new();
    let mut file = open_lock(&fixture.lock()).unwrap();
    write_record(&mut file, &record()).unwrap();
    assert_eq!(
        read_record(&mut file).unwrap().account.steam_id,
        account().steam_id
    );
    file.set_len(16385).unwrap();
    assert!(read_record(&mut file).is_err());
    let link = fixture.0.join("link");
    std::os::unix::fs::symlink(fixture.lock(), &link).unwrap();
    assert!(open_lock(&link).is_err());
}
#[test]
fn hosted_environment_preserves_lifetime_token_and_rejects_shim_hooks() {
    let mut plan = LaunchPlan {
        executable: "/game".into(),
        arguments: vec![],
        working_dir: "/".into(),
        environment: BTreeMap::from([
            (reaper::TOKEN_VAR.into(), "game-token".into()),
            ("STEAM_TUI_REMOTE_STORAGE".into(), "/remote".into()),
        ]),
        needs_steam: true,
    };
    client_env::prepare_hosted(&mut plan).unwrap();
    assert_eq!(
        plan.environment.get(reaper::TOKEN_VAR).unwrap(),
        "game-token"
    );
    assert!(!plan.environment.contains_key("STEAM_TUI_REMOTE_STORAGE"));
    assert!(!plan.arguments.iter().any(|arg| arg == "-insecure"));
    for argument in ["-insecure", "-INSECURE", "-insecure=1"] {
        let mut insecure = plan.clone();
        insecure.arguments.push(argument.into());
        assert!(
            client_env::prepare_hosted(&mut insecure)
                .unwrap_err()
                .to_string()
                .contains("cannot use -insecure")
        );
    }
    for key in [
        "LD_PRELOAD",
        "LD_AUDIT",
        "STEAM_TUI_BACKEND_SOCKET",
        "STEAM_TUI_CLIENT_LIB",
    ] {
        let mut hook = plan.clone();
        hook.environment.insert(key.into(), "/hook".into());
        assert!(client_env::prepare_hosted(&mut hook).is_err());
    }
}
#[test]
fn pid_reuse_and_unrelated_process_tokens_are_not_owned() {
    let process = Process::read(std::process::id() as i32).unwrap();
    assert!(process.alive());
    assert!(!process.tagged("unrelated-fixture-token"));
    assert!(
        !Process {
            pid: process.pid,
            start: process.start + 1
        }
        .alive()
    );
}

#[test]
fn helper_cleanup_signals_only_matching_owned_process_and_rejects_reused_pid() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn helper(token: Option<&str>) -> Child {
        let mut command = Command::new("python3");
        command.args(["-c", "import ctypes,time,sys; ctypes.CDLL(None).prctl(15,b'steamwebhelper',0,0,0); print('ready',flush=True); time.sleep(20)"]).stdout(Stdio::piped());
        command.env_remove("STEAM_TUI_HOST_TOKEN");
        if let Some(token) = token {
            command.env("STEAM_TUI_HOST_TOKEN", token);
        }
        let mut child = Child(command.spawn().unwrap());
        let mut ready = [0; 6];
        child
            .0
            .stdout
            .take()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        assert_eq!(&ready, b"ready\n");
        child
    }
    let token = reaper::new_token();
    let mut owned = helper(Some(&token));
    let mut unrelated = helper(None);
    let process = Process::read(owned.0.id() as i32).unwrap();
    assert_eq!(host_helpers(&token), vec![process.clone()]);
    Process {
        pid: process.pid,
        start: process.start + 1,
    }
    .signal(libc::SIGTERM);
    assert!(owned.0.try_wait().unwrap().is_none());
    process.signal(libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(2);
    while owned.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(owned.0.try_wait().unwrap().is_some());
    assert!(unrelated.0.try_wait().unwrap().is_none());
}
