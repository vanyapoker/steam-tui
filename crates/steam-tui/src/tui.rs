mod achievements;
mod model;
mod reconnect;
mod render;
mod theme;
mod watchdog;

use std::error::Error;
use std::io::{self, Stdout};
use std::time::Instant;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use steam_core::catalog::{CatalogItem, CatalogSnapshot};
use steam_core::friends::FriendEvent;
use steam_core::install::{Destination, InstallPhase, InstallRequest, PauseGate};
use steam_core::installed::{InstalledGame, Inventory};
use steam_core::launch::LaunchBackend;
use steam_core::notifications::NotificationEvent;
use steam_core::presence::PresenceEvent;
use steam_vent::Connection;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, LocalSet};
use tokio_util::sync::CancellationToken;

use model::{Action, CloudResult, InstallModal, Interim, Model};
use reconnect::{Decision, Reconnect, Trigger, is_connection_error, is_permanent_failure};
use watchdog::{Activity, Outcome, ProbeKind, Watchdog};

// Covers partial initialization as well as normal exit, errors, and unwinding.
struct TerminalSession {
    raw: bool,
    alternate: bool,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        let mut session = Self {
            raw: false,
            alternate: false,
        };
        enable_raw_mode()?;
        session.raw = true;
        session.alternate = true;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            Hide
        )?;
        Ok(session)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.alternate {
            let _ = execute!(
                io::stdout(),
                Show,
                DisableMouseCapture,
                PopKeyboardEnhancementFlags,
                LeaveAlternateScreen
            );
        }
        if self.raw {
            let _ = disable_raw_mode();
        }
    }
}

struct Workers(Vec<JoinHandle<()>>);

impl Workers {
    fn push(&mut self, worker: JoinHandle<()>) {
        self.0.retain(|worker| !worker.is_finished());
        self.0.push(worker);
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        for worker in &self.0 {
            worker.abort();
        }
    }
}

// Events are sent a handful of times per session; boxing the snapshots buys nothing.
#[allow(clippy::large_enum_variant)]
enum JobEvent {
    CloudConflicts {
        game: InstalledGame,
        items: Vec<steam_core::cloud::Conflict>,
    },
    CloudResolved {
        app_id: u32,
        name: String,
        result: Result<String, String>,
    },
    News {
        request: u64,
        result: Result<Vec<steam_core::news::NewsItem>, String>,
    },
    Achievements {
        request: u64,
        result: Result<steam_core::achievements::Achievements, String>,
    },
    /// Provisional rows (installed inventory, then cached catalog) sent before
    /// the live catalog while the model is still loading.
    Interim(u64, Interim, CatalogSnapshot),
    Catalog(u64, Result<CatalogSnapshot, String>, Option<String>),
    UpdateChecked {
        epoch: u64,
        game: InstalledGame,
        backend: LaunchBackend,
        result: Result<steam_core::catalog::UpdateStatus, String>,
    },
    QueueIdle,
    ContentProperties {
        app_id: u32,
        result: Result<steam_core::content_options::Properties, String>,
    },
    ContentChanged {
        app_id: u32,
        result: Result<u64, String>,
    },
    Modal(Result<Box<InstallModal>, String>),
    Destination(Result<Destination, String>),
    Installed {
        app_id: u32,
        shader: bool,
        result: Result<InstalledGame, String>,
    },
    CloudBadges(Vec<(u32, Option<steam_core::cloud::CloudBadge>)>),
    /// How the last sync of a game ended.
    CloudOutcome {
        app_id: u32,
        result: CloudResult,
    },
    /// One message sent or received by a game's Steam backend.
    Backend(steam_core::backend::Event),
    Launch {
        app_id: u32,
        result: Result<(u32, String), String>,
        /// Outcome of the Steam Cloud download that preceded the launch.
        cloud: Option<String>,
    },
    /// Progress text from a Steam Cloud sync.
    Cloud {
        app_id: u32,
        message: String,
    },
    /// A manual Steam Cloud sync finished.
    CloudDone {
        app_id: u32,
        message: String,
    },
    /// Launch needs a tool that is not installed; install it, then retry.
    NeedsTool {
        game: Box<steam_core::installed::InstalledGame>,
        dependency: steam_core::launch::Dependency,
        backend: LaunchBackend,
    },
    ToolPlan {
        epoch: u64,
        result: Result<InstallRequest, String>,
        requirement: steam_core::launch::Dependency,
    },
    Reconnected {
        trigger: Trigger,
        result: Result<Connection, String>,
    },
    Probe {
        epoch: u64,
        kind: ProbeKind,
        alive: bool,
    },
    Preflight {
        epoch: u64,
        alive: bool,
        then: Then,
    },
}

/// Social feeds and game presence bound to one Steam connection. Dropping this aborts
/// them, so replacing it after a reconnect never leaves a worker on the dead
/// session. The install job is deliberately not part of it: it must be awaited.
struct Live {
    presence_rx: mpsc::Receiver<PresenceEvent>,
    friend_rx: mpsc::Receiver<FriendEvent>,
    notification_rx: mpsc::Receiver<NotificationEvent>,
    refresh_tx: mpsc::Sender<()>,
    friends_open: bool,
    notifications_open: bool,
    _workers: Workers,
}

impl Live {
    /// Without a connection both feeds stay closed.
    fn spawn(connection: Option<&Connection>) -> Self {
        let (friend_tx, friend_rx) = mpsc::channel(32);
        let (refresh_tx, refresh_rx) = mpsc::channel(1);
        let (notification_tx, notification_rx) = mpsc::channel(64);
        let mut workers = Workers(Vec::new());
        let (presence_tx, presence_rx) = mpsc::channel(8);
        workers.push(tokio::task::spawn_local(steam_core::presence::run(
            connection.cloned(),
            presence_tx,
        )));
        if let Some(connection) = connection {
            let friends = connection.clone();
            workers.push(tokio::task::spawn_local(async move {
                let result =
                    steam_core::friends::run_with_refresh(friends, friend_tx.clone(), refresh_rx)
                        .await;
                let message = match result {
                    Ok(()) => "Friends connection ended".to_owned(),
                    Err(error) => format!("Friends disconnected: {error}"),
                };
                let _ = friend_tx.send(FriendEvent::Status(message)).await;
            }));
            let notifications = connection.clone();
            workers.push(tokio::task::spawn_local(async move {
                // The worker reports its own failure; only a clean end needs a message.
                if steam_core::notifications::run(notifications, notification_tx.clone())
                    .await
                    .is_ok()
                {
                    let _ = notification_tx
                        .send(NotificationEvent::Status(
                            "Notifications connection ended".into(),
                        ))
                        .await;
                }
            }));
        }
        Self {
            presence_rx,
            friend_rx,
            notification_rx,
            refresh_tx,
            friends_open: connection.is_some(),
            notifications_open: connection.is_some(),
            _workers: workers,
        }
    }
}

/// How long install planning may take before the session is presumed dead.
const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const PLAN_TIMEOUT_MESSAGE: &str = "Install planning timed out";
/// Wait for the quick probe that precedes planning or a refresh.
const PREFLIGHT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A user action held back until the session is known to answer.
enum Then {
    Install(Box<CatalogItem>),
    Refresh,
}

fn spawn_probe(
    connection: Connection,
    epoch: u64,
    kind: ProbeKind,
    updates: mpsc::Sender<JobEvent>,
    workers: &mut Workers,
) {
    workers.push(tokio::task::spawn_local(async move {
        let alive =
            steam_core::liveness::probe(&connection, steam_core::liveness::PROBE_TIMEOUT).await;
        let _ = updates.send(JobEvent::Probe { epoch, kind, alive }).await;
    }));
}

fn spawn_preflight(
    connection: Connection,
    epoch: u64,
    then: Then,
    updates: mpsc::Sender<JobEvent>,
    workers: &mut Workers,
) {
    workers.push(tokio::task::spawn_local(async move {
        let alive = steam_core::liveness::probe(&connection, PREFLIGHT_TIMEOUT).await;
        let _ = updates
            .send(JobEvent::Preflight { epoch, alive, then })
            .await;
    }));
}

/// Runs a held-back action on `connection`.
fn run_then(
    then: Then,
    connection: Option<&Connection>,
    account: Option<&str>,
    model: &mut Model,
    catalog_generation: &mut u64,
    updates: &mpsc::Sender<JobEvent>,
    workers: &mut Workers,
) {
    match then {
        Then::Install(game) => match connection {
            Some(connection) => {
                prepare_install(connection.clone(), *game, updates.clone(), workers)
            }
            None => {
                model.preparing_install = false;
                model.status = "No Steam connection — press r to reconnect".into();
            }
        },
        Then::Refresh => {
            *catalog_generation += 1;
            let active_library = model
                .active_install
                .as_ref()
                .map(|(_, library)| library.clone());
            scan(
                ScanJob {
                    connection: connection.cloned(),
                    account: account.map(str::to_owned),
                    completion: None,
                    generation: *catalog_generation,
                    active_library,
                    interim: false,
                    awaiting_login: false,
                },
                updates.clone(),
                workers,
            );
        }
    }
}

