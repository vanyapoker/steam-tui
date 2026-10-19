//! Durable account-scoped update intentions. No credentials or cached authority
//! are persisted: each execution resolves a new licensed plan. Only an explicit
//! runner (TUI or CLI) does work; there is no daemon.
mod store;
#[cfg(test)]
mod tests;

use crate::catalog::{self, UpdateStatus};
use crate::install::{InstallProgress, InstallRequest, PauseGate};
use crate::installed::{InstalledGame, Result, failure};
use std::path::PathBuf;
use steam_vent::Connection;
pub use store::{Job, JobState, Queue, Settings, UpdatePolicy, Window};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub fn local_minute() -> u16 {
    use chrono::Timelike;
    let now = chrono::Local::now();
    (now.hour() * 60 + now.minute()) as u16
}

/// Deliberately contains desired content and location, never a frozen install
/// plan or manifest key. Existing staging and transaction journals supply resume.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Intent {
    pub app_id: u32,
    pub steam_root: PathBuf,
    pub destination: PathBuf,
    #[serde(default)]
    pub dependency: Option<DependencyIntent>,
    #[serde(default)]
    pub shader: bool,
    /// A freshness hint for distinguishing a cancelled revision from a later
    /// update. Execution still resolves current authorized manifests.
    #[serde(default)]
    pub revision: Option<[u8; 20]>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DependencyIntent {
    pub parent_app_id: u32,
    pub requirement: crate::launch::Dependency,
}
impl Intent {
    pub fn installed(game: &InstalledGame) -> Self {
        Self {
            app_id: game.app_id,
            steam_root: game.steam_root.clone(),
            destination: game.library_root.clone(),
            dependency: None,
            shader: false,
            revision: None,
        }
    }
    pub fn shader(game: &InstalledGame) -> Self {
        let mut intent = Self::installed(game);
        intent.shader = true;
        intent
    }
    pub fn catalog(game: &InstalledGame, plan: &catalog::InstallPlan) -> Result<Self> {
        use sha1::Digest;
        let mut intent = Self::installed(game);
        intent.revision = Some(sha1::Sha1::digest(serde_json::to_vec(plan)?).into());
        Ok(intent)
    }
}

/// Resolve fresh ownership and metadata rather than trusting a catalog snapshot.
pub async fn request(
    connection: &Connection,
    account: &str,
    intent: &Intent,
) -> Result<InstallRequest> {
    let selection = Queue::open(account)?
        .settings()?
        .selections
        .get(&intent.app_id)
        .cloned()
        .unwrap_or_default();
    request_selected(connection, account, intent, &selection).await
}
async fn request_selected(
    connection: &Connection,
    account: &str,
    intent: &Intent,
    selection: &catalog::ContentSelection,
) -> Result<InstallRequest> {
    let inventory = crate::installed::scan(Some(&intent.steam_root), Some(account))?;
    if let Some(dependency) = &intent.dependency {
        let parent = inventory
            .games
            .iter()
            .find(|g| g.app_id == dependency.parent_app_id)
            .ok_or_else(|| failure("Queued dependency's parent game is no longer installed"))?;
        let library = crate::library::load(connection)
            .await
            .map_err(|e| failure(e.to_string()))?;
        if !library.apps.iter().any(|a| a.app_id == parent.app_id) {
            return Err(failure(
                "No current license for the dependency's parent game",
            ));
        }
        let request = dependency
            .requirement
            .install_request(connection, parent, account)
            .await?;
        if request.plan.app_id != intent.app_id || request.destination != intent.destination {
            return Err(failure(
                "Dependency installation target changed; recreate this job",
            ));
        }
        return Ok(request);
    }
    let existing = inventory
        .games
        .into_iter()
        .find(|g| g.app_id == intent.app_id);
    if existing
        .as_ref()
        .is_some_and(|g| g.library_root != intent.destination)
    {
        return Err(failure(
            "Queued update's library changed; cancel and recreate the job",
        ));
    }
    let plan = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        catalog::plan_selected(connection, intent.app_id, existing.as_ref(), selection),
    )
    .await
    .map_err(|_| failure("Update planning timed out"))?
    .map_err(|e| failure(e.to_string()))?;
    Ok(InstallRequest {
        plan,
        existing,
        account: account.into(),
        steam_root: intent.steam_root.clone(),
        destination: intent.destination.clone(),
    })
}

/// Check freshness without launching anything. Metadata failures are returned to
/// the caller so it can offer an explicit installed-version choice.
pub async fn check(connection: &Connection, game: &InstalledGame) -> Result<UpdateStatus> {
    check_selected(connection, game, &catalog::ContentSelection::default()).await
}

