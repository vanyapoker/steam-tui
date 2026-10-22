use super::*;
use crate::install::tests::Temp;

fn intent(id: u32) -> Intent {
    Intent {
        app_id: id,
        dependency: None,
        shader: false,
        revision: None,
        steam_root: "/tmp/library".into(),
        destination: "/tmp/library".into(),
    }
}
#[test]
fn restart_dedup_priority_and_account_isolation() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "Account").unwrap();
    let one = q.enqueue(intent(1), false).unwrap();
    assert_eq!(q.enqueue(intent(1), false).unwrap(), one);
    let two = q.enqueue(intent(2), true).unwrap();
    let lease = q.lease().unwrap();
    assert!(q.lease().is_err());
    assert_eq!(q.claim(&lease, 100, false).unwrap().unwrap().id, two);
    drop(lease); // Simulated worker crash, before the next process starts.
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let lease = q.lease().unwrap();
    assert_eq!(q.claim(&lease, 100, false).unwrap().unwrap().id, two);
    assert!(
        Queue::in_directory(temp.path(), "different")
            .unwrap()
            .jobs()
            .unwrap()
            .is_empty()
    );
    assert!(Queue::in_directory(temp.path(), "../escape").is_err());
}
#[test]
fn windows_policies_game_activity_and_foreground_override() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let id = q.enqueue(intent(1), false).unwrap();
    let mut settings = q.settings().unwrap();
    settings.window = Some(Window {
        start: 1380,
        end: 120,
    });
    q.save_settings(settings.clone()).unwrap();
    let lease = q.lease().unwrap();
    assert!(q.claim(&lease, 600, false).unwrap().is_none());
    assert!(q.claim(&lease, 60, true).unwrap().is_none());
    settings.policies.insert(1, UpdatePolicy::LaunchOnly);
    q.save_settings(settings).unwrap();
    assert!(q.claim(&lease, 60, false).unwrap().is_none());
    q.enqueue(intent(1), true).unwrap();
    assert_eq!(q.claim(&lease, 600, true).unwrap().unwrap().id, id);
}
#[test]
fn controls_survive_scans_and_explicit_retry_clears_failure() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let id = q.enqueue(intent(1), false).unwrap();
    q.control(id, JobState::Paused).unwrap();
    q.enqueue(intent(1), false).unwrap();
    assert_eq!(q.job(id).unwrap().unwrap().state, JobState::Paused);
    q.finish(id, JobState::Failed, Some("network".into()))
        .unwrap();
    q.enqueue(intent(1), false).unwrap();
    assert_eq!(q.job(id).unwrap().unwrap().state, JobState::Failed);
    q.enqueue(intent(1), true).unwrap();
    assert_eq!(q.job(id).unwrap().unwrap().state, JobState::Pending);
    q.control(id, JobState::Cancelled).unwrap();
    q.enqueue(intent(1), false).unwrap();
    assert_eq!(q.job(id).unwrap().unwrap().state, JobState::Cancelled);
    assert_ne!(q.enqueue(intent(1), true).unwrap(), id);
}
#[test]
fn corrupt_foreign_and_symlink_state_is_never_overwritten() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    q.enqueue(intent(1), false).unwrap();
    let directory = temp.path().join("steam-tui/updates-account");
    let path = directory.join("jobs.json");
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"broken").unwrap();
    assert!(q.enqueue(intent(2), true).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"broken");
    std::fs::write(
        &path,
        String::from_utf8(original)
            .unwrap()
            .replace("\"account\":\"account\"", "\"account\":\"foreign\""),
    )
    .unwrap();
    assert!(q.jobs().is_err());
    std::fs::remove_file(&path).unwrap();
    let outside = temp.path().join("outside");
    std::fs::write(&outside, b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    assert!(q.enqueue(intent(1), true).is_err());
    assert_eq!(std::fs::read(outside).unwrap(), b"keep");
}