fn spawn_reconnect(
    account: String,
    trigger: Trigger,
    delay: std::time::Duration,
    updates: mpsc::Sender<JobEvent>,
    workers: &mut Workers,
) {
    workers.push(tokio::task::spawn_local(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        // Saved-token login only; errors carry no token material.
        let result = steam_core::account::resume(&account)
            .await
            .map_err(|error| error.to_string());
        let _ = updates
            .send(JobEvent::Reconnected { trigger, result })
            .await;
    }));
}

/// Applies a reconnect decision: user-visible status plus the attempt itself.
fn apply_decision(
    decision: Decision,
    trigger: Trigger,
    account: Option<&str>,
    updates: &mpsc::Sender<JobEvent>,
    workers: &mut Workers,
    model: &mut Model,
) {
    // A failed job keeps its own message; the reconnect note is appended.
    let note = |model: &mut Model, text: &str| match trigger {
        Trigger::JobFailed => model.status = format!("{} · {text}", model.status),
        Trigger::ConnectionLost => {
            model.status = text.to_owned();
            model.connection_lost(text);
        }
        Trigger::Manual => model.status = text.to_owned(),
        Trigger::Startup => {
            model.status = text.to_owned();
            model.connection_lost(text);
        }
    };
    match decision {
        Decision::Start(delay) => {
            note(
                model,
                match trigger {
                    Trigger::ConnectionLost => "Steam connection lost — reconnecting…",
                    Trigger::Startup => "Signing in to Steam…",
                    _ => "Reconnecting to Steam…",
                },
            );
            if let Some(account) = account {
                spawn_reconnect(account.to_owned(), trigger, delay, updates.clone(), workers);
            }
        }
        Decision::Deferred => note(
            model,
            if trigger == Trigger::ConnectionLost {
                "Steam connection lost — reconnecting after the install ends"
            } else {
                "Reconnect waits for the install to end"
            },
        ),
        Decision::Exhausted => note(model, "Steam connection lost — press r to reconnect"),
        Decision::Ignored => {}
    }
}

struct ScanJob {
    connection: Option<Connection>,
    account: Option<String>,
    completion: Option<String>,
    generation: u64,
    active_library: Option<std::path::PathBuf>,
    /// Show installed/cached rows before the live catalog lands.
    interim: bool,
    /// Sign-in is still running: show the interim rows and send no final catalog;
    /// the scan that follows the login delivers it.
    awaiting_login: bool,
}

fn scan(job: ScanJob, updates: mpsc::Sender<JobEvent>, workers: &mut Workers) {
    let ScanJob {
        connection,
        account,
        completion,
        generation,
        active_library,
        interim,
        awaiting_login,
    } = job;
    workers.push(tokio::task::spawn_local(async move {
        let scan_account = account.clone();
        let inventory = tokio::task::spawn_blocking(move || {
            let destinations = steam_core::install::destinations(None)
                .map_err(|error| error.to_string())?;
            let mut unavailable = std::collections::BTreeSet::new();
            let mut warnings = Vec::new();
            for destination in destinations {
                // Our live transaction owns this library's locks and recovery.
                if active_library.as_ref() == Some(&destination.path) { continue; }
                if let Err(error) = steam_core::install::recover(&destination.path) {
                    warnings.push(format!("Recovery failed for {}; games in this library are hidden for safety: {error}", destination.path.display()));
                    unavailable.insert(destination.path);
                }
            }
            let mut inventory = steam_core::installed::scan(None, scan_account.as_deref())
                .map_err(|error| error.to_string())?;
            inventory.games.retain(|game| !unavailable.contains(&game.library_root));
            inventory.warnings.extend(warnings);
            Ok::<_, String>(inventory)
        }).await;
        let result = match inventory {
            Ok(Ok(inventory)) => {
                if interim && (connection.is_some() || awaiting_login) {
                    let installed = inventory_catalog(
                        inventory.games.iter().cloned(),
                        inventory.warnings.clone(),
                        None,
                    );
                    let _ = updates
                        .send(JobEvent::Interim(generation, Interim::Installed, installed))
                        .await;
                    if let Some(name) = account.clone() {
                        let for_cache = Inventory {
                            games: inventory.games.clone(),
                            warnings: inventory.warnings.clone(),
                        };
                        let cached = tokio::task::spawn_blocking(move || {
                            steam_core::catalog::cached(&name, for_cache)
                        })
                        .await;
                        if let Ok(Some(snapshot)) = cached {
                            let _ = updates
                                .send(JobEvent::Interim(generation, Interim::Cached, snapshot))
                                .await;
                        }
                    }
                }
                if awaiting_login && connection.is_none() {
                    return;
                }
                if let Some(connection) = &connection {
                    steam_core::catalog::load(connection, inventory, account.as_deref())
                        .await.map_err(|error| error.to_string())
                } else {
                    Ok(inventory_catalog(
                        inventory.games,
                        inventory.warnings,
                        Some("Online authentication is required to install or update"),
                    ))
                }
            }
            Ok(Err(error)) if connection.is_some() => {
                steam_core::catalog::load(
                    connection.as_ref().expect("connected catalog"),
                    Inventory { games: vec![], warnings: vec![error] },
                    account.as_deref(),
                ).await.map_err(|error| error.to_string())
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(format!("Inventory worker failed: {error}")),
        };
        let _ = updates.send(JobEvent::Catalog(generation, result, completion)).await;
    }));
}

/// Rows built purely from the disk inventory. `unavailable_reason` is set only
/// when no live catalog will follow (offline), so interim rows stay actionable.
fn inventory_catalog(
    games: impl IntoIterator<Item = InstalledGame>,
    warnings: Vec<String>,
    unavailable_reason: Option<&str>,
) -> CatalogSnapshot {
    CatalogSnapshot {
        items: games
            .into_iter()
            .map(|game| CatalogItem {
                app_id: game.app_id,
                name: game.name.clone(),
                last_played: game.last_played,
                playtime_minutes: game.playtime_minutes,
                size_bytes: game.size_bytes,
                installed: Some(game),
                plan: None,
                update: steam_core::catalog::UpdateStatus::Unknown,
                unavailable_reason: unavailable_reason.map(str::to_owned),
            })
            .collect(),
        warnings,
    }
}

fn prepare_install(
    connection: Connection,
    game: CatalogItem,
    updates: mpsc::Sender<JobEvent>,
    workers: &mut Workers,
) {
    workers.push(tokio::task::spawn_local(async move {
        let result = tokio::time::timeout(PLAN_TIMEOUT, async {
            let plan = match game.plan {
                Some(plan) => plan,
                None => {
                    steam_core::catalog::plan(&connection, game.app_id, game.installed.as_ref())
                        .await
                        .map_err(|error| error.to_string())?
                }
            };
            let existing = game.installed;
            let lookup_existing = existing.clone();
            let (steam_root, destinations) = tokio::task::spawn_blocking(move || {
                let steam_root = match lookup_existing.as_ref() {
                    Some(game) => game.steam_root.clone(),
                    None => steam_core::installed::discover_root(None)
                        .map_err(|error| error.to_string())?,
                };
                let destinations = if let Some(game) = lookup_existing {
                    vec![
                        steam_core::install::destination(&game.library_root)
                            .map_err(|error| error.to_string())?,
                    ]
                } else {
                    steam_core::install::destinations(Some(&steam_root))
                        .map_err(|error| error.to_string())?
                };
                Ok::<_, String>((steam_root, destinations))
            })
            .await
            .map_err(|error| error.to_string())??;
            Ok(Box::new(InstallModal {
                plan,
                existing,
                steam_root,
                destinations,
                selected: 0,
                custom: None,
                checking: false,
                low_space_confirmed: false,
                error: None,
            }))
        })
        .await
        .unwrap_or_else(|_| Err(PLAN_TIMEOUT_MESSAGE.to_owned()));
        let _ = updates.send(JobEvent::Modal(result)).await;
    }));
}

/// Dependency-install diagnostics; the status line is overwritten by refreshes.
/// Startup milestones for `scripts/bench-startup.py`: with `STEAM_TUI_STARTUP_TRACE=FILE`
/// each call appends `<ms since start> <event>`; otherwise it does nothing.
fn trace(event: &str) {
    use std::io::Write;
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    let Some(path) = std::env::var_os("STEAM_TUI_STARTUP_TRACE") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{} {event}", start.elapsed().as_millis());
    }
}

/// Returns freed heap to the OS. A catalog load or install burst peaks at ~100 MB of
/// short-lived buffers; glibc keeps those pages (RSS stayed ~114 MB against ~25 MB
/// after trimming), so trim whenever such a burst ends.
fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim has no preconditions; it only releases unused heap pages.
    unsafe {
        libc::malloc_trim(0);
    }
}