pub async fn check_selected(
    connection: &Connection,
    game: &InstalledGame,
    selection: &catalog::ContentSelection,
) -> Result<UpdateStatus> {
    let plan = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        catalog::plan_selected(connection, game.app_id, Some(game), selection),
    )
    .await
    .map_err(|_| failure("Update check timed out"))?
    .map_err(|e| failure(e.to_string()))?;
    Ok(catalog::compare(game, &plan))
}

/// Execute one claimed job. The runner lease is held until the install reaches a
/// safe boundary. Administrative commands use separate, short state-file locks.
/// Call from a LocalSet, as with the underlying installer.
pub async fn run_next(
    queue: Queue,
    connection: Connection,
    progress: mpsc::Sender<InstallProgress>,
    cancel: CancellationToken,
    minute: u16,
    games_running: bool,
) -> Result<Option<(u64, InstalledGame)>> {
    run_selected(
        queue,
        None,
        connection,
        progress,
        cancel,
        PauseGate::default(),
        minute,
        games_running,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_selected(
    queue: Queue,
    selected: Option<u64>,
    connection: Connection,
    progress: mpsc::Sender<InstallProgress>,
    cancel: CancellationToken,
    pause: PauseGate,
    minute: u16,
    games_running: bool,
) -> Result<Option<(u64, InstalledGame)>> {
    let lease = queue.lease()?;
    let Some(job) = queue.claim_id(&lease, selected, minute, games_running)? else {
        return Ok(None);
    };
    let result = async {
        let selection = queue.settings()?.selections.get(&job.intent.app_id).cloned().unwrap_or_default();
        let shader_game = if job.intent.shader {
            let inventory = crate::installed::scan(Some(&job.intent.steam_root), Some(queue.account()))?;
            let installed = inventory.games.iter().map(|game| game.app_id).collect();
            let cache_queue = queue.clone();
            tokio::task::spawn_blocking(move || crate::shaders::prune_uninstalled(&cache_queue, &installed)).await.map_err(|_| failure("Shader pruning worker failed"))??;
            let game = inventory.games.into_iter().find(|g| g.app_id == job.intent.app_id && g.library_root == job.intent.destination).ok_or_else(|| failure("Shader job game is no longer installed in this library"))?;
            let library = crate::library::load(&connection).await.map_err(|e| failure(e.to_string()))?;
            if !library.apps.iter().any(|app| app.app_id == game.app_id) { return Err(failure("No current game license for shader download")); }
            Some(game)
        } else { None };
        let request = if shader_game.is_none() { Some(tokio::select! {
            result = tokio::time::timeout(std::time::Duration::from_secs(120), request_selected(&connection, queue.account(), &job.intent, &selection)) => result.map_err(|_| failure("Update planning timed out"))??,
            _ = cancel.cancelled() => return Err(failure("Update cancelled during planning")),
        }) } else { None };
        if cancel.is_cancelled() { return Err(failure("Update cancelled before installation")); }
        let gate = PauseGate::default();
        let token = cancel.child_token();
        let run = async {
            if let Some(game) = shader_game {
                crate::shaders::download(&queue, connection, &game, progress, token.clone(), gate.clone()).await?;
                Ok(game)
            } else { crate::install::run(connection, request.expect("ordinary job request"), progress, token.clone(), gate.clone()).await }
        };
        tokio::pin!(run);
        let mut poll = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            tokio::select! {
                result = &mut run => break result,
                _ = poll.tick() => {
                    match queue.job(job.id) {
                        Ok(Some(current)) => match current.state {
                            JobState::Paused => gate.set(true),
                            JobState::Cancelled => { token.cancel(); gate.set(false); },
                            _ => {
                                match queue.settings() {
                                    Ok(settings) => gate.set(pause.is_paused() || store::suspended(&current, &settings, local_minute(), !crate::launch::running_app_ids().is_empty())),
                                    Err(_) => { token.cancel(); gate.set(false); },
                                }
                            },
                        },
                        // A broken state store must not orphan a mutating worker.
                        _ => { token.cancel(); gate.set(false); },
                    }
                }
            }
        }
    }.await;
    let current = queue.job(job.id)?;
    let state = if result.is_ok() {
        JobState::Complete
    } else if current
        .as_ref()
        .is_some_and(|j| j.state == JobState::Cancelled)
    {
        JobState::Cancelled
    } else if cancel.is_cancelled() {
        if current
            .as_ref()
            .is_some_and(|j| j.state == JobState::Paused)
        {
            JobState::Paused
        } else {
            JobState::Pending
        }
    } else {
        JobState::Failed
    };
    let error = if matches!(state, JobState::Failed | JobState::Cancelled) {
        result.as_ref().err().map(|e| e.to_string())
    } else {
        None
    };
    queue.finish(job.id, state, error)?;
    drop(lease);
    result.map(|game| Some((job.id, game)))
}
