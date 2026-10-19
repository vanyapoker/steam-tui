use super::Intent;
use crate::install::filesystem::{self, Lock};
use crate::installed::{Result, failure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
const MAX_JOBS: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdatePolicy {
    #[default]
    Automatic,
    LaunchOnly,
    Manual,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub start: u16,
    pub end: u16,
}
impl Window {
    pub fn contains(self, minute: u16) -> bool {
        if self.start == self.end {
            true
        } else if self.start < self.end {
            (self.start..self.end).contains(&minute)
        } else {
            minute >= self.start || minute < self.end
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "enabled")]
    pub shaders_enabled: bool,
    #[serde(default = "shader_budget")]
    pub shader_archive_budget: u64,
    pub policies: BTreeMap<u32, UpdatePolicy>,
    #[serde(default)]
    pub selections: BTreeMap<u32, crate::catalog::ContentSelection>,
    pub priorities: BTreeMap<u32, i16>,
    pub window: Option<Window>,
    pub pause_during_games: bool,
}
fn shader_budget() -> u64 {
    8 * 1024 * 1024 * 1024
}
fn enabled() -> bool {
    true
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            shaders_enabled: true,
            shader_archive_budget: shader_budget(),
            selections: BTreeMap::new(),
            policies: BTreeMap::new(),
            priorities: BTreeMap::new(),
            window: None,
            pause_during_games: true,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Pending,
    Running,
    Paused,
    Failed,
    Complete,
    Cancelled,
}
impl JobState {
    fn terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub intent: Intent,
    pub state: JobState,
    pub foreground: bool,
    pub priority: i16,
    pub error: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct State {
    version: u32,
    account: String,
    next: u64,
    settings: Settings,
    jobs: Vec<Job>,
}

#[derive(Clone, Debug)]
pub struct Queue {
    directory: PathBuf,
    account: String,
}
pub struct Lease {
    directory: PathBuf,
    _lock: Lock,
}
impl Queue {
    pub fn open(account: &str) -> Result<Self> {
        let root = std::env::var_os("XDG_STATE_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
            .ok_or_else(|| failure("Neither XDG_STATE_HOME nor HOME is set"))?;
        Self::in_directory(&root, account)
    }
    pub fn in_directory(root: &Path, account: &str) -> Result<Self> {
        if !root.is_absolute()
            || account.is_empty()
            || account.len() > 64
            || account.starts_with('.')
            || !account
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c))
        {
            return Err(failure("Invalid update queue account or state directory"));
        }
        fs::create_dir_all(root)?;
        let parent = root.join("steam-tui");
        if !parent.exists() {
            filesystem::private_directory(&parent)?;
        }
        let metadata = fs::symlink_metadata(&parent)?;
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(failure(
                "Update state parent must be a directory owned and writable only by the current user",
            ));
        }
        let directory = parent.join(format!("updates-{}", account.to_ascii_lowercase()));
        filesystem::private_directory(&directory)?;
        Ok(Self {
            directory,
            account: account.to_ascii_lowercase(),
        })
    }
    pub(crate) fn state_directory(&self) -> &Path {
        &self.directory
    }
    pub fn account(&self) -> &str {
        &self.account
    }
    fn try_lease(&self) -> Result<Option<Lease>> {
        Ok(
            Lock::try_acquire(&self.directory.join("runner.lock"))?.map(|lock| Lease {
                directory: self.directory.clone(),
                _lock: lock,
            }),
        )
    }
    pub fn lease(&self) -> Result<Lease> {
        Ok(Lease {
            directory: self.directory.clone(),
            _lock: Lock::acquire(&self.directory.join("runner.lock"))?,
        })
    }
    fn read(&self) -> Result<State> {
        let path = self.directory.join("jobs.json");
        let state: State = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(State {
                    version: VERSION,
                    account: self.account.clone(),
                    next: 1,
                    settings: Settings::default(),
                    jobs: vec![],
                });
            }
            Err(e) => return Err(e.into()),
            Ok(m) if !m.is_file() || m.file_type().is_symlink() => {
                return Err(failure("Update state must be a regular file"));
            }
            Ok(_) => {
                serde_json::from_slice(&crate::installed::read_bounded(&path, 8 * 1024 * 1024)?)?
            }
        };
        let mut seen = BTreeSet::new();
        if state.version != VERSION
            || state.account != self.account
            || state.next == 0
            || state.jobs.len() > MAX_JOBS
            || state
                .settings
                .window
                .is_some_and(|w| w.start >= 1440 || w.end >= 1440)
            || state
                .settings
                .selections
                .iter()
                .any(|(id, s)| *id == 0 || s.validate().is_err())
            || state.settings.policies.contains_key(&0)
            || state.settings.priorities.contains_key(&0)
            || state.jobs.iter().any(|j| {
                j.id == 0 || j.id >= state.next || !seen.insert(j.id) || !valid_intent(&j.intent)
            })
        {
            return Err(failure(
                "Invalid or unsupported update queue state; file retained",
            ));
        }
        Ok(state)
    }
    fn edit<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let _lock = Lock::acquire(&self.directory.join("state.lock"))?;
        let mut state = self.read()?;
        let result = f(&mut state)?;
        filesystem::atomic_write(
            &self.directory.join("jobs.json"),
            &serde_json::to_vec(&state)?,
        )?;
        Ok(result)
    }
    pub fn jobs(&self) -> Result<Vec<Job>> {
        Ok(self.read()?.jobs)
    }
    pub fn job(&self, id: u64) -> Result<Option<Job>> {
        Ok(self.jobs()?.into_iter().find(|j| j.id == id))
    }
    pub fn settings(&self) -> Result<Settings> {
        Ok(self.read()?.settings)
    }
    pub fn save_settings(&self, settings: Settings) -> Result<()> {
        if settings
            .selections
            .iter()
            .any(|(id, s)| *id == 0 || s.validate().is_err())
            || settings
                .window
                .is_some_and(|w| w.start >= 1440 || w.end >= 1440)
            || settings.policies.contains_key(&0)
            || settings.priorities.contains_key(&0)
        {
            return Err(failure("Invalid update settings"));
        }
        self.edit(|s| {
            for job in &mut s.jobs {
                job.priority = settings
                    .priorities
                    .get(&job.intent.app_id)
                    .copied()
                    .unwrap_or(0);
            }
            s.settings = settings;
            Ok(())
        })
    }
    /// An automatic scan never retries a failed/cancelled intent or unpauses a
    /// user-paused job. Explicit enqueue is the retry/foreground operation.
    pub fn enqueue(&self, intent: Intent, foreground: bool) -> Result<u64> {
        if !valid_intent(&intent) {
            return Err(failure("Invalid update intention"));
        }
        self.edit(|s| {
            if let Some(job) = s
                .jobs
                .iter_mut()
                .rev()
                .find(|j| same_target(&j.intent, &intent) && !j.state.terminal())
            {
                if job.state != JobState::Running && intent.revision.is_some() {
                    job.intent.revision = intent.revision;
                }
                if foreground {
                    job.foreground = true;
                    if matches!(job.state, JobState::Failed | JobState::Paused) {
                        job.state = JobState::Pending;
                        job.error = None;
                    }
                }
                return Ok(job.id);
            }
            if !foreground
                && let Some(job) = s
                    .jobs
                    .iter()
                    .rev()
                    .find(|j| j.intent == intent && j.state == JobState::Cancelled)
            {
                return Ok(job.id);
            }
            if s.jobs.len() >= MAX_JOBS {
                s.jobs.retain(|j| !j.state.terminal());
                if s.jobs.len() >= MAX_JOBS {
                    return Err(failure("Update queue is full"));
                }
            }
            let id = s.next;
            s.next = s
                .next
                .checked_add(1)
                .ok_or_else(|| failure("Update job ID exhausted"))?;
            let priority = s
                .settings
                .priorities
                .get(&intent.app_id)
                .copied()
                .unwrap_or(0);
            s.jobs.push(Job {
                id,
                intent,
                state: JobState::Pending,
                foreground,
                priority,
                error: None,
            });
            Ok(id)
        })
    }
    pub fn control(&self, id: u64, target: JobState) -> Result<()> {
        if !matches!(
            target,
            JobState::Pending | JobState::Paused | JobState::Cancelled
        ) {
            return Err(failure("Invalid job control"));
        }
        self.edit(|s| {
            let j = s
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .ok_or_else(|| failure("Unknown update job"))?;
            if j.state == JobState::Complete {
                return Err(failure("Completed jobs cannot be controlled"));
            }
            j.state = target;
            j.error = None;
            Ok(())
        })
    }
    pub fn suspended(&self, id: u64, minute: u16, games_running: bool) -> Result<bool> {
        let state = self.read()?;
        Ok(state
            .jobs
            .iter()
            .find(|j| j.id == id)
            .is_some_and(|j| suspended(j, &state.settings, minute, games_running)))
    }
    pub fn ready(&self, minute: u16, games_running: bool) -> Result<Option<Job>> {
        let Some(_lease) = self.try_lease()? else {
            return Ok(None);
        };
        if minute >= 1440 {
            return Err(failure("Invalid local minute"));
        }
        let state = self.read()?;
        Ok(state
            .jobs
            .iter()
            .filter(|j| eligible(j, &state.settings, minute, games_running))
            .max_by_key(|j| (j.foreground, j.priority, std::cmp::Reverse(j.id)))
            .cloned())
    }
    pub fn claim(&self, lease: &Lease, minute: u16, games_running: bool) -> Result<Option<Job>> {
        self.claim_id(lease, None, minute, games_running)
    }
    pub(super) fn claim_id(
        &self,
        lease: &Lease,
        id: Option<u64>,
        minute: u16,
        games_running: bool,
    ) -> Result<Option<Job>> {
        if lease.directory != self.directory || minute >= 1440 {
            return Err(failure("Invalid update runner lease/time"));
        }
        self.edit(|s| {
            // A runner lease proves no previous worker is alive; Running means
            // interrupted work. The installer recovers its journal on execution.
            for j in &mut s.jobs {
                if j.state == JobState::Running {
                    j.state = JobState::Pending;
                }
            }
            let choice = s
                .jobs
                .iter()
                .enumerate()
                .filter(|(_, j)| {
                    id.is_none_or(|id| j.id == id)
                        && eligible(j, &s.settings, minute, games_running)
                })
                .max_by_key(|(_, j)| (j.foreground, j.priority, std::cmp::Reverse(j.id)))
                .map(|(i, _)| i);
            Ok(choice.map(|i| {
                s.jobs[i].state = JobState::Running;
                s.jobs[i].clone()
            }))
        })
    }
    pub(super) fn finish(&self, id: u64, state: JobState, error: Option<String>) -> Result<()> {
        self.edit(|s| {
            let job = s
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .ok_or_else(|| failure("Unknown completed update job"))?;
            job.state = state;
            job.error = error.map(|e| e.chars().take(4096).collect());
            Ok(())
        })
    }
}
fn valid_intent(i: &Intent) -> bool {
    let dependency_valid = i.dependency.as_ref().is_none_or(|d| {
        d.parent_app_id != 0
            && d.parent_app_id != i.app_id
            && d.requirement.app_id() == i.app_id
            && match &d.requirement {
                crate::launch::Dependency::Tool(tool) => tool.name.len() <= 256,
                crate::launch::Dependency::Shared { depots, .. } => {
                    !depots.is_empty() && depots.len() <= 128 && !depots.contains(&0)
                }
                crate::launch::Dependency::Additional { .. } => true,
            }
    });
    dependency_valid
        && !(i.shader && i.dependency.is_some())
        && i.app_id != 0
        && [&i.steam_root, &i.destination].into_iter().all(|p| {
            p.is_absolute()
                && !p
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
        })
}

fn eligible(j: &Job, settings: &Settings, minute: u16, games_running: bool) -> bool {
    matches!(j.state, JobState::Pending | JobState::Running)
        && (!j.intent.shader || settings.shaders_enabled)
        && (j.foreground
            || (settings
                .policies
                .get(&j.intent.app_id)
                .copied()
                .unwrap_or_default()
                == UpdatePolicy::Automatic
                && !(games_running && settings.pause_during_games)
                && settings.window.is_none_or(|w| w.contains(minute))))
}

pub(super) fn suspended(j: &Job, settings: &Settings, minute: u16, games_running: bool) -> bool {
    j.state == JobState::Paused
        || (j.intent.shader && !settings.shaders_enabled)
        || (!j.foreground
            && (settings
                .policies
                .get(&j.intent.app_id)
                .copied()
                .unwrap_or_default()
                != UpdatePolicy::Automatic
                || games_running && settings.pause_during_games
                || settings.window.is_some_and(|w| !w.contains(minute))))
}

fn same_target(a: &Intent, b: &Intent) -> bool {
    a.app_id == b.app_id
        && a.destination == b.destination
        && a.steam_root == b.steam_root
        && a.dependency == b.dependency
        && a.shader == b.shader
}