fn debug_log(line: &str) {
    use std::io::Write;
    if let Some(home) = std::env::var_os("HOME") {
        let dir = std::path::PathBuf::from(home).join(".local/state/steam-tui");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("deps.log"))
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// Direct launch, preceded by the automatic Steam Cloud download. A cloud failure
/// never blocks the launch. Uploading is manual (`C`).
fn spawn_launch(
    game: steam_core::installed::InstalledGame,
    cloud: Option<(Connection, String)>,
    backend: LaunchBackend,
    model: &mut Model,
    updates: mpsc::Sender<JobEvent>,
    workers: &mut Workers,
) {
    if !model.launching.insert(game.app_id) {
        model.status = "This direct-launch request is already in progress".into();
        return;
    }
    let app_id = game.app_id;
    model
        .launches
        .insert(app_id, format!("{}: resolving direct launch…", game.name));
    workers.push(tokio::task::spawn_local(async move {
        let backend = if backend == LaunchBackend::BuiltIn {
            let owned = game.clone();
            match tokio::task::spawn_blocking(move || {
                steam_core::launch::default_backend_for_game(&owned)
            })
            .await
            {
                Ok(Ok(backend)) => backend,
                result => {
                    let error = match result {
                        Ok(Err(error)) => error.to_string(),
                        Err(error) => format!("Launch worker failed: {error}"),
                        _ => unreachable!(),
                    };
                    let _ = updates
                        .send(JobEvent::Launch {
                            app_id,
                            result: Err(error),
                            cloud: None,
                        })
                        .await;
                    return;
                }
            }
        } else {
            backend
        };
        let mut note = None;
        let account_name = cloud.as_ref().map(|(_, name)| name.clone());
        let cloud_connection = cloud.as_ref().map(|(connection, _)| connection.clone());
        if let Some((connection, account)) = cloud {
            let progress = updates.clone();
            let mut report = |message: String| {
                let _ = progress.try_send(JobEvent::Cloud { app_id, message });
            };
            let synced = sync_with_retry(
                &connection,
                &game,
                &account,
                steam_core::cloud::Mode::Down,
                backend,
                &mut report,
            )
            .await;
            if let Some(result) = sync_outcome(&synced) {
                let _ = updates
                    .send(JobEvent::CloudOutcome { app_id, result })
                    .await;
            }
            note = match synced {
                Ok(Some(report)) => {
                    let summary = report.summary();
                    (!summary.ends_with("no cloud files")).then_some(summary)
                }
                Ok(None) => None,
                Err(error) => Some(format!(
                    "Steam Cloud sync failed: {error}; launching with local saves"
                )),
            };
        }
        // Forward what the per-launch backend sends and receives to the TUI.
        let (traffic_tx, mut traffic_rx) = mpsc::unbounded_channel();
        let forward = updates.clone();
        tokio::task::spawn_local(async move {
            while let Some(event) = traffic_rx.recv().await {
                if forward.send(JobEvent::Backend(event)).await.is_err() {
                    break;
                }
            }
        });
        let event = match steam_core::launch::run_with_backend(
            &game,
            cloud_connection.as_ref(),
            account_name.as_deref(),
            backend,
            Some(traffic_tx),
        )
        .await
        {
            Ok(result) => JobEvent::Launch {
                app_id,
                result: Ok((result.pid, result.description)),
                cloud: note,
            },
            Err(error) => match steam_core::launch::Dependency::from_error(error.as_ref()) {
                Some(missing) => JobEvent::NeedsTool {
                    backend,
                    dependency: missing,
                    game: Box::new(game),
                },
                None => JobEvent::Launch {
                    app_id,
                    result: Err(error.to_string()),
                    cloud: note,
                },
            },
        };
        let _ = updates.send(event).await;
    }));
}

/// How a finished sync should look in the library; `None` when the game has no
/// cloud configuration.
fn sync_outcome(
    result: &Result<Option<steam_core::cloud::SyncReport>, Box<dyn Error + Send + Sync>>,
) -> Option<CloudResult> {
    match result {
        Ok(Some(report)) if !report.failed.is_empty() || !report.warnings.is_empty() => {
            Some(CloudResult::Failed)
        }
        Ok(Some(report)) if !report.conflicts.is_empty() => Some(CloudResult::Conflicts),
        Ok(Some(_)) => Some(CloudResult::Ok),
        Ok(None) => None,
        Err(_) => Some(CloudResult::Failed),
    }
}

/// Computes cloud badges for installed games off the UI thread (stat-based; see
/// `steam_core::cloud::badge_for`).
fn spawn_badges(
    games: Vec<steam_core::installed::InstalledGame>,
    account: Option<&str>,
    account_id: Option<u32>,
    updates: mpsc::Sender<JobEvent>,
    workers: &mut Workers,
) {
    let Some(account) = account.map(str::to_owned) else {
        return;
    };
    if games.is_empty() {
        return;
    }
    workers.push(tokio::task::spawn_local(async move {
        let badges = tokio::task::spawn_blocking(move || {
            games
                .iter()
                .map(|game| {
                    (
                        game.app_id,
                        steam_core::cloud::badge_for(game, &account, account_id),
                    )
                })
                .collect::<Vec<_>>()
        })
        .await;
        if let Ok(badges) = badges {
            let _ = updates.send(JobEvent::CloudBadges(badges)).await;
        }
    }));
}

/// Runs one cloud sync; if it fails (typically a Steam connection that closed
/// underneath it) logs in again once and retries, since an interrupted sync is
/// safe to repeat: finished files are skipped by SHA1.
async fn sync_with_retry(
    connection: &Connection,
    game: &steam_core::installed::InstalledGame,
    account: &str,
    mode: steam_core::cloud::Mode,
    backend: LaunchBackend,
    report: &mut impl FnMut(String),
) -> Result<Option<steam_core::cloud::SyncReport>, Box<dyn Error + Send + Sync>> {
    match steam_core::cloud::sync_game_with_backend(
        connection, game, account, mode, backend, report,
    )
    .await
    {
        Ok(result) => Ok(result),
        Err(first) if is_connection_error(&first.to_string()) => {
            report("Steam Cloud: connection problem, reconnecting…".into());
            match steam_core::account::resume(account).await {
                Ok(fresh) => {
                    steam_core::cloud::sync_game_with_backend(
                        &fresh, game, account, mode, backend, report,
                    )
                    .await
                }
                Err(_) => Err(first),
            }
        }
        Err(error) => Err(error),
    }
}

/// Manual Steam Cloud sync: download, then upload local changes.
fn spawn_cloud(
    connection: Connection,
    account: String,
    game: steam_core::installed::InstalledGame,
    updates: mpsc::Sender<JobEvent>,
) -> JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let app_id = game.app_id;
        let progress = updates.clone();
        let mut report = |message: String| {
            let _ = progress.try_send(JobEvent::Cloud { app_id, message });
        };
        let synced = sync_with_retry(
            &connection,
            &game,
            &account,
            steam_core::cloud::Mode::Full,
            LaunchBackend::BuiltIn,
            &mut report,
        )
        .await;
        let outcome = sync_outcome(&synced);
        let mut conflict_items = Vec::new();
        if let Ok(Some(report)) = &synced
            && !report.conflicts.is_empty()
        {
            match steam_core::cloud::conflicts_game(&connection, &game, &account, &report.conflicts)
                .await
            {
                Ok(items) => conflict_items = items,
                Err(error) => debug_log(&format!("cannot inspect cloud conflicts: {error}")),
            }
        }
        let message = match synced {
            Ok(Some(report)) => {
                let issue = report.failed.first().or(report.warnings.first());
                match issue {
                    Some(issue) => format!("{} ({issue})", report.summary()),
                    None => report.summary(),
                }
            }
            Ok(None) => format!("{} has no Steam Cloud configuration", game.name),
            Err(error) => format!("Steam Cloud sync failed: {error}"),
        };
        if let Some(result) = outcome {
            let _ = updates
                .send(JobEvent::CloudOutcome { app_id, result })
                .await;
        }
        let _ = updates.send(JobEvent::CloudDone { app_id, message }).await;
        if !conflict_items.is_empty() {
            let _ = updates
                .send(JobEvent::CloudConflicts {
                    game,
                    items: conflict_items,
                })
                .await;
        }
    })
}

fn spawn_queued_install(
    queue: steam_core::updates::Queue,
    id: u64,
    app_id: u32,
    connection: Connection,
    progress: mpsc::Sender<steam_core::install::InstallProgress>,
    updates: mpsc::Sender<JobEvent>,
) -> (CancellationToken, PauseGate, JoinHandle<()>) {
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let pause = PauseGate::default();
    let gate = pause.clone();
    let job = tokio::task::spawn_local(async move {
        let shader = queue
            .job(id)
            .ok()
            .flatten()
            .is_some_and(|job| job.intent.shader);
        let result = steam_core::updates::run_selected(
            queue,
            Some(id),
            connection,
            progress,
            token,
            gate,
            crate::content_jobs::minute(),
            !steam_core::launch::running_app_ids().is_empty(),
        )
        .await;
        let event = match result {
            Ok(Some((_, game))) => JobEvent::Installed {
                app_id: game.app_id,
                shader,
                result: Ok(game),
            },
            Ok(None) => JobEvent::QueueIdle,
            Err(error) => JobEvent::Installed {
                app_id,
                shader,
                result: Err(error.to_string()),
            },
        };
        let _ = updates.send(event).await;
    });
    (cancel, pause, job)
}

/// Login precedes terminal setup; local-only inventory remains usable on failure.
pub async fn run(account: Option<String>, offline: bool) -> Result<(), Box<dyn Error>> {
    LocalSet::new().run_until(run_local(account, offline)).await
}

