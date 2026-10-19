use super::*;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use steam_ipc::{DlcInfo, Reply, Request, VERSION};

fn metadata(disabled: bool) -> AppsSnapshot {
    AppsSnapshot {
        build_id: 123,
        branch: "test".into(),
        depots: vec![10, 20, 21],
        dlc: vec![
            DlcInfo {
                app_id: 481,
                name: "éDLC".into(),
                enabled: !disabled,
                installed: !disabled,
                depots: vec![20],
                downloading: true,
                downloaded: 12,
                total: 20,
                ..Default::default()
            },
            DlcInfo {
                app_id: 482,
                name: "Unowned".into(),
                enabled: true,
                installed: true,
                depots: vec![21],
                ..Default::default()
            },
            DlcInfo {
                app_id: 483,
                name: "Not installed".into(),
                enabled: true,
                ..Default::default()
            },
        ],
    }
}

unsafe fn exercise_api() {
    let this = ptr::null_mut();
    unsafe {
        assert_eq!(slot_10(this), 3);
        let (mut app, mut available) = (99, true);
        let mut name = [b'!'; 8];
        assert!(slot_11(
            this,
            0,
            &mut app,
            &mut available,
            name.as_mut_ptr().cast(),
            4
        ));
        assert_eq!(app, 481);
        assert!(!available); // Store visibility is not ownership.
        assert_eq!(&name[..4], &[0xc3, 0xa9, b'D', 0]);
        assert_eq!(name[4], b'!');
        assert!(slot_11(
            this,
            0,
            &mut app,
            &mut available,
            name.as_mut_ptr().cast(),
            2
        ));
        assert_eq!(name[0], 0); // UTF-8 is not split to fill a small buffer.
        for index in [-1, 3] {
            app = 99;
            available = true;
            name[0] = b'!';
            assert!(!slot_11(
                this,
                index,
                &mut app,
                &mut available,
                name.as_mut_ptr().cast(),
                8
            ));
            assert_eq!(app, 0);
            assert!(!available);
            assert_eq!(name[0], 0);
        }
        assert!(!slot_11(
            this,
            0,
            ptr::null_mut(),
            &mut available,
            name.as_mut_ptr().cast(),
            8
        ));
        assert!(slot_7(this, 481));
        assert!(!slot_7(this, 482)); // Installed files cannot grant ownership.
        assert!(!slot_7(this, 483));
        assert!(!slot_7(this, 999));
        let mut depots = [999; 4];
        assert_eq!(slot_17(this, 480, depots.as_mut_ptr(), 1), 1);
        assert_eq!(depots, [10, 999, 999, 999]);
        assert_eq!(slot_17(this, 481, depots.as_mut_ptr(), 4), 1);
        assert_eq!(depots[0], 20);
        assert_eq!(slot_17(this, 999, depots.as_mut_ptr(), 4), 0);
        assert_eq!(slot_17(this, 480, ptr::null_mut(), 4), 0);
        let (mut done, mut total) = (99, 99);
        assert!(slot_22(this, 481, &mut done, &mut total));
        assert_eq!((done, total), (12, 20));
        assert!(!slot_22(this, 999, &mut done, &mut total));
        assert_eq!((done, total), (0, 0));
        assert!(!slot_22(this, 481, ptr::null_mut(), &mut total));
        assert_eq!(slot_23(this), 123);
        assert!(slot_15(this, name.as_mut_ptr(), 8));
        assert_eq!(&name[..5], b"test\0");
        assert!(!slot_15(this, name.as_mut_ptr(), 1));
        assert_eq!(name[0], 0);
        slot_12(this, 481);
        assert!(slot_7(this, 481));
        slot_13(this, 481);
        assert!(!slot_7(this, 481)); // Mutation invalidates the metadata cache.
        slot_12(this, 999); // A backend error must not leave the old snapshot cached.
        assert_eq!(slot_10(this), 0);
        assert!(!slot_7(this, 481));
    }
}

#[test]
fn sdk_dlc_queries_enforce_ownership_buffer_bounds_and_mutation_invalidation() {
    const CHILD: &str = "STEAM_TUI_TEST_APPS";
    if std::env::var_os(CHILD).is_some() {
        unsafe {
            exercise_api();
        }
        return;
    }
    let directory = std::env::temp_dir().join(format!(
        "steam-tui-apps-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _directory = Cleanup(directory.clone());
    let socket = directory.join("backend.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "apps::tests::sdk_dlc_queries_enforce_ownership_buffer_bounds_and_mutation_invalidation"])
        .env(CHILD, "1").env("SteamAppId", "480").env("STEAM_TUI_BACKEND_SOCKET", socket)
        .stdin(Stdio::null()).spawn().unwrap();
    struct Reap(std::process::Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Reap(child);
    let mut disabled = false;
    let mut failed = false;
    let mut mutations = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "SDK child timed out");
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        let (mut stream, _) = match listener.accept() {
            Ok(stream) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(error) => panic!("{error}"),
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request: Request = steam_ipc::read_frame(&mut stream).unwrap();
        assert_eq!(request.app_id, 480);
        assert_eq!(request.version, VERSION);
        let response = match request.operation {
            Operation::Apps(AppsOp::Snapshot) if !failed => Response::Apps(metadata(disabled)),
            Operation::Apps(AppsOp::Snapshot) => Response::Error {
                message: "unavailable".into(),
            },
            Operation::AppOwnership { app_id } => Response::Ownership {
                owned: app_id == 481,
            },
            Operation::Apps(AppsOp::InstallDlc { app_id: 481 }) => {
                mutations.push(true);
                disabled = false;
                Response::AppsAccepted
            }
            Operation::Apps(AppsOp::UninstallDlc { app_id: 481 }) => {
                mutations.push(false);
                disabled = true;
                Response::AppsAccepted
            }
            Operation::Apps(AppsOp::InstallDlc { app_id: 999 }) => {
                failed = true;
                Response::Error {
                    message: "unrelated app".into(),
                }
            }
            other => panic!("unexpected operation: {other:?}"),
        };
        steam_ipc::write_frame(
            &mut stream,
            &Reply {
                version: VERSION,
                app_id: 480,
                response,
            },
        )
        .unwrap();
    }
    assert_eq!(mutations, [true, false]);
    assert!(failed);
}