#[test]
fn active_runner_is_not_stolen_and_new_revisions_survive_previous_cancellation() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let mut old = intent(1);
    old.revision = Some([1; 20]);
    let id = q.enqueue(old.clone(), false).unwrap();
    let lease = q.lease().unwrap();
    q.claim(&lease, 1, false).unwrap();
    assert!(q.ready(1, false).unwrap().is_none());
    drop(lease);
    q.control(id, JobState::Cancelled).unwrap();
    assert_eq!(q.enqueue(old, false).unwrap(), id);
    let mut new = intent(1);
    new.revision = Some([2; 20]);
    let next = q.enqueue(new, false).unwrap();
    assert_ne!(id, next);
    assert_eq!(q.enqueue(intent(1), true).unwrap(), next);
    let mut settings = q.settings().unwrap();
    settings.priorities.insert(1, 42);
    q.save_settings(settings).unwrap();
    assert_eq!(q.job(next).unwrap().unwrap().priority, 42);
    assert!(!q.suspended(next, 1, true).unwrap()); // Explicit foreground work.
}
#[test]
fn suspension_tracks_game_activity_policy_and_window_without_persisting_auto_pause() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let id = q.enqueue(intent(1), false).unwrap();
    assert!(q.suspended(id, 1, true).unwrap());
    assert!(!q.suspended(id, 1, false).unwrap());
    let mut settings = q.settings().unwrap();
    settings.window = Some(Window {
        start: 60,
        end: 120,
    });
    q.save_settings(settings).unwrap();
    assert!(q.suspended(id, 1, false).unwrap());
    assert!(!q.suspended(id, 61, false).unwrap());
    assert_eq!(q.job(id).unwrap().unwrap().state, JobState::Pending);
    q.control(id, JobState::Paused).unwrap();
    assert!(q.suspended(id, 61, false).unwrap());
}

#[test]
fn dates_survive_resume_recovery_and_refresh_and_reset_on_retry() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "dates").unwrap();
    let id = q.enqueue(intent(1), false).unwrap();
    let created = q.job(id).unwrap().unwrap().created_at;
    assert!(created.is_some());
    let path = temp.path().join("steam-tui/updates-dates/jobs.json");
    // Fixed old dates distinguish preservation from rewriting within one second.
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state["jobs"][0]["started_at"] = 123.into();
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let lease = q.lease().unwrap();
    assert_eq!(
        q.claim(&lease, 1, false).unwrap().unwrap().started_at,
        Some(123)
    );
    q.control(id, JobState::Paused).unwrap();
    q.control(id, JobState::Paused).unwrap();
    q.control(id, JobState::Pending).unwrap();
    assert_eq!(
        q.claim(&lease, 1, false).unwrap().unwrap().started_at,
        Some(123)
    );
    drop(lease);
    let lease = q.lease().unwrap();
    assert_eq!(
        q.claim(&lease, 1, false).unwrap().unwrap().started_at,
        Some(123)
    );
    q.finish(id, JobState::Failed, Some("failure".into()))
        .unwrap();
    let failed = q.job(id).unwrap().unwrap();
    assert!(failed.finished_at.is_some());
    q.enqueue(intent(1), false).unwrap();
    q.save_settings(q.settings().unwrap()).unwrap();
    assert_eq!(q.job(id).unwrap().unwrap().finished_at, failed.finished_at);
    q.control(id, JobState::Pending).unwrap();
    let retry = q.job(id).unwrap().unwrap();
    assert_eq!(retry.created_at, created);
    assert_eq!((retry.started_at, retry.finished_at), (None, None));
    assert!(
        q.claim(&lease, 1, false)
            .unwrap()
            .unwrap()
            .started_at
            .is_some()
    );
    q.control(id, JobState::Cancelled).unwrap();
    let cancelled = q.job(id).unwrap().unwrap().finished_at;
    assert!(cancelled.is_some());
    q.control(id, JobState::Cancelled).unwrap();
    assert_eq!(q.job(id).unwrap().unwrap().finished_at, cancelled);
    assert!(q.control(id, JobState::Pending).is_err());
    drop(lease);
    q.control(id, JobState::Pending).unwrap();
    assert_eq!(q.job(id).unwrap().unwrap().finished_at, None);
    let lease = q.lease().unwrap();
    q.claim(&lease, 1, false).unwrap();
    q.finish(id, JobState::Complete, None).unwrap();
    assert!(q.job(id).unwrap().unwrap().finished_at.is_some());
}

#[test]
fn legacy_jobs_stay_undated_until_a_real_transition() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "legacy").unwrap();
    let id = q.enqueue(intent(1), false).unwrap();
    let path = temp.path().join("steam-tui/updates-legacy/jobs.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for field in ["created_at", "started_at", "finished_at"] {
        state["jobs"][0].as_object_mut().unwrap().remove(field);
    }
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    q.enqueue(intent(1), false).unwrap();
    q.control(id, JobState::Pending).unwrap();
    q.save_settings(q.settings().unwrap()).unwrap();
    let job = q.job(id).unwrap().unwrap();
    assert_eq!(
        (job.created_at, job.started_at, job.finished_at),
        (None, None, None)
    );
    q.control(id, JobState::Cancelled).unwrap();
    let job = q.job(id).unwrap().unwrap();
    assert_eq!((job.created_at, job.started_at), (None, None));
    assert!(job.finished_at.is_some());
}