async fn run_local(account: Option<String>, offline: bool) -> Result<(), Box<dyn Error>> {
    trace("start");
    // The terminal comes up first; sign-in runs in the background as a `Startup` reconnect.
    let online = !offline && account.is_some();
    let connection_error = (!offline && account.is_none())
        .then(|| "No account selected; use --account or run offline".to_owned());
    let queue = account
        .as_deref()
        .map(steam_core::updates::Queue::open)
        .transpose();
    let (queue, queue_error) = match queue {
        Ok(q) => (q, None),
        Err(e) => (None, Some(e.to_string())),
    };
    let mut checking_updates = std::collections::BTreeSet::new();
    let mut pending_update_launch: Option<(u64, u32, LaunchBackend)> = None;
    let mut connection: Option<Connection> = None;
    let mut workers = Workers(Vec::new());
    let mut live = Live::spawn(connection.as_ref());
    let mut model = Model::new(offline, connection_error);
    model.account = account.clone();
    if let Some(error) = queue_error {
        model
            .warnings
            .push(format!("Persistent updates unavailable: {error}"));
    }
    if let Some(text) = super::preferences::load_settings() {
        model.apply_settings(&text);
    }
    let mut saved_settings = model.settings_text();
    let (job_tx, mut job_rx) = mpsc::channel(32);
    let (progress_tx, mut progress_rx) = mpsc::channel(32);
    let mut catalog_generation = 0;
    scan(
        ScanJob {
            connection: connection.clone(),
            account: account.clone(),
            completion: None,
            generation: catalog_generation,
            active_library: None,
            interim: true,
            awaiting_login: online,
        },
        job_tx.clone(),
        &mut workers,
    );
    // Start network/disk workers before terminal initialization.
    tokio::task::yield_now().await;
    let _session = TerminalSession::enter()?;
    let mut terminal: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    trace("terminal_ready");
    let mut input = EventStream::new();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    // Unlike auxiliary workers, an installer must never be aborted in commit.
    let mut install_job: Option<(CancellationToken, PauseGate, JoinHandle<()>)> = None;
    let mut active_queue: Option<u64> = None;
    let mut reconnect = Reconnect::new(online);
    // Handlers only record these; they are applied at the top of the loop, when no
    // select! future borrows the workers' channels.
    let mut reconnect_request: Option<Trigger> = None;
    let mut restored: Option<(Trigger, Connection)> = None;
    let mut lost_noted = false;
    let mut epoch: u64 = 0;
    let mut watchdog = Watchdog::new(online, Instant::now());
    // What to do once a reconnect lands, and what to tell the user then.
    let mut after_reconnect: Option<Then> = None;
    let mut restored_note: Option<&'static str> = None;
    // We cancelled the install ourselves because its connection died.
    let mut stall_cancel = false;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Manual Steam Cloud syncs are awaited on exit so an upload batch is completed.
    let mut cloud_jobs: Vec<JoinHandle<()>> = Vec::new();
    let mut first_frame = false;
    if online {
        let decision = reconnect.request(Trigger::Startup, false);
        apply_decision(
            decision,
            Trigger::Startup,
            account.as_deref(),
            &job_tx,
            &mut workers,
            &mut model,
        );
    }
    let result: Result<(), Box<dyn Error>> = async {
        loop {
            if model.queue_view && let Some(queue) = &queue {
                if let Ok(jobs) = queue.jobs() { model.queue_jobs = jobs; model.queue_selected = model.queue_selected.min(model.queue_jobs.len().saturating_sub(1)); }
                if let Ok(settings) = queue.settings() { model.queue_settings = settings; }
            }
            let settings = model.settings_text();
            if settings != saved_settings {
                super::preferences::save_settings(&settings);
                saved_settings = settings;
            }
            if let Some((trigger, new)) = restored.take() {
                reconnect.succeeded();
                lost_noted = false;
                // Probes and pre-flights of the old connection are now stale.
                epoch += 1;
                watchdog.reset(Instant::now());
                // Replacing `live` aborts the old read-only workers; the install job
                // (if any) keeps its own connection clone and is never touched.
                live = Live::spawn(Some(&new));
                connection = Some(new);
                model.connection_restored();
                if trigger == Trigger::Startup {
                    trace("signed_in");
                    if let Some(name) = account.as_deref()
                        && let Err(error) = super::preferences::remember(name)
                    {
                        model.warnings.push(format!("Could not remember the account: {error}"));
                    }
                }
                let message = restored_note.take().unwrap_or(match trigger {
                    Trigger::JobFailed => "Reconnected to Steam — press i to resume",
                    Trigger::Startup => "Signed in to Steam",
                    _ => "Reconnected to Steam",
                });
                model.status = message.into();
                model.loading = true;
                // Results of scans on the old connection are ignored by generation.
                catalog_generation += 1;
                let active_library = model.active_install.as_ref().map(|(_, library)| library.clone());
                scan(ScanJob { connection: connection.clone(), account: account.clone(), completion: Some(message.into()), generation: catalog_generation, active_library, interim: false, awaiting_login: false }, job_tx.clone(), &mut workers);
                // Continue what the user asked for before the reconnect interrupted it.
                if let Some(then @ Then::Install(_)) = after_reconnect.take() {
                    run_then(then, connection.as_ref(), account.as_deref(), &mut model, &mut catalog_generation, &job_tx, &mut workers);
                }
                after_reconnect = None;
            }
            if !model.install_active && !model.preparing_install && !model.loading && model.modal.is_none()
                && !reconnect.in_flight() && !model.cancel_requested && model.launching.is_empty()
                && let (Some(queue), Some(connection)) = (&queue, connection.clone())
            {
                match queue.ready(crate::content_jobs::minute(), !steam_core::launch::running_app_ids().is_empty()) {
                    Ok(Some(job)) => {
                        model.active_shader = job.intent.shader;
                        model.active_install = Some((job.intent.app_id, job.intent.destination.clone()));
                        model.install_active = true;
                        model.begin_install(); watchdog.install_activity(Instant::now());
                        active_queue = Some(job.id);
                        install_job = Some(spawn_queued_install(queue.clone(), job.id, job.intent.app_id, connection, progress_tx.clone(), job_tx.clone()));
                    }
                    Ok(None) => {},
                    Err(error) => { model.status = format!("Update queue unavailable: {error}"); },
                }
            }
            if let (Some(queue), Some(id)) = (&queue, active_queue)
                && let Ok(paused) = queue.suspended(id, crate::content_jobs::minute(), !steam_core::launch::running_app_ids().is_empty())
            { model.set_paused(paused, Instant::now()); }
            let activity = Activity {
                connected: connection.is_some() && !reconnect.in_flight(),
                install_active: model.install_active,
                preparing: model.preparing_install,
                paused: model.paused,
                downloading: model.progress.as_ref().is_some_and(|progress| progress.phase == InstallPhase::Downloading),
                loading: model.loading,
                scan_generation: catalog_generation,
            };
            if let Some(kind) = watchdog.due(Instant::now(), activity)
                && let Some(connection) = connection.clone()
            {
                spawn_probe(connection, epoch, kind, job_tx.clone(), &mut workers);
            }
            if let Some(trigger) = reconnect_request.take() {
                let decision = reconnect.request(trigger, model.install_active);
                apply_decision(decision, trigger, account.as_deref(), &job_tx, &mut workers, &mut model);
            }
            // A new sign-in would replace the session a running game (or the Valve
            // host serving it) depends on, so automatic recovery waits for it to end.
            let hold = model.install_active || !model.running_games.is_empty() || !model.launching.is_empty();
            if let Some((trigger, decision)) = reconnect.release(hold) {
                apply_decision(decision, trigger, account.as_deref(), &job_tx, &mut workers, &mut model);
            }
            // Friends ending means a dead or displaced session (their presence would
            // otherwise sit at Unknown forever), even while notifications look alive.
            if connection.is_some() && !live.friends_open && !lost_noted {
                lost_noted = true;
                let decision = reconnect.request(Trigger::ConnectionLost, hold);
                apply_decision(decision, Trigger::ConnectionLost, account.as_deref(), &job_tx, &mut workers, &mut model);
            }
            terminal.draw(|frame| render::draw(frame, &mut model))?;
            if !first_frame {
                first_frame = true;
                trace("first_frame");
            }
            tokio::select! {
                result = &mut shutdown => {
                    result?;
                    break;
                }
                event = input.next() => {
                    match event {
                        Some(Ok(event @ (Event::Key(_) | Event::Mouse(_)))) => {
                            let action = match event {
                                Event::Key(key) if key.kind != KeyEventKind::Release => model.key(key),
                                Event::Mouse(mouse) => model.mouse(mouse),
                                _ => Action::None,
                            };
                            match action {
                                Action::Quit => break,
                                Action::SkipShaders(app) => { if steam_core::shaders::skip(app) { model.status = "Skipping shader processing; continuing launch…".into(); } },
                                Action::CycleLaunchOption(app) => {
                                    model.status = match model.selected_game().and_then(|g| g.installed.clone()) {
                                        Some(game) if game.app_id == app => match steam_core::launch::options::cycle(&game) {
                                            Ok(option) => format!("Launch option [{app}]: {}", option.description),
                                            Err(error) => format!("Launch option [{app}]: {error}"),
                                        },
                                        _ => "Install the game before choosing a launch option".into(),
                                    };
                                }
                                Action::PrecacheShaders(app) => {
                                    model.status = match (&queue, model.selected_game().and_then(|g| g.installed.clone())) {
                                        (None, _) => "Persistent queue unavailable for this account".into(),
                                        (Some(queue), _) if !queue.settings().is_ok_and(|s| s.shaders_enabled) => "Shader pre-caching is disabled (steam-tui shaders enabled on)".into(),
                                        (Some(queue), Some(game)) if game.app_id == app => match queue.enqueue(steam_core::updates::Intent::shader(&game), true) {
                                            Ok(_) => format!("Queued Vulkan shader pre-cache [{app}]; press J to follow it"),
                                            Err(error) => format!("Cannot queue shader pre-cache [{app}]: {error}"),
                                        },
                                        _ => "Install the game before pre-caching its shaders".into(),
                                    };
                                }
                                Action::None => {},
                                Action::ResolveCloud => {
                                    if let (Some(modal), Some(connection), Some(account)) = (&model.cloud_conflicts, connection.clone(), account.clone()) {
                                        let (game, conflict, choice) = (modal.game.clone(), modal.items[modal.index].clone(), modal.choice.unwrap());
                                        let updates = job_tx.clone();
                                        cloud_jobs.push(tokio::task::spawn_local(async move {
                                            let result = steam_core::cloud::resolve_game(&connection, &game, &account, &conflict, choice).await.map(|r| r.summary()).map_err(|e| e.to_string());
                                            let _ = updates.send(JobEvent::CloudResolved { app_id: game.app_id, name: conflict.name, result }).await;
                                        }));
                                    } else if let Some(modal) = &mut model.cloud_conflicts {
                                        modal.busy = false;
                                        model.cloud_busy.remove(&modal.game.app_id);
                                        modal.error = Some("An online account connection is required".into());
                                    }
                                }
                                Action::News => {
                                    if let Some(news) = &model.news {
                                        let (request, app_id) = (news.request, news.app_id);
                                        let updates = job_tx.clone();
                                        workers.push(tokio::task::spawn_local(async move {
                                            let result = steam_core::news::fetch(app_id).await.map_err(|e| e.to_string());
                                            let _ = updates.send(JobEvent::News { request, result }).await;
                                        }));
                                    }
                                }
                                Action::Achievements => {
                                    if let Some(view) = &model.achievements {
                                        let (request, app_id, language) = (view.request, view.app_id, view.language.clone());
                                        let updates = job_tx.clone();
                                        if let Some(connection) = connection.clone() {
                                            workers.push(tokio::task::spawn_local(async move {
                                                let result = steam_core::achievements::fetch(&connection, app_id, &language).await;
                                                let _ = updates.send(JobEvent::Achievements { request, result }).await;
                                            }));
                                        } else {
                                            let _ = updates.try_send(JobEvent::Achievements {
                                                request,
                                                result: Err("Sign in (steam-tui login) and go online to see achievements".into()),
                                            });
                                        }
                                    }
                                }
                                Action::Refresh => {
                                    let broken = online
                                        && (connection.is_none() || !live.friends_open || !live.notifications_open);
                                    if broken {
                                        let decision = reconnect.request(Trigger::Manual, model.install_active);
                                        if decision == Decision::Ignored {
                                            model.status = "Already reconnecting to Steam…".into();
                                        }
                                        apply_decision(decision, Trigger::Manual, account.as_deref(), &job_tx, &mut workers, &mut model);
                                    } else {
                                        if !model.loading {
                                            model.loading = true;
                                            model.status = "Refreshing owned library and update metadata…".into();
                                            match connection.clone() {
                                                Some(connection) if !watchdog.recently_alive(Instant::now()) => {
                                                    spawn_preflight(connection, epoch, Then::Refresh, job_tx.clone(), &mut workers);
                                                }
                                                _ => run_then(Then::Refresh, connection.as_ref(), account.as_deref(), &mut model, &mut catalog_generation, &job_tx, &mut workers),
                                            }
                                        }
                                        if live.friends_open {
                                            match live.refresh_tx.try_send(()) {
                                                Ok(()) => model.friends_status = "Refreshing friend presence…".into(),
                                                Err(mpsc::error::TrySendError::Full(_)) => {},
                                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                                    model.friends_status = "Friends disconnected — press r to reconnect".into();
                                                }
                                            }
                                        }
                                    }
                                }
                                Action::Install => {
                                    if let (Some(connection), Some(game)) = (connection.clone(), model.selected_game().cloned()) {
                                        // Planning on a dead socket would hang: prove the session
                                        // answers first, unless something just did.
                                        if watchdog.recently_alive(Instant::now()) {
                                            prepare_install(connection, game, job_tx.clone(), &mut workers);
                                        } else {
                                            spawn_preflight(connection, epoch, Then::Install(Box::new(game)), job_tx.clone(), &mut workers);
                                        }
                                    } else {
                                        model.preparing_install = false;
                                        model.status = if online {
                                            "No Steam connection — press r to reconnect".into()
                                        } else {
                                            "An online account connection is required to install".into()
                                        };
                                    }
                                }
                                Action::CheckDestination => {
                                    let path = model.modal.as_ref().and_then(|modal| modal.custom.clone());
                                    if let Some(path) = path {
                                        let updates = job_tx.clone();
                                        workers.push(tokio::task::spawn_local(async move {
                                            let result = tokio::task::spawn_blocking(move || {
                                                steam_core::install::destination(std::path::Path::new(&path))
                                                    .map_err(|error| error.to_string())
                                            }).await.map_err(|error| error.to_string()).and_then(|result| result);
                                            let _ = updates.send(JobEvent::Destination(result)).await;
                                        }));
                                    }
                                }
                                Action::StartInstall => {
                                    if model.install_active { continue; }
                                    let Some(connection) = connection.clone() else {
                                        model.status = "Connection unavailable".into();
                                        continue;
                                    };
                                    let Some(account) = account.clone() else {
                                        model.status = "An account is required".into();
                                        continue;
                                    };
                                    let Some(modal) = model.modal.take() else { continue; };
                                    let Some(destination) = modal.selected_destination().map(|destination| destination.path.clone()) else { continue; };
                                    let app_id = modal.plan.app_id;
                                    let request = InstallRequest {
                                        plan: modal.plan, destination, steam_root: modal.steam_root,
                                        account, existing: modal.existing,
                                    };
                                    let Some(queue) = &queue else { model.status = "Persistent queue unavailable; cannot start a durable install".into(); continue; };
                                    let id = match queue.enqueue(steam_core::updates::Intent { app_id, steam_root: request.steam_root.clone(), destination: request.destination.clone(), dependency: None, shader: false, revision: None }, true) {
                                        Ok(id) => id, Err(e) => { model.status = format!("Cannot queue install: {e}"); continue; }
                                    };
                                    active_queue = Some(id);
                                    model.active_shader = false;
                                    model.active_install = Some((app_id, request.destination.clone()));
                                    model.install_active = true;
                                    model.cancel_requested = false;
                                    model.begin_install();
                                    watchdog.install_activity(Instant::now());
                                    model.status = format!("Install [{app_id}]: preparing transaction…");
                                    install_job = Some(spawn_queued_install(queue.clone(), id, app_id, connection, progress_tx.clone(), job_tx.clone()));
                                }
                                Action::CancelInstall => {
                                    if pending_update_launch.as_ref().is_some_and(|(job, _, _)| Some(*job) == active_queue) { pending_update_launch = None; }
                                    model.pending_launch = None;
                                    if let (Some(queue), Some(id)) = (&queue, active_queue)
                                        && let Err(error) = queue.control(id, steam_core::updates::JobState::Cancelled) { model.status = error.to_string(); }
                                    if let Some((cancel, pause, _)) = &install_job {
                                        cancel.cancel();
                                        pause.set(false);
                                    }
                                }
                                Action::TogglePause => {
                                    if let (Some(queue), Some(id)) = (&queue, active_queue) {
                                        if let Err(error) = queue.control(id, if model.paused { steam_core::updates::JobState::Paused } else { steam_core::updates::JobState::Pending }) { model.status = error.to_string(); }
                                    } else if let Some((_, pause, _)) = &install_job { pause.set(model.paused); }
                                }
                                Action::Queue => {
                                    if queue.is_none() { model.status = "Persistent queue unavailable for this account".into(); }
                                }
                                Action::QueueControl(id, state) => {
                                    if let Some(queue) = &queue {
                                        if let Err(e) = queue.control(id, state) { model.status = e.to_string(); }
                                        if state == steam_core::updates::JobState::Cancelled && pending_update_launch.as_ref().is_some_and(|(job, _, _)| *job == id) { pending_update_launch = None; }
                                    }
                                }
                                Action::QueuePriority(app, delta) => {
                                    if let Some(queue) = &queue {
                                        let result = (|| { let mut settings = queue.settings()?; let value = settings.priorities.entry(app).or_default(); *value = value.saturating_add(delta); queue.save_settings(settings) })();
                                        if let Err(error) = result { model.status = error.to_string(); }
                                    }
                                }
                                Action::QueuePolicy(app) => {
                                    if let Some(queue) = &queue {
                                        let result = (|| { let mut settings = queue.settings()?; let policy = settings.policies.entry(app).or_default();
                                            *policy = match *policy { steam_core::updates::UpdatePolicy::Automatic => steam_core::updates::UpdatePolicy::LaunchOnly, steam_core::updates::UpdatePolicy::LaunchOnly => steam_core::updates::UpdatePolicy::Manual, steam_core::updates::UpdatePolicy::Manual => steam_core::updates::UpdatePolicy::Automatic }; queue.save_settings(settings) })();
                                        if let Err(error) = result { model.status = error.to_string(); }
                                    }
                                }
                                Action::ContentProperties => {
                                    if let Some(view) = &mut model.properties {
                                        if let Some(connection) = connection.clone() {
                                            let game = view.game.clone(); let updates = job_tx.clone();
                                            workers.push(tokio::task::spawn_local(async move {
                                                let result = steam_core::content_options::properties(&connection, &game).await.map_err(|e| e.to_string());
                                                let _ = updates.send(JobEvent::ContentProperties { app_id: game.app_id, result }).await;
                                            }));
                                        } else { view.busy = false; view.error = Some("An online connection is required".into()); }
                                    }
                                }
                                Action::UnlockBranch => {
                                    if let Some(view) = &mut model.properties {
                                        if let (Some(connection), Some(queue), Some(data), Some(password)) = (connection.clone(), queue.clone(), &view.data, view.password.take()) {
                                            let game = view.game.clone(); let persist = view.remember_key;
                                            let desired = steam_core::catalog::ContentSelection { branch: data.branches.get(view.branch).map(|b| b.name.clone()), language: data.languages.get(view.language).cloned() };
                                            let updates = job_tx.clone();
                                            workers.push(tokio::task::spawn_local(async move {
                                                let result = async {
                                                    let branch = desired.branch.as_deref().ok_or_else(|| std::io::Error::other("No branch selected"))?;
                                                    steam_core::branches::unlock(&connection, game.app_id, branch, password, persist).await?;
                                                    steam_core::content_options::select(&connection, &queue, &game, desired).await
                                                }.await.map_err(|e: Box<dyn std::error::Error + Send + Sync>| e.to_string());
                                                let _ = updates.send(JobEvent::ContentChanged { app_id: game.app_id, result }).await;
                                            }));
                                        } else { view.busy = false; view.error = Some("Online connection and queue are required".into()); }
                                    }
                                }
                                action @ (Action::ApplyContent | Action::ToggleDlc(_, _)) => {
                                    if let Some(view) = &mut model.properties {
                                        if let (Some(connection), Some(queue), Some(data)) = (connection.clone(), queue.clone(), &view.data) {
                                            let desired = steam_core::catalog::ContentSelection { branch: data.branches.get(view.branch).map(|b| b.name.clone()), language: data.languages.get(view.language).cloned() };
                                            let game = view.game.clone(); let updates = job_tx.clone();
                                            workers.push(tokio::task::spawn_local(async move {
                                                let result = match action {
                                                    Action::ToggleDlc(id, enabled) => steam_core::content_options::select_dlc(&connection, &queue, &game, id, enabled).await,
                                                    _ => steam_core::content_options::select(&connection, &queue, &game, desired).await,
                                                }.map_err(|e| e.to_string());
                                                let _ = updates.send(JobEvent::ContentChanged { app_id: game.app_id, result }).await;
                                            }));
                                        } else { view.busy = false; view.error = Some("Online connection and update queue are required".into()); }
                                    }
                                }
                                Action::Launch(backend) => {
                                    if let Some(game) = model.selected_game().and_then(|g| g.installed.clone()) {
                                        if !model.launching.insert(game.app_id) { continue; }
                                        checking_updates.insert(game.app_id);
                                        model.status = format!("Checking updates for {}… (Shift+Enter launches installed version)", game.name);
                                        let updates = job_tx.clone(); let session = connection.clone();
                                        let selection = queue.as_ref().and_then(|q| q.settings().ok()).and_then(|s| s.selections.get(&game.app_id).cloned()).unwrap_or_default();
                                        workers.push(tokio::task::spawn_local(async move {
                                            let result = match session {
                                                Some(connection) => steam_core::updates::check_selected(&connection, &game, &selection).await.map_err(|e| e.to_string()),
                                                None => Err("Offline: freshness cannot be checked".into()),
                                            };
                                            let _ = updates.send(JobEvent::UpdateChecked { epoch, game, backend, result }).await;
                                        }));
                                    }
                                }
                                Action::LaunchInstalled(backend) => {
                                    if let Some(game) = model.selected_game().and_then(|game| game.installed.clone()) {
                                        if checking_updates.remove(&game.app_id) { model.launching.remove(&game.app_id); }
                                        if pending_update_launch.as_ref().is_some_and(|(_, app, _)| *app == game.app_id) {
                                            pending_update_launch = None;
                                            if let Some(queue) = &queue && let Ok(jobs) = queue.jobs() {
                                                for job in jobs.iter().filter(|j| j.intent.app_id == game.app_id && j.foreground && matches!(j.state, steam_core::updates::JobState::Pending | steam_core::updates::JobState::Paused)) { let _ = queue.control(job.id, steam_core::updates::JobState::Cancelled); }
                                            }
                                        }
                                        spawn_launch(game, connection.clone().zip(account.clone()), backend, &mut model, job_tx.clone(), &mut workers);
                                    }
                                }
                                Action::CloudSync => {
                                    let Some(game) = model.selected_game().and_then(|game| game.installed.clone()) else { continue };
                                    let app_id = game.app_id;
                                    if steam_core::launch::is_running(app_id) {
                                        model.cloud_busy.remove(&app_id);
                                        model.status = format!("{} is running; close it before syncing Steam Cloud", game.name);
                                    } else if let Some((connection, account)) = connection.clone().zip(account.clone()) {
                                        cloud_jobs.retain(|job| !job.is_finished());
                                        cloud_jobs.push(spawn_cloud(connection, account, game, job_tx.clone()));
                                    } else {
                                        model.cloud_busy.remove(&app_id);
                                        model.status = "Steam Cloud sync needs an online account connection".into();
                                    }
                                }
                            }
                        }
                        Some(Ok(Event::Resize(_, _))) => {},
                        Some(Ok(_)) => continue,
                        Some(Err(error)) => return Err(error.into()),
                        None => return Err("terminal input closed".into()),
                    }
                }
                update = live.friend_rx.recv(), if live.friends_open => {
                    match update {
                        Some(FriendEvent::Snapshot(friends)) => {
                            trace("friends_loaded");
                            model.set_friends(friends);
                        }
                        Some(FriendEvent::Status(message)) => model.friends_status = message,
                        None => live.friends_open = false,
                    }
                }
                Some(update) = live.presence_rx.recv() => {
                    match update {
                        PresenceEvent::Running(apps) => model.running_games = apps,
                        PresenceEvent::Status(message) => model.status = message,
                    }
                }
                update = live.notification_rx.recv(), if live.notifications_open => {
                    match update {
                        Some(event) => model.notification_event(event),
                        None => live.notifications_open = false,
                    }
                }
                Some(progress) = progress_rx.recv() => {
                    watchdog.install_activity(Instant::now());
                    model.apply_progress(progress);
                }
                _ = tick.tick() => {}
                Some(update) = job_rx.recv() => {
                    match update {
                        JobEvent::CloudConflicts { game, items } => {
                            let modal = model::CloudConflicts { game, items, index: 0, choice: None, busy: false, error: None };
                            if model.cloud_conflicts.is_some() { model.cloud_conflict_queue.push_back(modal); } else { model.cloud_conflicts = Some(modal); }
                        }
                        JobEvent::CloudResolved { app_id, name, result } => {
                            model.cloud_busy.remove(&app_id);
                            let ok = result.is_ok();
                            model.status = result.as_ref().cloned().unwrap_or_else(|e| format!("Cloud conflict: {e}"));
                            if let Some(modal) = &mut model.cloud_conflicts && modal.game.app_id == app_id {
                                modal.busy = false;
                                modal.choice = None;
                                match result {
                                    Ok(_) => { modal.items.retain(|i| i.name != name); modal.index = modal.index.min(modal.items.len().saturating_sub(1)); },
                                    Err(error) => modal.error = Some(error),
                                }
                                if modal.items.is_empty() { model.cloud_conflicts = model.cloud_conflict_queue.pop_front(); }
                            }
                            model.cloud_last.insert(app_id, if ok && !model.cloud_conflicts.as_ref().is_some_and(|m| m.game.app_id == app_id) && !model.cloud_conflict_queue.iter().any(|m| m.game.app_id == app_id) { CloudResult::Ok } else if ok { CloudResult::Conflicts } else { CloudResult::Failed });
                            let games = model.games.iter().filter(|g| g.app_id == app_id).filter_map(|g| g.installed.clone()).collect();
                            spawn_badges(games, account.as_deref(), connection.as_ref().map(|c| c.steam_id().account_id()), job_tx.clone(), &mut workers);
                        }
                        JobEvent::Achievements { request, result } => {
                            if let Some(view) = &mut model.achievements && view.request == request {
                                view.result = Some(result);
                                view.reselect(None);
                            }
                        }
                        JobEvent::News { request, result } => {
                            if let Some(news) = &mut model.news && news.request == request {
                                news.result = Some(result);
                            }
                        }
                        JobEvent::Interim(generation, _, _) | JobEvent::Catalog(generation, _, _) if generation != catalog_generation => {},
                        JobEvent::Interim(_, source, catalog) => {
                            trace("catalog_interim");
                            model.show_interim(catalog, source);
                        }
                        JobEvent::Catalog(_, Ok(catalog), completion) => {
                            trace("catalog_live");
                            if let Some(queue) = &queue {
                                for row in &catalog.items {
                                    if let Some(game) = &row.installed && queue.settings().is_ok_and(|s| s.shaders_enabled) && steam_core::shaders::state(queue, game.app_id).is_ok_and(|s| s.manifest != 0) && steam_core::shaders::due(queue, game.app_id).unwrap_or(false)
                                        && let Err(error) = queue.enqueue(steam_core::updates::Intent::shader(game), false) { model.warnings.push(format!("Cannot queue shaders [{}]: {error}", game.app_id)); }
                                    if matches!(row.update, steam_core::catalog::UpdateStatus::Available { .. }) && let Some(game) = &row.installed
                                        && let Err(error) = row.plan.as_ref().map_or_else(|| Ok(steam_core::updates::Intent::installed(game)), |plan| steam_core::updates::Intent::catalog(game, plan)).and_then(|intent| queue.enqueue(intent, false))
                                    { model.warnings.push(format!("Cannot queue update [{}]: {error}", row.app_id)); }
                                }
                            }
                            model.set_catalog(catalog);
                            release_memory();
                            if let Some(message) = completion { model.status = message; }
                            let games: Vec<_> = model.games.iter().filter_map(|g| g.installed.clone()).collect();
                            spawn_badges(games, account.as_deref(), connection.as_ref().map(|c| c.steam_id().account_id()), job_tx.clone(), &mut workers);
                        }
                        JobEvent::Catalog(_, Err(error), completion) => {
                            model.loading = false;
                            release_memory();
                            let local = if model.games.is_empty() { "" } else { " · showing local/cached rows only" };
                            model.status = format!("{}Catalog refresh failed: {error}{local}", completion.map(|message| format!("{message}; ")).unwrap_or_default());
                            if is_connection_error(&error) {
                                reconnect_request = Some(Trigger::JobFailed);
                            }
                            model.inventory_error = Some(error);
                        }
                        JobEvent::ContentProperties { app_id, result } => {
                            if let Some(view) = &mut model.properties && view.game.app_id == app_id {
                                view.busy = false;
                                match result {
                                    Ok(data) => {
                                        let desired = queue.as_ref().and_then(|q| q.settings().ok()).and_then(|s| s.selections.get(&app_id).cloned()).unwrap_or_default();
                                        let branch = desired.branch.as_deref().unwrap_or(&view.game.branch);
                                        let language = desired.language.as_deref().unwrap_or(&view.game.language);
                                        view.branch = data.branches.iter().position(|b| b.name == branch).unwrap_or(0);
                                        view.language = data.languages.iter().position(|l| l == language).unwrap_or(0);
                                        view.data = Some(data);
                                    }
                                    Err(e) => view.error = Some(e),
                                }
                            }
                        }
                        JobEvent::ContentChanged { app_id, result } => {
                            match result {
                                Ok(id) => { model.properties = None; model.status = format!("Content change queued as job {id}"); }
                                Err(e) => if let Some(view) = &mut model.properties && view.game.app_id == app_id { view.busy = false; view.error = Some(e); },
                            }
                        }
                        JobEvent::QueueIdle => {
                            active_queue = None;
                            if let Some((_, _, job)) = install_job.take() { let _ = job.await; }
                            model.active_shader = false;
                            model.install_active = false; model.active_install = None; model.clear_progress();
                        }
                        JobEvent::UpdateChecked { epoch: checked_epoch, game, backend, result } => {
                            if !checking_updates.remove(&game.app_id) { continue; }
                            model.launching.remove(&game.app_id);
                            if checked_epoch != epoch { model.status = "Account connection changed; retry launch".into(); continue; }
                            match result {
                                Ok(steam_core::catalog::UpdateStatus::UpToDate) => spawn_launch(game, connection.clone().zip(account.clone()), backend, &mut model, job_tx.clone(), &mut workers),
                                Ok(steam_core::catalog::UpdateStatus::Available { .. }) => {
                                    if let Some(queue) = &queue {
                                        match queue.enqueue(steam_core::updates::Intent::installed(&game), true) {
                                            Ok(id) => {
                                                pending_update_launch = Some((id, game.app_id, backend));
                                                if model.active_shader && let Some((cancel, pause, _)) = &install_job { cancel.cancel(); pause.set(false); }
                                                model.status = format!("Updating {} before launch…", game.name);
                                            },
                                            Err(e) => model.status = format!("Cannot queue update: {e}; Shift+Enter launches installed version"),
                                        }
                                    } else { model.status = "Update required, but queue is unavailable; use u or Shift+Enter".into(); }
                                }
                                Ok(_) => model.status = "Update freshness is unknown; Shift+Enter explicitly launches installed version".into(),
                                Err(error) => model.status = format!("Update check failed: {error}; Shift+Enter explicitly launches installed version"),
                            }
                        }
                        JobEvent::Modal(result) => {
                            model.preparing_install = false;
                            match result {
                                Ok(modal) => model.modal = Some(*modal),
                                Err(error) if error == PLAN_TIMEOUT_MESSAGE => {
                                    restored_note = Some("Install planning timed out; reconnected — press i to retry");
                                    reconnect_request = Some(Trigger::JobFailed);
                                    model.status = "Install planning timed out".into();
                                }
                                Err(error) => {
                                    if is_connection_error(&error) {
                                        reconnect_request = Some(Trigger::JobFailed);
                                    }
                                    model.status = format!("Cannot install: {error}");
                                }
                            }
                        }
                        JobEvent::Reconnected { trigger, result } => match result {
                            Ok(new) => restored = Some((trigger, new)),
                            Err(error) => {
                                let retry = reconnect.failed(is_permanent_failure(&error));
                                let what = if trigger == Trigger::Startup { "sign-in" } else { "reconnect" };
                                if trigger == Trigger::Startup {
                                    model.friends_status = format!("Friends unavailable: {error}");
                                    model.notif_status = format!("Notifications unavailable: {error}");
                                }
                                match (retry, account.as_deref()) {
                                    (Some(delay), Some(name)) => {
                                        model.status = format!("Steam {what} failed: {error} — retrying in {}s", delay.as_secs());
                                        spawn_reconnect(name.to_owned(), trigger, delay, job_tx.clone(), &mut workers);
                                    }
                                    _ => {
                                        // Giving up: release whatever was waiting on the new session.
                                        after_reconnect = None;
                                        restored_note = None;
                                        model.preparing_install = false;
                                        model.loading = false;
                                        model.status = format!("Steam {what} failed: {error} — press r to retry");
                                    }
                                }
                            }
                        },
                        JobEvent::Probe { epoch: probe_epoch, kind, alive } => {
                            // A probe of a replaced connection says nothing about the new one.
                            if probe_epoch == epoch
                                && watchdog.finished(kind, alive, Instant::now()) == Outcome::Dead
                            {
                                    match kind {
                                        ProbeKind::Idle => {
                                            lost_noted = true;
                                            reconnect_request = Some(Trigger::ConnectionLost);
                                        }
                                        ProbeKind::ScanStall => {
                                            restored_note = Some("Library refresh stalled; reconnected — press r to retry");
                                            reconnect_request = Some(Trigger::JobFailed);
                                        }
                                        ProbeKind::InstallStall => {
                                            // The job must still be awaited: cancel it and let
                                            // its completion trigger the reconnect.
                                            if let Some((cancel, pause, _)) = &install_job {
                                                cancel.cancel();
                                                pause.set(false);
                                                stall_cancel = true;
                                                model.cancel_requested = true;
                                                restored_note = Some("Connection lost during install — reconnected, press i to resume");
                                                model.status = "Connection lost during install — cancelling safely, then reconnecting…".into();
                                            }
                                        }
                                    }
                            }
                        }
                        JobEvent::Preflight { epoch: probe_epoch, alive, then } => {
                            // A replaced connection was already refreshed by the reconnect.
                            if alive || probe_epoch != epoch {
                                run_then(then, connection.as_ref(), account.as_deref(), &mut model, &mut catalog_generation, &job_tx, &mut workers);
                            } else {
                                after_reconnect = Some(then);
                                reconnect_request = Some(Trigger::Manual);
                            }
                        }
                        JobEvent::Destination(result) => {
                            if let Some(modal) = &mut model.modal {
                                modal.checking = false;
                                match result {
                                    Ok(destination) => {
                                        modal.selected = modal.destinations.iter().position(|existing| existing.path == destination.path)
                                            .unwrap_or(modal.destinations.len());
                                        if modal.selected == modal.destinations.len() { modal.destinations.push(destination); }
                                        else { modal.destinations[modal.selected] = destination; }
                                        modal.custom = None;
                                        modal.low_space_confirmed = false;
                                        modal.error = modal.blocked_reason();
                                    }
                                    Err(error) => modal.error = Some(error),
                                }
                            }
                        }
                        JobEvent::Installed { app_id, shader, result } => {
                            let finished_queue = active_queue.take();
                            if let Some((_, _, job)) = install_job.take() { let _ = job.await; }
                            release_memory();
                            model.install_active = false;
                            model.active_shader = false;
                            model.active_install = None;
                            // Consume this job's queued reports before a new job can start.
                            while let Ok(progress) = progress_rx.try_recv() { model.apply_progress(progress); }
                            let stalled = std::mem::take(&mut stall_cancel);
                            let installed_ok = result.is_ok();
                            if shader {
                                model.clear_progress();
                                model.status = match result { Ok(_) => format!("Shader pre-cache [{app_id}] ready; processing runs before launch"), Err(error) => format!("Shader pre-cache [{app_id}]: {error}") };
                                model.cancel_requested = false;
                                continue;
                            }
                            if pending_update_launch.as_ref().is_some_and(|(job, _, _)| Some(*job) == finished_queue) {
                                let (_, _, backend) = pending_update_launch.take().unwrap();
                                if let Ok(game) = &result {
                                    spawn_launch(game.clone(), connection.clone().zip(account.clone()), backend, &mut model, job_tx.clone(), &mut workers);
                                }
                            }
                            let message = match result {
                                Ok(game) => {
                                    model.installed(game);
                                    model.complete_install();
                                    format!("Install [{app_id}] finished; refreshing library")
                                }
                                Err(error) => {
                                    model.clear_progress();
                                    // Any non-cancel failure may mean a silently dead session;
                                    // so does a stall cancel we issued ourselves.
                                    if stalled || !model.cancel_requested {
                                        reconnect_request = Some(Trigger::JobFailed);
                                    }
                                    if stalled {
                                        format!("Install [{app_id}] interrupted: Steam connection lost (verified data is kept)")
                                    } else {
                                        format!("Install [{app_id}] {}: {error}",
                                            if model.cancel_requested { "ended after cancellation request" } else { "failed" })
                                    }
                                }
                            };
                            if model.pending_launch.is_some() || app_id == 1628350 {
                                debug_log(&format!("install [{app_id}] -> {message}"));
                            }
                            if let Some((game, backend)) = model.pending_launch.take_if(|_| installed_ok) {
                                spawn_launch(game, connection.clone().zip(account.clone()), backend, &mut model, job_tx.clone(), &mut workers);
                            } else {
                                model.pending_launch = None;
                            }
                            model.install_results.insert(app_id, message.clone());
                            model.status = message.clone();
                            model.cancel_requested = false;
                            model.loading = true;
                            catalog_generation += 1;
                            scan(ScanJob { connection: connection.clone(), account: account.clone(), completion: Some(message), generation: catalog_generation, active_library: None, interim: false, awaiting_login: false }, job_tx.clone(), &mut workers);
                        }
                        JobEvent::NeedsTool { game, dependency, backend } => {
                            model.launching.remove(&game.app_id);
                            match (connection.clone(), account.clone()) {
                                (Some(connection), Some(account)) if !model.install_active && !model.preparing_install && model.pending_launch.is_none() => {
                                    model.status = format!("{}: {dependency}; installing first…", game.name);
                                    model.preparing_install = true;
                                    model.pending_launch = Some((*game.clone(), backend));
                                    let updates = job_tx.clone();
                                    workers.push(tokio::task::spawn_local(async move {
                                        let result = dependency.install_request(&connection, &game, &account)
                                            .await.map_err(|e| e.to_string());
                                        let _ = updates.send(JobEvent::ToolPlan { epoch, result, requirement: dependency }).await;
                                    }));
                                }
                                _ => {
                                    let message = format!("Launch [{}] needs {dependency}; an online account and no running install are required to fetch it", game.app_id);
                                    model.status = message.clone();
                                    model.launches.insert(game.app_id, message);
                                }
                            }
                        }
                        JobEvent::ToolPlan { epoch: planned_epoch, result, requirement } => match (result, connection.clone()) {
                            (Ok(request), Some(connection)) if planned_epoch == epoch && !model.install_active && model.pending_launch.is_some() => {
                                model.preparing_install = false;
                                let app_id = request.plan.app_id;
                                let Some(queue) = &queue else { model.preparing_install = false; model.pending_launch = None; model.status = "Dependency queue unavailable".into(); continue; };
                                let parent = model.pending_launch.as_ref().unwrap().0.app_id;
                                let intent = steam_core::updates::Intent { app_id, steam_root: request.steam_root.clone(), destination: request.destination.clone(), shader: false, dependency: Some(steam_core::updates::DependencyIntent { parent_app_id: parent, requirement }), revision: None };
                                let id = match queue.enqueue(intent, true) { Ok(id) => id, Err(e) => { model.preparing_install = false; model.pending_launch = None; model.status = e.to_string(); continue; } };
                                active_queue = Some(id);
                                model.active_shader = false;
                                model.active_install = Some((app_id, request.destination.clone()));
                                model.install_active = true;
                                model.cancel_requested = false;
                                model.begin_install();
                                watchdog.install_activity(Instant::now());
                                model.status = format!("Install [{app_id}]: preparing dependency…");
                                install_job = Some(spawn_queued_install(queue.clone(), id, app_id, connection, progress_tx.clone(), job_tx.clone()));
                            }
                            (Err(error), _) => {
                                model.preparing_install = false;
                                model.pending_launch = None;
                                debug_log(&format!("tool plan failed: {error}"));
                                model.status = format!("Dependency install unavailable: {error}");
                            }
                            _ => {
                                model.preparing_install = false;
                                model.pending_launch = None;
                                model.status = "Dependency install could not start (busy or disconnected)".into();
                            }
                        },
                        JobEvent::Backend(event) => {
                            if matches!(event.channel, "Setup" | "Shaders" | "Media") && model.launching.contains(&event.app_id) {
                                model.status = format!("Launch [{}]: {}", event.app_id, event.title);
                            }
                            model.push_backend(event);
                        },
                        JobEvent::Cloud { app_id, message } => {
                            if model.launching.contains(&app_id) || model.cloud_busy.contains(&app_id) {
                                model.status = message;
                            }
                        }
                        JobEvent::CloudDone { app_id, message } => {
                            model.cloud_busy.remove(&app_id);
                            model.status = message;
                        }
                        JobEvent::CloudOutcome { app_id, result } => {
                            model.cloud_last.insert(app_id, result);
                            // The sync changed state files: refresh this game's badge.
                            let games: Vec<_> = model.games.iter().filter(|g| g.app_id == app_id).filter_map(|g| g.installed.clone()).collect();
                            spawn_badges(games, account.as_deref(), connection.as_ref().map(|c| c.steam_id().account_id()), job_tx.clone(), &mut workers);
                        }
                        JobEvent::CloudBadges(badges) => {
                            for (app_id, badge) in badges {
                                match badge {
                                    Some(badge) => { model.cloud_badges.insert(app_id, badge); }
                                    None => { model.cloud_badges.remove(&app_id); }
                                }
                            }
                        }
                        JobEvent::Launch { app_id, result, cloud } => {
                            model.launching.remove(&app_id);
                            let message = match result {
                                Ok((pid, description)) => format!("Launched [{app_id}] PID {pid}: {description}"),
                                Err(error) => format!("Launch [{app_id}] failed: {error}"),
                            };
                            let message = match cloud {
                                Some(note) => format!("{note} · {message}"),
                                None => message,
                            };
                            model.status = message.clone();
                            model.launches.insert(app_id, message);
                        }
                    }
                }
            }
        }
        Ok(())
    }.await;
    if let Some((cancel, _, mut job)) = install_job {
        cancel.cancel();
        model.cancel_requested = true;
        model.status = "Closing: waiting for installer to cancel safely or finish commit…".into();
        loop {
            let _ = terminal.draw(|frame| render::draw(frame, &mut model));
            tokio::select! {
                _ = &mut job => break,
                Some(progress) = progress_rx.recv() => model.apply_progress(progress),
                Some(_) = job_rx.recv() => {},
            }
        }
    }
    // Uploads cannot be abandoned midway through a batch. Individual live API
    // requests have deadlines; drain events while waiting so senders cannot
    // deadlock on a full channel after the interactive loop has stopped.
    for mut job in cloud_jobs {
        model.status = "Closing: waiting for Steam Cloud to finish…".into();
        let _ = terminal.draw(|frame| render::draw(frame, &mut model));
        loop {
            tokio::select! {
                _ = &mut job => break,
                Some(_) = job_rx.recv() => {},
                _ = tick.tick() => {
                    let _ = terminal.draw(|frame| render::draw(frame, &mut model));
                }
            }
        }
    }
    result
}
