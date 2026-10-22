use super::keys::{self, Cmd, Key, Nav};
use steam_core::launch::LaunchBackend;
/// Source of provisional rows shown while the live catalog loads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Interim {
    Installed,
    Cached,
}

use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use steam_core::catalog::{CatalogItem, CatalogSnapshot, InstallPlan, UpdateStatus};
use steam_core::cloud::CloudBadge;
use steam_core::friends::Friend;
use steam_core::install::{Destination, InstallPhase, InstallProgress};
use steam_core::installed::InstalledGame;
use steam_core::notifications::{CountKind, Notification, NotificationEvent};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Sort {
    Name,
    Size,
    LastPlayed,
    PlayTime,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
            Self::LastPlayed => "last played",
            Self::PlayTime => "play time",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Name => Self::Size,
            Self::Size => Self::LastPlayed,
            Self::LastPlayed => Self::PlayTime,
            Self::PlayTime => Self::Name,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Focus {
    Library,
    Friends,
}

impl Focus {
    pub fn next(self) -> Self {
        match self {
            Self::Library => Self::Friends,
            Self::Friends => Self::Library,
        }
    }
    pub fn previous(self) -> Self {
        self.next()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LibraryFilter {
    All,
    Installed,
    NotInstalled,
}

impl LibraryFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Installed => "Installed",
            Self::NotInstalled => "Not installed",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::All => Self::Installed,
            Self::Installed => Self::NotInstalled,
            Self::NotInstalled => Self::All,
        }
    }
}

pub(super) struct InstallModal {
    pub plan: InstallPlan,
    pub existing: Option<InstalledGame>,
    pub steam_root: PathBuf,
    pub destinations: Vec<Destination>,
    pub selected: usize,
    pub custom: Option<String>,
    pub checking: bool,
    pub low_space_confirmed: bool,
    pub error: Option<String>,
}

impl InstallModal {
    pub fn selected_destination(&self) -> Option<&Destination> {
        self.destinations.get(self.selected)
    }

    pub fn blocked_reason(&self) -> Option<String> {
        let destination = self.selected_destination()?;
        if !destination.available {
            return Some(
                destination
                    .message
                    .clone()
                    .unwrap_or_else(|| "Destination unavailable".into()),
            );
        }
        if !self.low_space_confirmed
            && destination
                .free_bytes
                .is_some_and(|free| free < self.plan.installed_bytes)
        {
            return Some(
                "Free space is below the full game size; resumable staged files may still fit. Press Enter again to let the installer check exactly".into(),
            );
        }
        None
    }
}

/// Download speed from successive `downloaded_bytes` samples: the average over a
/// sliding window, so a single bursty report neither spikes nor stalls the readout.
/// Time is always passed in, keeping the tracker deterministic.
#[derive(Debug, Default)]
pub(super) struct SpeedTracker {
    samples: VecDeque<(Instant, u64)>,
    started: Option<Instant>,
    frozen: Option<Instant>,
    paused_at: Option<Instant>,
    paused_total: Duration,
}

const SPEED_WINDOW: Duration = Duration::from_secs(5);
const MIN_SPEED_SPAN: Duration = Duration::from_secs(1);
/// ETAs beyond this are noise from a near-zero rate.
const MAX_ETA: Duration = Duration::from_secs(100 * 24 * 3600);

impl SpeedTracker {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Begins a fresh job at `now`.
    pub fn start(&mut self, now: Instant) {
        *self = Self {
            started: Some(now),
            ..Self::default()
        };
    }

    pub fn record(&mut self, now: Instant, downloaded_bytes: u64) {
        if self.frozen.is_some() || self.paused_at.is_some() {
            return;
        }
        if self
            .samples
            .back()
            .is_some_and(|&(_, last)| downloaded_bytes < last)
        {
            self.samples.clear();
        }
        self.started.get_or_insert(now);
        self.samples.push_back((now, downloaded_bytes));
        // Keep exactly one sample at or before the window edge as its anchor.
        if let Some(cutoff) = now.checked_sub(SPEED_WINDOW) {
            while self.samples.len() > 1 && self.samples[1].0 <= cutoff {
                self.samples.pop_front();
            }
        }
    }

    /// Stops the clock and hides the speed (finished or cancelled).
    pub fn freeze(&mut self, now: Instant) {
        self.resume(now);
        self.frozen.get_or_insert(now);
    }

    /// Bytes per second; `None` when frozen, too little history, or stalled.
    pub fn speed(&self, now: Instant) -> Option<f64> {
        if self.frozen.is_some() || self.paused_at.is_some() {
            return None;
        }
        let &(first_at, first_bytes) = self.samples.front()?;
        let &(_, last_bytes) = self.samples.back()?;
        let span = now.saturating_duration_since(first_at);
        if span < MIN_SPEED_SPAN {
            return None;
        }
        let rate = last_bytes.saturating_sub(first_bytes) as f64 / span.as_secs_f64();
        (rate > 0.0).then_some(rate)
    }

    pub fn eta(&self, now: Instant, remaining_bytes: u64) -> Option<Duration> {
        if remaining_bytes == 0 {
            return Some(Duration::ZERO);
        }
        let seconds = remaining_bytes as f64 / self.speed(now)?;
        let eta = Duration::try_from_secs_f64(seconds).ok()?;
        (eta <= MAX_ETA).then_some(eta)
    }

    /// Time spent running, excluding paused spans.
    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        let started = self.started?;
        let end = self.paused_at.or(self.frozen).unwrap_or(now);
        Some(
            end.saturating_duration_since(started)
                .saturating_sub(self.paused_total),
        )
    }

    /// Stops counting time and speed until `resume`.
    pub fn pause(&mut self, now: Instant) {
        if self.frozen.is_none() {
            self.paused_at.get_or_insert(now);
        }
    }

    /// Restarts the speed window so paused time never dilutes the rate.
    pub fn resume(&mut self, now: Instant) {
        if let Some(since) = self.paused_at.take() {
            self.paused_total += now.saturating_duration_since(since);
            self.samples.clear();
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Action {
    None,
    Quit,
    Clipboard(u64),
    Refresh,
    Launch(LaunchBackend),
    LaunchInstalled(LaunchBackend),
    SkipShaders(u32),
    PrecacheShaders(u32),
    CycleLaunchOption(u32),
    Queue,
    QueueControl(u64, steam_core::updates::JobState),
    QueuePriority(u32, i16),
    QueuePolicy(u32),
    ContentProperties,
    ApplyContent,
    UnlockBranch,
    ToggleDlc(u32, bool),
    Install,
    /// Manual Steam Cloud sync (download, then upload) of the selected game.
    CloudSync,
    ResolveCloud,
    News,
    OpenNewsLink(usize),
    /// Open or reload the achievements viewer of the selected game.
    Achievements,
    StartInstall,
    CheckDestination,
    CancelInstall,
    TogglePause,
}

pub(super) struct ContentProperties {
    pub game: InstalledGame,
    pub password: Option<zeroize::Zeroizing<String>>,
    pub remember_key: bool,
    pub data: Option<steam_core::content_options::Properties>,
    pub branch: usize,
    pub language: usize,
    pub selected: usize,
    pub busy: bool,
    pub error: Option<String>,
}

/// One line of the backend traffic log: an event and how many identical ones followed it.
pub(super) struct BackendEntry {
    pub event: steam_core::backend::Event,
    pub count: u32,
    pub last_at: std::time::SystemTime,
}

pub(super) struct NewsView {
    pub request: u64,
    pub app_id: u32,
    pub name: String,
    pub result: Option<Result<Vec<steam_core::news::NewsItem>, String>>,
    pub offset: u16,
    pub document: Option<Vec<super::news_content::Block>>,
    pub links: Vec<(Rect, String)>,
    pub link_error: Option<String>,
}

pub(super) struct CloudConflicts {
    pub game: InstalledGame,
    pub items: Vec<steam_core::cloud::Conflict>,
    pub index: usize,
    pub choice: Option<steam_core::cloud::Choice>,
    pub busy: bool,
    pub error: Option<String>,
}

pub(super) struct Model {
    pub games: Vec<CatalogItem>,
    names: Vec<String>,
    pub order: Vec<usize>,
    pub selected: usize,
    pub library_offset: usize,
    pub library_rows: usize,
    /// Hit targets from the last frame, including its filter, scroll and divider.
    pub library_hits: Vec<(Rect, usize)>,
    pub news: Option<NewsView>,
    pub news_images: super::chat_images::Images,
    pub achievements: Option<super::achievements::AchievementsView>,
    pub cloud_conflicts: Option<CloudConflicts>,
    pub cloud_conflict_queue: VecDeque<CloudConflicts>,
    news_request: u64,
    achievements_request: u64,
    last_library_click: Option<(u32, Instant)>,
    pub friends: Vec<Friend>,
    pub friend_order: Vec<usize>,
    pub friend_offset: usize,
    pub friend_selected: usize,
    pub chat: super::chat::Chat,
    pub library_icons: super::library_icons::LibraryIcons,
    pub friend_rows: usize,
    pub show_offline: bool,
    pub focus: Focus,
    pub sort: Sort,
    pub reverse: bool,
    /// Pinned app IDs sort above everything else.
    pub pins: std::collections::BTreeSet<u32>,
    pub search: String,
    pub searching: bool,
    pub help: bool,
    pub loading: bool,
    pub inventory_error: Option<String>,
    /// True once the disk inventory (or a catalog) has produced rows.
    pub inventory_ready: bool,
    pub warnings: Vec<String>,
    pub friends_status: String,
    /// All available account history, newest first.
    pub notifications: Vec<Notification>,
    /// Non-zero counts; each update replaces the whole summary.
    pub notif_counts: Vec<(CountKind, u32)>,
    /// Offline/error text; empty while the live feed is healthy.
    pub notif_status: String,
    /// False without a Steam connection (offline mode or failed login).
    pub notifications_live: bool,
    pub account: Option<String>,
    pub profile: Option<Friend>,
    pub profile_name: Option<String>,
    pub profile_live: bool,
    pub notif_offset: usize,
    pub notif_rows: usize,
    /// Items received since the feed was last paged.
    pub notif_unseen: usize,
    pub notif_fresh: std::collections::HashSet<u64>,
    pub status: String,
    pub launches: HashMap<u32, String>,
    pub running_games: std::collections::BTreeSet<u32>,
    pub launching: std::collections::HashSet<u32>,
    /// Games with a manual Steam Cloud sync in flight.
    pub cloud_busy: std::collections::HashSet<u32>,
    /// Steam Cloud badge per installed game with cloud configured; computed off
    /// the UI thread at load and after each sync.
    pub cloud_badges: HashMap<u32, CloudBadge>,
    pub cloud_last: HashMap<u32, CloudResult>,
    /// Game to launch once its missing runtime finishes installing.
    pub pending_launch: Option<(InstalledGame, LaunchBackend)>,
    pub install_results: HashMap<u32, String>,
    pub help_offset: u16,
    /// Traffic of the per-launch Steam backend, oldest first (capped).
    pub backend_log: std::collections::VecDeque<BackendEntry>,
    pub backend_view: bool,
    pub queue_view: bool,
    pub queue_filter: Option<super::downloads::Kind>,
    pub queue_run: Option<u64>,
    pub queue_error: Option<String>,
    pub queue_load_error: Option<String>,
    pub queue_names: HashMap<u32, String>,
    pub queue_active: Option<u64>,
    pub queue_details_offset: u16,
    pub queue_jobs: Vec<steam_core::updates::Job>,
    pub queue_selected: usize,
    pub queue_settings: steam_core::updates::Settings,
    pub properties: Option<ContentProperties>,
    /// Lines scrolled up from the newest entry; 0 follows the tail.
    pub backend_offset: usize,
    pub filter: LibraryFilter,
    pub modal: Option<InstallModal>,
    pub preparing_install: bool,
    pub install_active: bool,
    pub active_shader: bool,
    pub active_install: Option<(u32, PathBuf)>,
    pub cancel_requested: bool,
    pub paused: bool,
    pub progress: Option<InstallProgress>,
    pub speed: SpeedTracker,
    /// Rate of whole-file verification (hashed bytes per second).
    pub verify_speed: SpeedTracker,
}

impl Model {
    pub fn new(offline: bool, connection_error: Option<String>) -> Self {
        let notifications_live = !offline && connection_error.is_none();
        let (friends_status, notif_status) = if offline {
            (
                "Offline mode — no Steam connection".to_owned(),
                "Offline — no Steam notifications".to_owned(),
            )
        } else if let Some(error) = connection_error {
            (
                format!("Friends unavailable: {error}"),
                format!("Notifications unavailable: {error}"),
            )
        } else {
            ("Loading friends…".to_owned(), String::new())
        };
        Self {
            games: Vec::new(),
            names: Vec::new(),
            order: Vec::new(),
            selected: 0,
            library_offset: 0,
            library_rows: 1,
            library_hits: Vec::new(),
            news: None,
            news_images: Default::default(),
            achievements: None,
            cloud_conflicts: None,
            cloud_conflict_queue: VecDeque::new(),
            news_request: 0,
            achievements_request: 0,
            last_library_click: None,
            friends: Vec::new(),
            friend_order: Vec::new(),
            friend_offset: 0,
            friend_selected: 0,
            library_icons: super::library_icons::LibraryIcons::new(!offline),
            chat: super::chat::Chat {
                available: notifications_live,
                status: if notifications_live {
                    String::new()
                } else {
                    "Chat unavailable — no Steam connection".into()
                },
                ..Default::default()
            },
            friend_rows: 1,
            account: None,
            profile: None,
            profile_name: None,
            profile_live: false,
            show_offline: false,
            focus: Focus::Library,
            sort: Sort::Name,
            reverse: false,
            pins: Default::default(),
            search: String::new(),
            searching: false,
            help: false,
            loading: true,
            inventory_error: None,
            inventory_ready: false,
            warnings: Vec::new(),
            help_offset: 0,
            backend_log: std::collections::VecDeque::new(),
            backend_view: false,
            queue_view: false,
            queue_filter: None,
            queue_run: None,
            queue_error: None,
            queue_load_error: None,
            queue_names: HashMap::new(),
            queue_active: None,
            queue_details_offset: 0,
            queue_jobs: Vec::new(),
            queue_selected: 0,
            queue_settings: Default::default(),
            properties: None,
            backend_offset: 0,
            friends_status,
            notifications: Vec::new(),
            notif_counts: Vec::new(),
            notif_status,
            notifications_live,
            notif_offset: 0,
            notif_rows: 1,
            notif_unseen: 0,
            notif_fresh: Default::default(),
            status: "Scanning installed games…".to_owned(),
            launches: HashMap::new(),
            running_games: Default::default(),
            launching: std::collections::HashSet::new(),
            cloud_busy: std::collections::HashSet::new(),
            cloud_badges: HashMap::new(),
            cloud_last: HashMap::new(),
            pending_launch: None,
            install_results: HashMap::new(),
            filter: LibraryFilter::All,
            modal: None,
            preparing_install: false,
            install_active: false,
            active_shader: false,
            active_install: None,
            cancel_requested: false,
            progress: None,
            speed: SpeedTracker::default(),
            verify_speed: SpeedTracker::default(),
            paused: false,
        }
    }

    /// Clears the previous job's progress and starts the elapsed clock.
    pub fn begin_install(&mut self) {
        self.begin_install_at(Instant::now());
    }

    fn begin_install_at(&mut self, now: Instant) {
        self.progress = None;
        self.paused = false;
        self.speed.start(now);
        self.verify_speed.reset();
    }

    /// Pause/resume only holds the download phase; after that the job runs to its end.
    fn toggle_pause_at(&mut self, now: Instant) -> Action {
        if self.cancel_requested {
            return Action::None;
        }
        if self.progress.as_ref().is_some_and(|progress| {
            !matches!(
                progress.phase,
                InstallPhase::Preparing | InstallPhase::Downloading
            )
        }) {
            self.status = "Pause only applies while downloading".into();
            return Action::None;
        }
        self.set_paused(!self.paused, now);
        self.status = if self.paused {
            "Paused — p resumes".into()
        } else {
            "Resumed".into()
        };
        Action::TogglePause
    }

    pub(super) fn set_paused(&mut self, paused: bool, now: Instant) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if paused {
            self.speed.pause(now);
        } else {
            self.speed.resume(now);
        }
    }

    /// Stores a progress sample; terminal phases freeze speed and elapsed time.
    pub fn apply_progress(&mut self, progress: InstallProgress) {
        self.apply_progress_at(progress, Instant::now());
    }

    fn apply_progress_at(&mut self, progress: InstallProgress, now: Instant) {
        match progress.phase {
            InstallPhase::Finished | InstallPhase::Cancelled => {
                self.set_paused(false, now);
                self.speed.freeze(now);
                self.verify_speed.freeze(now);
            }
            // The download loop is over, so a pending pause no longer holds anything;
            // verification gets its own rate from hashed bytes.
            InstallPhase::Verifying => {
                self.set_paused(false, now);
                self.verify_speed.record(now, progress.verify_bytes_done);
            }
            InstallPhase::Committing => self.set_paused(false, now),
            _ => self.speed.record(now, progress.downloaded_bytes),
        }
        self.progress = Some(progress);
    }

    /// The job committed: show the last sample as finished.
    pub fn complete_install(&mut self) {
        self.complete_install_at(Instant::now());
    }

    fn complete_install_at(&mut self, now: Instant) {
        if let Some(progress) = &mut self.progress {
            progress.phase = InstallPhase::Finished;
            progress.completed_bytes = progress.total_bytes;
            progress.verify_bytes_done = progress.verify_bytes_total;
            progress.message = "Verified files committed successfully".into();
        }
        self.set_paused(false, now);
        self.speed.freeze(now);
        self.verify_speed.freeze(now);
    }

    /// The job failed or was cancelled before committing: drop its progress.
    pub fn clear_progress(&mut self) {
        self.progress = None;
        self.paused = false;
        self.speed.reset();
        self.verify_speed.reset();
    }

    pub fn selected_game(&self) -> Option<&CatalogItem> {
        self.order
            .get(self.selected)
            .map(|&index| &self.games[index])
    }

    /// Replaces the rows while keeping selection, search, sort and filter.
    fn replace_games(&mut self, catalog: CatalogSnapshot) {
        self.library_icons.refresh();
        let selected = self.selected_game().map(|game| game.app_id);
        self.games = catalog.items;
        for game in &mut self.games {
            game.name = super::super::terminal_text(&game.name).into_owned();
        }
        self.names = self
            .games
            .iter()
            .map(|game| game.name.to_lowercase())
            .collect();
        self.warnings = catalog.warnings;
        self.inventory_error = None;
        self.inventory_ready = true;
        self.reorder(selected);
    }

    /// Provisional rows (local inventory or last cached catalog); the live
    /// catalog is still loading, so `loading` stays set.
    pub fn show_interim(&mut self, catalog: CatalogSnapshot, source: Interim) {
        self.replace_games(catalog);
        self.loading = true;
        self.status = match source {
            Interim::Installed => format!(
                "{} installed games · refreshing owned library…",
                self.games.len()
            ),
            Interim::Cached => format!(
                "{} games from cache · refreshing owned library…",
                self.games.len()
            ),
        };
    }

    pub fn set_catalog(&mut self, catalog: CatalogSnapshot) {
        self.replace_games(catalog);
        self.loading = false;
        self.status = if self.warnings.is_empty() {
            format!("Library refreshed: {} games", self.games.len())
        } else {
            format!(
                "Library refreshed: {} games; {} scan warnings (? shows them)",
                self.games.len(),
                self.warnings.len()
            )
        };
    }

    /// Persisted view settings as `key=value` lines.
    pub fn settings_text(&self) -> String {
        let sort = match self.sort {
            Sort::Name => "name",
            Sort::Size => "size",
            Sort::LastPlayed => "last",
            Sort::PlayTime => "played",
        };
        let filter = match self.filter {
            LibraryFilter::All => "all",
            LibraryFilter::Installed => "installed",
            LibraryFilter::NotInstalled => "not-installed",
        };
        let pins: Vec<_> = self.pins.iter().map(u32::to_string).collect();
        format!(
            "sort={sort}\nreverse={}\nfilter={filter}\nshow_offline={}\npins={}\nprofile_account={}\nprofile_name={}\n",
            self.reverse,
            self.show_offline,
            pins.join(","),
            self.account.as_deref().unwrap_or_default(),
            self.profile_name.as_deref().unwrap_or_default()
        )
    }

    /// Unknown keys and malformed values are ignored.
    pub fn apply_settings(&mut self, text: &str) {
        // View preferences are global; the cached persona belongs to one account.
        let profile_account = text
            .lines()
            .find_map(|line| line.strip_prefix("profile_account="));
        let restore_profile = self.account.is_some() && profile_account == self.account.as_deref();
        self.profile_name = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match (key.trim(), value.trim()) {
                ("sort", "name") => self.sort = Sort::Name,
                ("sort", "size") => self.sort = Sort::Size,
                ("sort", "last") => self.sort = Sort::LastPlayed,
                ("sort", "played") => self.sort = Sort::PlayTime,
                ("reverse", v) => self.reverse = v == "true",
                ("filter", "all") => self.filter = LibraryFilter::All,
                ("filter", "installed") => self.filter = LibraryFilter::Installed,
                ("filter", "not-installed") => self.filter = LibraryFilter::NotInstalled,
                ("show_offline", v) => self.show_offline = v == "true",
                ("profile_name", v) if restore_profile && !v.is_empty() => {
                    self.profile_name = Some(super::super::terminal_text(v).into_owned())
                }
                ("pins", v) => self.pins = v.split(',').filter_map(|id| id.parse().ok()).collect(),
                _ => {}
            }
        }
        self.filter_friends();
        let selected = self.selected_game().map(|game| game.app_id);
        self.reorder(selected);
    }

    /// Games that can appear at all (blocked, not-installed apps never do).
    pub fn listed_count(&self) -> usize {
        self.games
            .iter()
            .filter(|game| game.installed.is_some() || game.unavailable_reason.is_none())
            .count()
    }

    /// Pinned games are always first in `order`.
    pub fn pinned_count(&self) -> usize {
        self.order
            .iter()
            .take_while(|&&index| self.pins.contains(&self.games[index].app_id))
            .count()
    }

    /// A divider row separates pinned games from the rest.
    pub fn has_divider(&self) -> bool {
        let pinned = self.pinned_count();
        pinned > 0 && pinned < self.order.len()
    }

    pub fn installed(&mut self, game: InstalledGame) {
        let selected = self.selected_game().map(|item| item.app_id);
        if let Some(item) = self
            .games
            .iter_mut()
            .find(|item| item.app_id == game.app_id)
        {
            item.size_bytes = game.size_bytes;
            item.installed = Some(game);
            item.update = UpdateStatus::UpToDate;
            item.unavailable_reason = None;
        }
        self.reorder(selected);
    }

    fn reorder(&mut self, preserve: Option<u32>) {
        let query = self.search.to_lowercase();
        self.order = self
            .names
            .iter()
            .enumerate()
            .filter_map(|(index, name)| {
                let installed = self.games[index].installed.is_some();
                // Blocked apps (no installable depots: tools, soundtracks, ...) are hidden.
                let blocked = !installed && self.games[index].unavailable_reason.is_some();
                (!blocked
                    && name.contains(&query)
                    && match self.filter {
                        LibraryFilter::All => true,
                        LibraryFilter::Installed => installed,
                        LibraryFilter::NotInstalled => !installed,
                    })
                .then_some(index)
            })
            .collect();
        let sort = self.sort;
        let reverse = self.reverse;
        self.order.sort_by(|&a, &b| {
            let left = &self.games[a];
            let right = &self.games[b];
            let ordering = match sort {
                Sort::Name => directed(self.names[a].cmp(&self.names[b]), reverse),
                Sort::Size => optional_cmp(left.size_bytes, right.size_bytes, reverse),
                Sort::LastPlayed => optional_cmp(left.last_played, right.last_played, reverse),
                Sort::PlayTime => {
                    optional_cmp(left.playtime_minutes, right.playtime_minutes, reverse)
                }
            };
            let pinned = |index: usize| self.pins.contains(&self.games[index].app_id);
            pinned(b)
                .cmp(&pinned(a))
                .then(ordering)
                .then_with(|| self.names[a].cmp(&self.names[b]))
                .then_with(|| left.app_id.cmp(&right.app_id))
        });
        self.selected = preserve
            .and_then(|id| {
                self.order
                    .iter()
                    .position(|&index| self.games[index].app_id == id)
            })
            .unwrap_or_else(|| self.selected.min(self.order.len().saturating_sub(1)));
        self.keep_visible();
    }

    pub fn set_profile(&mut self, mut profile: Friend) {
        profile.name = super::super::terminal_text(&profile.name).into_owned();
        profile.game = profile
            .game
            .as_deref()
            .map(|value| super::super::terminal_text(value).into_owned());
        if !profile.name.trim().is_empty() {
            self.profile_name = Some(profile.name.clone());
        }
        self.profile = Some(profile);
        self.profile_live = true;
    }

    pub fn set_friends(&mut self, mut friends: Vec<Friend>) {
        for friend in &mut friends {
            friend.name = super::super::terminal_text(&friend.name).into_owned();
            friend.game = friend
                .game
                .as_ref()
                .map(|game| super::super::terminal_text(game).into_owned());
        }
        friends.sort_by_cached_key(|friend| {
            (
                friend.state.is_offline(),
                friend.name.to_lowercase(),
                friend.steam_id,
            )
        });
        let selected_id = self.selected_friend().map(|f| f.steam_id);
        self.friends = friends;
        self.friend_order.clear();
        self.filter_friends();
        if let Some(pos) = selected_id.and_then(|id| {
            self.friend_order
                .iter()
                .position(|&i| self.friends[i].steam_id == id)
        }) {
            self.friend_selected = pos;
        }
        self.keep_visible();
    }

    pub(super) fn filter_chat_friends(&mut self) {
        self.filter_friends();
    }

    fn filter_friends(&mut self) {
        let selected_id = self.selected_friend().map(|f| f.steam_id);
        self.friend_order = self
            .friends
            .iter()
            .enumerate()
            .filter_map(|(index, friend)| {
                (self.show_offline
                    || !friend.state.is_offline()
                    || self
                        .chat
                        .conversations
                        .get(&friend.steam_id)
                        .is_some_and(|c| c.unread > 0))
                .then_some(index)
            })
            .collect();
        self.friend_selected = selected_id
            .and_then(|id| {
                self.friend_order
                    .iter()
                    .position(|&i| self.friends[i].steam_id == id)
            })
            .unwrap_or(
                self.friend_selected
                    .min(self.friend_order.len().saturating_sub(1)),
            );
        self.friend_offset = bounded_offset(
            self.friend_offset,
            self.friend_order.len(),
            self.friend_rows,
        );
    }

    pub fn keep_visible(&mut self) {
        let rows = self.library_rows.max(1);
        if self.selected < self.library_offset {
            self.library_offset = self.selected;
        } else if self.selected >= self.library_offset.saturating_add(rows) {
            self.library_offset = self.selected + 1 - rows;
        }
        self.library_offset = bounded_offset(self.library_offset, self.order.len(), rows);
        self.friend_selected = self
            .friend_selected
            .min(self.friend_order.len().saturating_sub(1));
        if self.friend_selected < self.friend_offset {
            self.friend_offset = self.friend_selected;
        } else if self.friend_selected >= self.friend_offset.saturating_add(self.friend_rows.max(1))
        {
            self.friend_offset = self.friend_selected + 1 - self.friend_rows.max(1);
        }
        self.friend_offset = bounded_offset(
            self.friend_offset,
            self.friend_order.len(),
            self.friend_rows,
        );
        self.notif_offset =
            bounded_offset(self.notif_offset, self.notifications.len(), self.notif_rows);
    }

    fn move_library(&mut self, down: bool) {
        self.selected = if down {
            self.selected
                .saturating_add(1)
                .min(self.order.len().saturating_sub(1))
        } else {
            self.selected.saturating_sub(1)
        };
        self.keep_visible();
    }

    fn move_friend(&mut self, down: bool) {
        self.friend_selected = if down {
            self.friend_selected
                .saturating_add(1)
                .min(self.friend_order.len().saturating_sub(1))
        } else {
            self.friend_selected.saturating_sub(1)
        };
        self.keep_visible();
    }

    fn page_notifications(&mut self, down: bool) {
        let amount = self.notif_rows.max(1);
        self.notif_offset = if down {
            self.notif_offset.saturating_add(amount)
        } else {
            self.notif_offset.saturating_sub(amount)
        };
        self.notif_offset =
            bounded_offset(self.notif_offset, self.notifications.len(), self.notif_rows);
    }

    fn clear_notifications(&mut self) {
        self.notifications.clear();
        self.notif_offset = 0;
        self.notif_unseen = 0;
        self.notif_fresh.clear();
        self.status = "Notification feed cleared".into();
    }

    /// The Steam session is gone: say so in the panels that depend on it.
    pub fn connection_lost(&mut self, message: &str) {
        self.profile_live = false;
        self.chat_disconnected();
        self.friends_status = message.to_owned();
        self.notif_status = message.to_owned();
    }

    /// A fresh session replaced the old one; its workers are about to report.
    pub fn connection_restored(&mut self) {
        self.profile_live = false;
        self.chat_disconnected();
        self.chat.status.clear();
        self.chat.available = true;
        for c in self.chat.conversations.values_mut() {
            c.unread = 0;
        }
        if let Some(friend) = self.chat.active {
            let c = self.chat.conversations.entry(friend).or_default();
            c.loading = true;
            c.loaded = false;
            self.chat
                .pending
                .push_back(steam_core::chat::Command::History {
                    friend,
                    before: None,
                });
        }
        self.friends_status = "Loading friends…".into();
        self.notif_status.clear();
        self.notifications_live = true;
        self.inventory_error = None;
    }

    /// Applies a live update from the notifications worker. The text is
    /// sanitized by the worker; it is filtered again here as it is server data.
    pub fn notification_event(&mut self, event: NotificationEvent) {
        match event {
            NotificationEvent::Counts(counts) => self.notif_counts = counts,
            NotificationEvent::History(history) => {
                let mut ids: std::collections::HashSet<u64> = history
                    .iter()
                    .filter_map(|note| (note.id != 0).then_some(note.id))
                    .collect();
                self.notifications
                    .retain(|note| note.id == 0 || !ids.contains(&note.id));
                ids.clear();
                for mut note in history {
                    if note.id == 0 || ids.insert(note.id) {
                        for text in [&mut note.label, &mut note.title, &mut note.body] {
                            *text = super::super::terminal_text(text).into_owned();
                        }
                        self.notifications.push(note);
                    }
                }
                self.notifications
                    .sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.id.cmp(&a.id)));
            }
            NotificationEvent::Removed(id) => {
                if id != 0 {
                    self.notifications.retain(|note| note.id != id);
                    self.notif_fresh.remove(&id);
                }
            }
            NotificationEvent::Status(message) => {
                self.notif_status = super::super::terminal_text(&message).into_owned();
            }
            NotificationEvent::Received(mut notification) => {
                for text in [
                    &mut notification.label,
                    &mut notification.title,
                    &mut notification.body,
                ] {
                    *text = super::super::terminal_text(text).into_owned();
                }
                let existing = notification.id != 0
                    && self
                        .notifications
                        .iter()
                        .any(|note| note.id == notification.id);
                if !existing {
                    self.notif_fresh.insert(notification.id);
                }
                if notification.id != 0 {
                    self.notifications
                        .retain(|existing| existing.id != notification.id);
                }
                self.notifications.insert(0, notification);
                self.notifications
                    .sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.id.cmp(&a.id)));
            }
        }
        self.notif_unseen = self
            .notifications
            .iter()
            .filter(|note| self.notif_fresh.contains(&note.id))
            .count();
        self.notif_offset =
            bounded_offset(self.notif_offset, self.notifications.len(), self.notif_rows);
    }

    /// Identical consecutive messages (the backend's repeated ClientHello nudges, say)
    /// are stacked into one entry with a repeat count instead of filling the log.
    pub fn push_backend(&mut self, event: steam_core::backend::Event) {
        const MAX: usize = 1000;
        if let Some(last) = self.backend_log.back_mut()
            && last.event.direction == event.direction
            && last.event.channel == event.channel
            && last.event.title == event.title
            && last.event.detail == event.detail
        {
            last.count = last.count.saturating_add(1);
            last.last_at = event.at;
            return;
        }
        if self.backend_log.len() >= MAX {
            self.backend_log.pop_front();
        }
        self.backend_log.push_back(BackendEntry {
            last_at: event.at,
            event,
            count: 1,
        });
        if self.backend_offset > 0 {
            // Keep the reader's place while new traffic arrives.
            self.backend_offset = self.backend_offset.saturating_add(1);
        }
    }

    fn open_news(&mut self) -> Action {
        let Some(game) = self.selected_game() else {
            return Action::None;
        };
        let (app_id, name) = (game.app_id, game.name.clone());
        self.news_request = self.news_request.wrapping_add(1);
        self.news = Some(NewsView {
            request: self.news_request,
            document: None,
            links: Vec::new(),
            link_error: None,
            app_id,
            name,
            result: None,
            offset: 0,
        });
        Action::News
    }

    fn open_achievements(&mut self) -> Action {
        let Some(game) = self.selected_game() else {
            return Action::None;
        };
        let (app_id, name) = (game.app_id, game.name.clone());
        let language = game
            .installed
            .as_ref()
            .map(|g| g.language.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "english".into());
        self.achievements_request = self.achievements_request.wrapping_add(1);
        self.achievements = Some(super::achievements::AchievementsView::new(
            self.achievements_request,
            app_id,
            name,
            language,
        ));
        Action::Achievements
    }

    pub fn mouse(&mut self, event: MouseEvent) -> Action {
        self.mouse_at(event, Instant::now())
    }

    fn mouse_at(&mut self, event: MouseEvent, now: Instant) -> Action {
        let previous = if matches!(
            event.kind,
            MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            self.last_library_click.take()
        } else {
            None
        };
        if self.chat.active.is_some()
            || self.modal.is_some()
            || self.help
            || self.backend_view
            || self.queue_view
            || self.properties.is_some()
            || self.cloud_conflicts.is_some()
        {
            return Action::None;
        }
        if let Some(view) = &mut self.achievements {
            match event.kind {
                MouseEventKind::ScrollDown => view.step(3),
                MouseEventKind::ScrollUp => view.step(-3),
                MouseEventKind::Down(MouseButton::Right) => self.achievements = None,
                _ => {}
            }
            return Action::None;
        }
        if let Some(news) = &mut self.news {
            match event.kind {
                MouseEventKind::ScrollDown => news.offset = news.offset.saturating_add(3),
                MouseEventKind::ScrollUp => news.offset = news.offset.saturating_sub(3),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(index) = news
                        .links
                        .iter()
                        .position(|(rect, _)| rect.contains((event.column, event.row).into()))
                    {
                        news.link_error = None;
                        return Action::OpenNewsLink(index);
                    }
                }
                MouseEventKind::Down(MouseButton::Right) => self.news = None,
                _ => {}
            }
            return Action::None;
        }
        let position = (event.column, event.row).into();
        if let Some((_, index)) = self
            .library_hits
            .iter()
            .find(|(rect, _)| rect.contains(position))
        {
            match event.kind {
                MouseEventKind::Down(MouseButton::Left | MouseButton::Right) => {
                    self.selected = *index;
                    self.focus = Focus::Library;
                    self.searching = false;
                    self.keep_visible();
                    if event.kind == MouseEventKind::Down(MouseButton::Right) {
                        return self.open_news();
                    }
                    if let Some(app_id) = self.selected_game().map(|game| game.app_id) {
                        if previous.is_some_and(|(id, at)| {
                            id == app_id
                                && now.saturating_duration_since(at) <= Duration::from_millis(400)
                        }) {
                            return self.key(KeyEvent::new(KeyCode::Enter, event.modifiers));
                        }
                        self.last_library_click = Some((app_id, now));
                    }
                }
                MouseEventKind::ScrollDown => self.move_library(true),
                MouseEventKind::ScrollUp => self.move_library(false),
                _ => {}
            }
        }
        Action::None
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        self.last_library_click = None;
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Action::Quit;
        }
        if let Some(friend) = self.chat.active {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('v' | 'V'))
            {
                let c = self.chat.conversations.entry(friend).or_default();
                if !c.sending && !c.clipboard_pending {
                    c.clipboard_pending = true;
                    c.error.clear();
                    return Action::Clipboard(friend);
                }
                return Action::None;
            }
            self.chat_key(key);
            return Action::None;
        }
        if self.modal.is_some() {
            return self.modal_key(key);
        }
        if self.cloud_conflicts.is_some() {
            return self.conflict_key(key);
        }
        if self.achievements.is_some() {
            return self.achievements_key(key);
        }
        if self.news.is_some() {
            return self.news_key(key);
        }
        if self.queue_view {
            return self.queue_key(key);
        }
        if self.properties.is_some() {
            return self.manage_key(key);
        }
        if self.backend_view {
            return self.backend_key(key);
        }
        if self.help {
            return self.help_key(key);
        }
        self.main_key(key)
    }

    fn conflict_key(&mut self, key: KeyEvent) -> Action {
        let modal = self.cloud_conflicts.as_mut().expect("conflict modal");
        if modal.busy {
            return Action::None;
        }
        if keys::closes(&key, &[]) {
            self.cloud_conflicts = self.cloud_conflict_queue.pop_front();
            return Action::None;
        }
        match keys::nav(&key) {
            Some(Nav::Up) => {
                modal.index = modal.index.saturating_sub(1);
                modal.choice = None;
            }
            Some(Nav::Down) => {
                modal.index = (modal.index + 1).min(modal.items.len().saturating_sub(1));
                modal.choice = None;
            }
            Some(_) => {}
            None => match key.code {
                KeyCode::Char('l') => modal.choice = Some(steam_core::cloud::Choice::Local),
                KeyCode::Char('r') => modal.choice = Some(steam_core::cloud::Choice::Cloud),
                KeyCode::Enter if modal.choice.is_some() && !modal.items.is_empty() => {
                    modal.busy = true;
                    self.cloud_busy.insert(modal.game.app_id);
                    return Action::ResolveCloud;
                }
                _ => {}
            },
        }
        Action::None
    }

    fn achievements_key(&mut self, key: KeyEvent) -> Action {
        if keys::closes(&key, &[Key::Char('a')]) {
            self.achievements = None;
            return Action::None;
        }
        let view = self.achievements.as_mut().expect("achievements view");
        if let Some(nav) = keys::nav(&key) {
            match nav {
                Nav::Up => view.step(-1),
                Nav::Down => view.step(1),
                Nav::PageUp => view.step(-(keys::PAGE as isize)),
                Nav::PageDown => view.step(keys::PAGE as isize),
                Nav::Home => view.selected = 0,
                Nav::End => view.selected = usize::MAX,
            }
        } else {
            match key.code {
                KeyCode::Char('s') => view.cycle_sort(),
                KeyCode::Char(digit @ '1'..='4') => view.select_sort(digit),
                KeyCode::Char('f') => view.cycle_filter(),
                KeyCode::Char('r') => {
                    view.result = None;
                    return Action::Achievements;
                }
                _ => {}
            }
        }
        let last = view.visible().len().saturating_sub(1);
        view.selected = view.selected.min(last);
        Action::None
    }

    fn news_key(&mut self, key: KeyEvent) -> Action {
        if keys::closes(&key, &[Key::Char('n')]) {
            self.news = None;
            return Action::None;
        }
        let news = self.news.as_mut().expect("news view");
        let page = keys::PAGE as u16;
        match keys::nav(&key) {
            Some(Nav::Up) => news.offset = news.offset.saturating_sub(1),
            Some(Nav::Down) => news.offset = news.offset.saturating_add(1),
            Some(Nav::PageUp) => news.offset = news.offset.saturating_sub(page),
            Some(Nav::PageDown) => news.offset = news.offset.saturating_add(page),
            Some(Nav::Home) => news.offset = 0,
            Some(Nav::End) => news.offset = u16::MAX,
            None => {}
        }
        Action::None
    }

    fn queue_key(&mut self, key: KeyEvent) -> Action {
        if keys::closes(&key, &[Key::Char('d')]) {
            self.queue_view = false;
            return Action::None;
        }
        if key.code == KeyCode::Char('f') {
            use super::downloads::Kind;
            self.queue_filter = match self.queue_filter {
                None => Some(Kind::Content),
                Some(Kind::Content) => Some(Kind::Shaders),
                Some(Kind::Shaders) => Some(Kind::Dependency),
                Some(Kind::Dependency) => None,
            };
            self.queue_selected = 0;
            self.queue_run = None;
            self.queue_details_offset = 0;
            return Action::None;
        }
        if matches!(key.code, KeyCode::Left | KeyCode::Right) {
            if let Some(entry) = self.download_entries().get(self.queue_selected) {
                let index = self
                    .queue_run
                    .and_then(|id| entry.runs.iter().position(|r| *r == id))
                    .unwrap_or(0);
                let next = if key.code == KeyCode::Left {
                    (index + 1).min(entry.runs.len() - 1)
                } else {
                    index.saturating_sub(1)
                };
                self.queue_run = Some(entry.runs[next]);
                self.queue_details_offset = 0;
            }
            return Action::None;
        }
        if matches!(key.code, KeyCode::Char('{') | KeyCode::Char('}')) {
            self.queue_details_offset = if key.code == KeyCode::Char('}') {
                self.queue_details_offset.saturating_add(1)
            } else {
                self.queue_details_offset.saturating_sub(1)
            };
            return Action::None;
        }
        let last = self.download_entries().len().saturating_sub(1);
        if let Some(nav) = keys::nav(&key) {
            self.queue_run = None;
            self.queue_details_offset = 0;
            self.queue_selected = match nav {
                Nav::Up => self.queue_selected.saturating_sub(1),
                Nav::Down => self.queue_selected.saturating_add(1),
                Nav::PageUp => self.queue_selected.saturating_sub(keys::PAGE),
                Nav::PageDown => self.queue_selected.saturating_add(keys::PAGE),
                Nav::Home => 0,
                Nav::End => last,
            }
            .min(last);
            return Action::None;
        }
        let Some(job) = self.selected_download() else {
            return Action::None;
        };
        use steam_core::updates::JobState;
        match key.code {
            KeyCode::Char('p')
                if matches!(
                    job.state,
                    JobState::Running | JobState::Pending | JobState::Paused
                ) =>
            {
                Action::QueueControl(
                    job.id,
                    if job.state == JobState::Paused {
                        JobState::Pending
                    } else {
                        JobState::Paused
                    },
                )
            }
            KeyCode::Char('r') if matches!(job.state, JobState::Failed | JobState::Cancelled) => {
                Action::QueueControl(job.id, JobState::Pending)
            }
            KeyCode::Char('c')
                if !matches!(job.state, JobState::Complete | JobState::Cancelled) =>
            {
                Action::QueueControl(job.id, JobState::Cancelled)
            }
            KeyCode::Char(']') => Action::QueuePriority(job.intent.app_id, 1),
            KeyCode::Char('[') => Action::QueuePriority(job.intent.app_id, -1),
            KeyCode::Char('a') => Action::QueuePolicy(job.intent.app_id),
            _ => Action::None,
        }
    }

    fn manage_key(&mut self, key: KeyEvent) -> Action {
        let props = self.properties.as_mut().expect("manage view");
        if let Some(password) = &mut props.password {
            match key.code {
                KeyCode::Esc => props.password = None,
                KeyCode::Enter => {
                    props.busy = true;
                    return Action::UnlockBranch;
                }
                KeyCode::Backspace => {
                    password.pop();
                }
                KeyCode::F(2) => props.remember_key = !props.remember_key,
                KeyCode::Char(c) if !c.is_control() && password.len() + c.len_utf8() <= 4096 => {
                    password.push(c)
                }
                _ => {}
            }
            return Action::None;
        }
        if keys::closes(&key, &[Key::Char('m')]) {
            self.properties = None;
            return Action::None;
        }
        if props.busy {
            return Action::None;
        }
        let Some(data) = &props.data else {
            return Action::None;
        };
        let last = data.dlc.len() + 1;
        if let Some(nav) = keys::nav(&key) {
            props.selected = match nav {
                Nav::Up => props.selected.saturating_sub(1),
                Nav::Down => props.selected.saturating_add(1),
                Nav::PageUp => props.selected.saturating_sub(keys::PAGE),
                Nav::PageDown => props.selected.saturating_add(keys::PAGE),
                Nav::Home => 0,
                Nav::End => last,
            }
            .min(last);
            return Action::None;
        }
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l') if props.selected < 2 => {
                let (index, count) = if props.selected == 0 {
                    (&mut props.branch, data.branches.len())
                } else {
                    (&mut props.language, data.languages.len())
                };
                if count > 0 {
                    let forward = matches!(key.code, KeyCode::Right | KeyCode::Char('l'));
                    *index = if forward {
                        (*index + 1) % count
                    } else {
                        (*index + count - 1) % count
                    };
                }
            }
            KeyCode::Enter if props.selected < 2 => {
                if props.selected == 0
                    && data
                        .branches
                        .get(props.branch)
                        .is_some_and(|b| b.password_required)
                {
                    props.password = Some(zeroize::Zeroizing::new(String::with_capacity(4096)));
                } else {
                    props.busy = true;
                    return Action::ApplyContent;
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                if let Some(dlc) = props.selected.checked_sub(2).and_then(|i| data.dlc.get(i)) {
                    props.busy = true;
                    return Action::ToggleDlc(dlc.app_id, !dlc.enabled);
                }
            }
            _ => {}
        }
        Action::None
    }

    fn backend_key(&mut self, key: KeyEvent) -> Action {
        if keys::closes(&key, &[Key::Char('b')]) {
            self.backend_view = false;
            return Action::None;
        }
        if key.code == KeyCode::Char('x') {
            self.backend_log.clear();
            self.backend_offset = 0;
            return Action::None;
        }
        // The offset counts lines back from the newest entry.
        match keys::nav(&key) {
            Some(Nav::Up) => self.backend_offset = self.backend_offset.saturating_add(1),
            Some(Nav::Down) => self.backend_offset = self.backend_offset.saturating_sub(1),
            Some(Nav::PageUp) => {
                self.backend_offset = self.backend_offset.saturating_add(keys::PAGE)
            }
            Some(Nav::PageDown) => {
                self.backend_offset = self.backend_offset.saturating_sub(keys::PAGE)
            }
            Some(Nav::End) => self.backend_offset = 0,
            Some(Nav::Home) => self.backend_offset = usize::MAX / 2,
            None => {}
        }
        Action::None
    }

    fn help_key(&mut self, key: KeyEvent) -> Action {
        if keys::closes(&key, &[Key::Char('?'), Key::Code(KeyCode::F(1))]) {
            self.help = false;
            return Action::None;
        }
        let page = keys::PAGE as u16;
        match keys::nav(&key) {
            Some(Nav::Up) => self.help_offset = self.help_offset.saturating_sub(1),
            Some(Nav::Down) => self.help_offset = self.help_offset.saturating_add(1),
            Some(Nav::PageUp) => self.help_offset = self.help_offset.saturating_sub(page),
            Some(Nav::PageDown) => self.help_offset = self.help_offset.saturating_add(page),
            Some(Nav::Home) => self.help_offset = 0,
            Some(Nav::End) => self.help_offset = u16::MAX,
            None => {}
        }
        Action::None
    }

    /// Page keys always scroll notifications; other movement follows focus.
    fn navigate(&mut self, cmd: Cmd) -> Action {
        if matches!(cmd, Cmd::PageUp | Cmd::PageDown) {
            self.page_notifications(cmd == Cmd::PageDown);
            self.notif_unseen = 0;
            self.notif_fresh.clear();
            return Action::None;
        }
        match self.focus {
            Focus::Library => match cmd {
                Cmd::Up => self.move_library(false),
                Cmd::Down => self.move_library(true),
                Cmd::Home => {
                    self.selected = 0;
                    self.keep_visible();
                }
                _ => {
                    self.selected = self.order.len().saturating_sub(1);
                    self.keep_visible();
                }
            },
            Focus::Friends => match cmd {
                Cmd::Up => self.move_friend(false),
                Cmd::Down => self.move_friend(true),
                Cmd::Home => {
                    self.friend_selected = 0;
                    self.keep_visible();
                }
                _ => {
                    self.friend_selected = usize::MAX / 2;
                    self.move_friend(true);
                }
            },
        }
        Action::None
    }

    fn main_key(&mut self, key: KeyEvent) -> Action {
        let cmd = keys::lookup(self.focus, &key);
        if self.searching {
            return self.filter_key(key, cmd);
        }
        let Some(cmd) = cmd else {
            return Action::None;
        };
        let preserve = self.selected_game().map(|game| game.app_id);
        match cmd {
            Cmd::Quit => return Action::Quit,
            Cmd::Refresh => return Action::Refresh,
            Cmd::News => return self.open_news(),
            Cmd::Achievements => return self.open_achievements(),
            Cmd::Activate if self.focus == Focus::Friends => {
                self.open_chat();
                return Action::None;
            }
            Cmd::Activate | Cmd::ValveHost => {
                let Some(backend) = self.launch_backend(cmd == Cmd::ValveHost) else {
                    return Action::None;
                };
                return if self
                    .selected_game()
                    .is_some_and(|game| game.installed.is_some())
                {
                    Action::Launch(backend)
                } else {
                    self.request_install(false)
                };
            }
            Cmd::LaunchInstalled => {
                let Some(backend) = self.launch_backend(false) else {
                    return Action::None;
                };
                if self
                    .selected_game()
                    .is_some_and(|game| game.installed.is_some())
                {
                    return Action::LaunchInstalled(backend);
                }
                self.status = "Install the game before launching it".into();
            }
            Cmd::Update => return self.request_install(true),
            Cmd::Pause if self.install_active => {
                return self.toggle_pause_at(Instant::now());
            }
            Cmd::Pause => {}
            Cmd::Cancel => {
                if let Some(game) = self.selected_game()
                    && self.launching.contains(&game.app_id)
                {
                    return Action::SkipShaders(game.app_id);
                }
                if self.install_active {
                    self.set_paused(false, Instant::now());
                    self.cancel_requested = true;
                    self.status =
                        "Cancellation requested; waiting for a safe transaction boundary…".into();
                    return Action::CancelInstall;
                }
                self.status = "Nothing to cancel for this game".into();
            }
            Cmd::Downloads => {
                self.queue_view = true;
                return Action::Queue;
            }
            Cmd::Manage => {
                if let Some(game) = self.selected_game().and_then(|g| g.installed.clone()) {
                    self.properties = Some(ContentProperties {
                        game,
                        password: None,
                        remember_key: true,
                        data: None,
                        branch: 0,
                        language: 0,
                        selected: 0,
                        busy: true,
                        error: None,
                    });
                    return Action::ContentProperties;
                }
                self.status = "Install the game before managing its content".into();
            }
            Cmd::LaunchOption => match self.selected_game() {
                Some(game) if game.installed.is_some() => {
                    return Action::CycleLaunchOption(game.app_id);
                }
                _ => self.status = "Install the game before choosing a launch option".into(),
            },
            Cmd::Shaders => match self.selected_game() {
                Some(game) if game.installed.is_some() => {
                    return Action::PrecacheShaders(game.app_id);
                }
                _ => self.status = "Install the game before pre-caching its shaders".into(),
            },
            Cmd::Backend => self.backend_view = true,
            Cmd::SyncSaves => return self.request_cloud_sync(),
            Cmd::View => {
                self.filter = self.filter.next();
                self.reorder(preserve);
            }
            Cmd::Filter => {
                self.focus = Focus::Library;
                self.searching = true;
            }
            Cmd::ClearFilter => {
                self.search.clear();
                self.reorder(preserve);
            }
            Cmd::Pin => {
                if let Some(id) = self.selected_game().map(|game| game.app_id) {
                    if !self.pins.remove(&id) {
                        self.pins.insert(id);
                    }
                    self.status = if self.pins.contains(&id) {
                        "Pinned to the top"
                    } else {
                        "Unpinned"
                    }
                    .into();
                    self.reorder(preserve);
                }
            }
            Cmd::SortName => self.choose_sort(Sort::Name, preserve),
            Cmd::SortSize => self.choose_sort(Sort::Size, preserve),
            Cmd::SortLast => self.choose_sort(Sort::LastPlayed, preserve),
            Cmd::SortPlayed => self.choose_sort(Sort::PlayTime, preserve),
            Cmd::CycleSort => {
                self.sort = self.sort.next();
                self.reorder(preserve);
            }
            Cmd::NextPane | Cmd::PrevPane => {
                self.focus = if cmd == Cmd::NextPane {
                    self.focus.next()
                } else {
                    self.focus.previous()
                };
            }
            Cmd::ToggleOffline => {
                self.show_offline = !self.show_offline;
                self.filter_friends();
            }
            Cmd::ClearFeed => self.clear_notifications(),
            Cmd::Help => self.help = true,
            Cmd::Up | Cmd::Down | Cmd::PageUp | Cmd::PageDown | Cmd::Home | Cmd::End => {
                return self.navigate(cmd);
            }
        }
        Action::None
    }

    /// While a filter is typed every character is text; only the movement keys
    /// and F1 keep their meaning.
    fn filter_key(&mut self, key: KeyEvent, cmd: Option<Cmd>) -> Action {
        let movement = matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End
        );
        if movement && let Some(cmd) = cmd {
            return self.navigate(cmd);
        }
        if key.code == KeyCode::F(1) {
            self.help = true;
            return Action::None;
        }
        let preserve = self.selected_game().map(|game| game.app_id);
        match key.code {
            KeyCode::Esc => {
                self.search.clear();
                self.searching = false;
            }
            KeyCode::Enter => self.searching = false,
            KeyCode::Backspace | KeyCode::Char('h')
                if key.code == KeyCode::Backspace
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.search.pop();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !ch.is_control() =>
            {
                self.search.push(ch)
            }
            _ => return Action::None,
        }
        self.reorder(preserve);
        Action::None
    }

    /// A column key: the active sort reverses, any other becomes the sort.
    fn choose_sort(&mut self, sort: Sort, preserve: Option<u32>) {
        if self.sort == sort {
            self.reverse = !self.reverse;
        } else {
            self.sort = sort;
        }
        self.reorder(preserve);
    }

    /// The checks every launch shares. `None` means the launch was refused and
    /// the status says why; otherwise the backend that would run it.
    fn launch_backend(&mut self, force_host: bool) -> Option<LaunchBackend> {
        if self.pending_launch.is_some() {
            self.status = "A launch is waiting for its dependency installation to finish".into();
            return None;
        }
        let backend = if force_host
            || self
                .selected_game()
                .is_some_and(|game| steam_core::launch::requires_valve_host(game.app_id))
        {
            LaunchBackend::ValveHost
        } else {
            LaunchBackend::BuiltIn
        };
        if let Some(game) = self.selected_game().filter(|game| game.installed.is_some())
            && let Err(error) = steam_core::launch::validate_backend(game.app_id, backend)
        {
            self.status = error.to_string();
            return None;
        }
        if let Some(game) = self
            .selected_game()
            .filter(|game| game.installed.is_some() && self.cloud_busy.contains(&game.app_id))
        {
            self.status = format!(
                "Steam Cloud is syncing {}; wait for it to finish",
                game.name
            );
            return None;
        }
        if self.selected_game().is_some_and(|game| {
            game.installed.is_some()
                && !self.active_shader
                && self
                    .active_install
                    .as_ref()
                    .is_some_and(|(app_id, _)| *app_id == game.app_id)
        }) {
            self.status =
                "Cannot launch a game while its install/update transaction is active".into();
            return None;
        }
        Some(backend)
    }

    /// `y`: sync Steam Cloud saves of the selected installed game (download,
    /// then upload). The running-game check needs the OS and happens in the caller.
    fn request_cloud_sync(&mut self) -> Action {
        let Some(game) = self.selected_game() else {
            self.status = "No game is selected".into();
            return Action::None;
        };
        let (id, name) = (game.app_id, game.name.clone());
        let refusal = if game.installed.is_none() {
            Some("Steam Cloud sync needs an installed game".to_owned())
        } else if self.launching.contains(&id) {
            Some("Wait for the launch request to finish before syncing Steam Cloud".to_owned())
        } else if self.cloud_busy.contains(&id) {
            Some(format!("Steam Cloud is already syncing {name}"))
        } else if self
            .active_install
            .as_ref()
            .is_some_and(|(app_id, _)| *app_id == id)
        {
            Some("Wait for the install/update to finish before syncing Steam Cloud".to_owned())
        } else {
            None
        };
        if let Some(message) = refusal {
            self.status = message;
            return Action::None;
        }
        self.cloud_busy.insert(id);
        self.status = format!("Steam Cloud: syncing {name}…");
        Action::CloudSync
    }

    fn request_install(&mut self, update: bool) -> Action {
        if self.install_active || self.preparing_install {
            self.status = "An installation job is already in progress".into();
            return Action::None;
        }
        let Some(game) = self.selected_game() else {
            self.status = "No game is selected".into();
            return Action::None;
        };
        if self.launching.contains(&game.app_id) {
            self.status =
                "Wait for the direct-launch request to finish before changing this game".into();
            return Action::None;
        }
        if game.installed.is_some() != update {
            self.status = if update {
                "Select an installed game to update"
            } else {
                "Already installed; u updates it, Enter launches it"
            }
            .into();
            return Action::None;
        }
        if let Some(reason) = &game.unavailable_reason {
            self.status = format!("Installation unavailable: {reason}");
            return Action::None;
        }
        if update && matches!(game.update, UpdateStatus::UpToDate) {
            self.status = "This game is up to date".into();
            return Action::None;
        }
        self.preparing_install = true;
        self.status = "Resolving depot plan and destination space…".into();
        Action::Install
    }

    fn modal_key(&mut self, key: KeyEvent) -> Action {
        let modal = self.modal.as_mut().expect("modal key requires modal");
        if let Some(path) = modal.custom.as_mut() {
            match key.code {
                KeyCode::Esc if !modal.checking => modal.custom = None,
                KeyCode::Enter if !modal.checking => {
                    if path.trim().is_empty() {
                        modal.error = Some("Enter a library path".into());
                    } else {
                        modal.checking = true;
                        modal.error = None;
                        return Action::CheckDestination;
                    }
                }
                KeyCode::Backspace if !modal.checking => {
                    path.pop();
                }
                KeyCode::Char(ch)
                    if !modal.checking
                        && !ch.is_control()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    path.push(ch)
                }
                _ => {}
            }
            return Action::None;
        }
        match key.code {
            KeyCode::Esc => self.modal = None,
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Up | KeyCode::Char('k') => {
                modal.selected = modal.selected.saturating_sub(1);
                modal.low_space_confirmed = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                modal.selected = modal
                    .selected
                    .saturating_add(1)
                    .min(modal.destinations.len().saturating_sub(1));
                modal.low_space_confirmed = false;
            }
            KeyCode::Char('n') if modal.existing.is_none() => {
                modal.custom = Some(String::new());
                modal.error = None;
            }
            KeyCode::Enter => {
                if modal.selected_destination().is_none() {
                    modal.error = Some("No destination available; n adds a custom library".into());
                } else if let Some(reason) = modal.blocked_reason() {
                    if modal
                        .selected_destination()
                        .is_some_and(|destination| destination.available)
                    {
                        modal.low_space_confirmed = true;
                    }
                    modal.error = Some(reason);
                } else {
                    return Action::StartInstall;
                }
            }
            _ => {}
        }
        Action::None
    }
}

/// Outcome of the last Steam Cloud sync of a game this session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CloudResult {
    Ok,
    Conflicts,
    Failed,
}

/// What the library's cloud column shows for a game.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CloudIcon {
    Never,
    Synced,
    Pending,
    /// A sync is running; the number picks the spinner frame.
    Syncing(usize),
    /// The last sync failed or left conflicts.
    Problem,
}

impl Model {
    /// Cached, O(1): `None` (blank) for uninstalled games and games without
    /// Steam Cloud configuration. `spin` is the animation frame counter.
    pub fn cloud_icon(&self, game: &CatalogItem, spin: usize) -> Option<CloudIcon> {
        game.installed.as_ref()?;
        let badge = self.cloud_badges.get(&game.app_id).copied();
        let syncing = self.cloud_busy.contains(&game.app_id)
            || (self.launching.contains(&game.app_id) && badge.is_some());
        if syncing {
            return Some(CloudIcon::Syncing(spin));
        }
        let badge = badge?;
        if matches!(
            self.cloud_last.get(&game.app_id),
            Some(CloudResult::Failed | CloudResult::Conflicts)
        ) {
            return Some(CloudIcon::Problem);
        }
        Some(match badge {
            CloudBadge::Never => CloudIcon::Never,
            CloudBadge::Synced => CloudIcon::Synced,
            CloudBadge::Pending => CloudIcon::Pending,
        })
    }
}

fn directed(order: Ordering, reverse: bool) -> Ordering {
    if reverse { order.reverse() } else { order }
}

fn optional_cmp<T: Ord>(left: Option<T>, right: Option<T>, reverse: bool) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => directed(left.cmp(&right), reverse),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn bounded_offset(offset: usize, len: usize, rows: usize) -> usize {
    offset.min(len.saturating_sub(rows.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use steam_core::friends::Presence;

    fn game(app_id: u32, name: &str, size: Option<u64>) -> CatalogItem {
        CatalogItem {
            app_id,
            name: name.to_owned(),
            installed: Some(InstalledGame {
                app_id,
                name: name.to_owned(),
                size_bytes: size,
                last_played: None,
                playtime_minutes: None,
                install_dir: PathBuf::new(),
                library_root: PathBuf::new(),
                steam_root: PathBuf::new(),
                launch_options: None,
                build_id: None,
                branch: "public".into(),
                language: "english".into(),
                platform: None,
                installed_depots: Default::default(),
            }),
            plan: None,
            update: UpdateStatus::Unknown,
            last_played: None,
            playtime_minutes: None,
            size_bytes: size,
            unavailable_reason: None,
        }
    }

    fn loaded() -> Model {
        let mut model = Model::new(true, None);
        model.set_catalog(CatalogSnapshot {
            items: vec![
                game(10, "Alpha", Some(10)),
                game(20, "Beta", None),
                game(30, "Quest", Some(30)),
            ],
            warnings: Vec::new(),
        });
        model
    }

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn enter_installs_what_is_missing_and_launches_what_is_installed() {
        let mut model = loaded();
        assert_eq!(
            model.key(press(KeyCode::Enter)),
            Action::Launch(LaunchBackend::BuiltIn)
        );
        model.games[model.order[0]].installed = None;
        assert_eq!(model.key(press(KeyCode::Enter)), Action::Install);
        // `i` no longer installs; `l` refuses to launch what is not installed.
        model.preparing_install = false;
        assert_eq!(model.key(key('i')), Action::None);
        assert_eq!(model.key(key('l')), Action::None);
        assert_eq!(model.status, "Install the game before launching it");
    }

    #[test]
    fn c_cancels_the_install_or_skips_shaders_of_a_launching_game() {
        let mut model = loaded();
        assert_eq!(model.key(key('c')), Action::None);
        assert_eq!(model.status, "Nothing to cancel for this game");
        model.install_active = true;
        assert_eq!(model.key(key('c')), Action::CancelInstall);
        model.install_active = false;
        model.cancel_requested = false;
        model.launching.insert(10);
        assert_eq!(model.key(key('c')), Action::SkipShaders(10));
        // Upper-case S is not a second skip key any more: it is nothing.
        assert_eq!(model.key(key('S')), Action::None);
    }

    #[test]
    fn game_actions_use_their_new_letters() {
        let mut model = loaded();
        assert_eq!(model.key(key('d')), Action::Queue);
        assert!(model.queue_view);
        model.queue_view = false;
        assert_eq!(model.key(key('m')), Action::ContentProperties);
        model.properties = None;
        assert_eq!(model.key(key('o')), Action::CycleLaunchOption(10));
        assert_eq!(model.key(key('z')), Action::PrecacheShaders(10));
        assert_eq!(model.key(key('y')), Action::CloudSync);
        for removed in ['J', 'P', 'O', 'Z', 'C', 'L', 'B'] {
            assert_eq!(model.key(key(removed)), Action::None, "{removed}");
        }
    }

    #[test]
    fn number_keys_pick_a_sort_and_the_active_one_reverses_it() {
        let mut model = loaded();
        model.key(key('2'));
        assert_eq!((model.sort, model.reverse), (Sort::Size, false));
        model.key(key('2'));
        assert_eq!((model.sort, model.reverse), (Sort::Size, true));
        model.key(key('3'));
        assert_eq!(model.sort, Sort::LastPlayed);
        model.key(key('s'));
        assert_eq!(model.sort, Sort::PlayTime);
    }

    #[test]
    fn j_and_k_move_the_focused_pane_only() {
        let mut model = loaded();
        model.friend_rows = 1;
        model.set_friends(
            (0..4)
                .map(|id| Friend {
                    steam_id: id,
                    name: id.to_string(),
                    state: Presence::Online,
                    game: None,
                })
                .collect(),
        );
        model.key(key('j'));
        assert_eq!((model.selected, model.friend_offset), (1, 0));
        model.focus = Focus::Friends;
        model.key(key('j'));
        assert_eq!((model.selected, model.friend_offset), (1, 1));
        model.key(key('k'));
        assert_eq!(model.friend_offset, 0);
    }

    #[test]
    fn overlays_close_with_esc_q_or_their_own_key_and_q_no_longer_quits_them() {
        let mut model = loaded();
        for (open, close) in [
            ('n', 'n'),
            ('n', 'q'),
            ('n', 'x'),
            ('a', 'a'),
            ('d', 'd'),
            ('b', 'b'),
            ('?', '?'),
        ] {
            model.key(key(open));
            let overlay_open = |m: &Model| {
                m.news.is_some()
                    || m.achievements.is_some()
                    || m.queue_view
                    || m.backend_view
                    || m.help
            };
            assert!(overlay_open(&model), "{open}");
            let action = model.key(key(close));
            if close == 'x' {
                // Not a closing key: the overlay stays; Esc closes it.
                assert!(overlay_open(&model));
                model.key(press(KeyCode::Esc));
            }
            assert_eq!(action, Action::None, "{open}{close}");
            assert!(!overlay_open(&model), "{open}{close}");
        }
        assert_eq!(model.key(key('q')), Action::Quit);
    }

    #[test]
    fn help_and_queue_overlays_page_by_ten() {
        let mut model = loaded();
        model.key(key('?'));
        model.key(press(KeyCode::PageDown));
        assert_eq!(model.help_offset, 10);
        model.key(key('j'));
        assert_eq!(model.help_offset, 11);
        model.key(press(KeyCode::Home));
        assert_eq!(model.help_offset, 0);
        model.key(key('?'));
        assert!(!model.help);
    }

    #[test]
    fn manage_overlay_changes_branch_with_h_and_l_as_well_as_arrows() {
        let mut model = loaded();
        assert_eq!(model.key(key('m')), Action::ContentProperties);
        let props = model.properties.as_mut().unwrap();
        props.busy = false;
        props.data = Some(steam_core::content_options::Properties {
            app_id: 10,
            branches: ["public", "beta", "old"]
                .map(|name| steam_core::content_options::Branch {
                    name: name.into(),
                    password_required: false,
                })
                .to_vec(),
            languages: vec!["english".into()],
            dlc: vec![],
        });
        model.key(key('l'));
        assert_eq!(model.properties.as_ref().unwrap().branch, 1);
        model.key(press(KeyCode::Right));
        model.key(key('h'));
        assert_eq!(model.properties.as_ref().unwrap().branch, 1);
        model.key(key('m'));
        assert!(model.properties.is_none());
    }

    #[test]
    fn unknown_values_remain_last_in_both_directions() {
        for reverse in [false, true] {
            assert_eq!(optional_cmp(Some(2), None, reverse), Ordering::Less);
            assert_eq!(optional_cmp(None, Some(2), reverse), Ordering::Greater);
            assert_eq!(optional_cmp::<u64>(None, None, reverse), Ordering::Equal);
        }
        let mut model = loaded();
        model.key(key('2'));
        assert_eq!(
            model
                .order
                .iter()
                .map(|&i| model.games[i].app_id)
                .collect::<Vec<_>>(),
            [10, 30, 20]
        );
        model.key(key('2'));
        assert_eq!(
            model
                .order
                .iter()
                .map(|&i| model.games[i].app_id)
                .collect::<Vec<_>>(),
            [30, 10, 20]
        );
    }

    #[test]
    fn selection_survives_sort_and_refresh() {
        let mut model = loaded();
        model.selected = 1;
        model.key(key('2'));
        model.key(key('2'));
        assert_eq!(model.selected_game().unwrap().app_id, 20);
        model.set_catalog(CatalogSnapshot {
            items: vec![game(20, "Beta", Some(999)), game(10, "Alpha", None)],
            warnings: Vec::new(),
        });
        assert_eq!(model.selected_game().unwrap().app_id, 20);
    }

    #[test]
    fn obsolete_host_preference_is_ignored() {
        let mut model = loaded();
        model.apply_settings("steam_host=true\n");
        assert!(!model.settings_text().contains("steam_host"));
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Launch(LaunchBackend::BuiltIn)
        );
    }

    #[test]
    fn profile_name_is_cached_safely_without_reusing_live_presence() {
        use steam_core::friends::Presence;
        let mut model = Model::new(false, None);
        model.account = Some("account-one".into());
        model.set_profile(Friend {
            steam_id: 76561198000000099,
            name: "vanyapoker\nshow_offline=true\u{1b}".into(),
            state: Presence::Online,
            game: None,
        });
        let text = model.settings_text();
        assert!(!text.contains("\nshow_offline=true"));
        let mut restored = Model::new(true, None);
        restored.account = Some("account-one".into());
        restored.apply_settings(&text);
        assert_eq!(restored.profile_name, model.profile_name);
        assert!(restored.profile.is_none());
        assert!(!restored.profile_live);
        assert!(!restored.show_offline);
        assert!(restored.friends.is_empty());
        restored.account = Some("account-two".into());
        restored.apply_settings(&text);
        assert!(restored.profile_name.is_none());
    }

    #[test]
    fn pinned_games_sort_first_and_settings_round_trip() {
        let mut model = loaded();
        model.key(key('j'));
        model.key(key('j'));
        model.key(key('f'));
        assert_eq!(model.selected_game().unwrap().app_id, 30);
        assert_eq!(model.games[model.order[0]].app_id, 30);
        model.sort = Sort::Name;
        model.key(key('1'));
        assert!(model.reverse);
        assert_eq!(model.games[model.order[0]].app_id, 30);
        let text = model.settings_text();
        let mut fresh = loaded();
        fresh.apply_settings(&text);
        assert_eq!(fresh.settings_text(), text);
        assert_eq!(fresh.games[fresh.order[0]].app_id, 30);
        assert!(fresh.reverse);
    }

    #[test]
    fn search_takes_precedence_over_printable_hotkeys() {
        let mut model = loaded();
        model.key(key('/'));
        assert_eq!(model.key(key('q')), Action::None);
        assert_eq!(model.search, "q");
        assert_eq!(model.selected_game().unwrap().app_id, 30);
        model.key(key('o'));
        assert!(!model.show_offline);
        model.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(model.search.is_empty());
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn filter_field_takes_f_digits_and_q_as_text_and_f_pins_outside_it() {
        let mut model = loaded();
        model.key(key('f'));
        assert!(!model.searching);
        assert_eq!(model.pins.len(), 1);
        model.key(key('f'));
        assert!(model.pins.is_empty());
        model.key(key('/'));
        assert!(model.searching);
        for ch in ['f', '1', 'q', 's', 'v'] {
            assert_eq!(model.key(key(ch)), Action::None);
        }
        assert_eq!(model.search, "f1qsv");
        assert_eq!(model.sort, Sort::Name);
        assert_eq!(model.filter, LibraryFilter::All);
        assert!(model.order.is_empty());
        model.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(model.search, "f1qs");
        model.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!model.searching);
        assert_eq!(model.order.len(), 3);
    }

    #[test]
    fn movement_keys_act_on_the_focused_pane_and_clamp() {
        let mut model = loaded();
        model.library_rows = 2;
        model.friend_rows = 3;
        model.set_friends(
            (0..8)
                .map(|id| Friend {
                    steam_id: id,
                    name: id.to_string(),
                    state: Presence::Online,
                    game: None,
                })
                .collect(),
        );
        model.focus = Focus::Friends;
        model.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            (model.friend_selected, model.friend_offset, model.selected),
            (1, 0, 0)
        );
        for _ in 0..8 {
            model.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!((model.friend_offset, model.selected), (5, 0));
        for _ in 0..8 {
            model.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        assert_eq!(model.friend_offset, 0);
        model.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(model.friend_offset, 5);
        model.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(model.friend_offset, 0);
        model.focus = Focus::Library;
        model.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!((model.selected, model.friend_offset), (1, 0));
        model.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(model.selected, 2);
        model.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(model.selected, model.order.len() - 1);
        model.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(model.selected, 0);
        model.friend_rows = 20;
        model.keep_visible();
        assert_eq!(model.friend_offset, 0);
    }

    #[test]
    fn offline_toggle_clamps_friend_scroll_without_moving_library() {
        let mut model = loaded();
        model.friend_rows = 1;
        model.set_friends(vec![
            Friend {
                steam_id: 1,
                name: "Online".into(),
                state: Presence::Online,
                game: Some("Game".into()),
            },
            Friend {
                steam_id: 2,
                name: "Offline".into(),
                state: Presence::Offline,
                game: None,
            },
        ]);
        model.focus = Focus::Friends;
        model.key(key('o'));
        model.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(model.friend_offset, 1);
        model.key(key('o'));
        assert_eq!(model.friend_order.len(), 1);
        assert_eq!(model.friend_offset, 0);
        assert_eq!(model.selected_game().unwrap().app_id, 10);
    }

    #[test]
    fn vac_games_automatically_use_host_and_ctrl_enter_remains_explicit() {
        for id in [440, 570, 730, 1422450, 3124540] {
            let mut model = loaded();
            model.games[model.order[model.selected]].app_id = id;
            let expected = if steam_core::launch::requires_valve_host(id) {
                Action::Launch(LaunchBackend::ValveHost)
            } else {
                Action::Launch(LaunchBackend::BuiltIn)
            };
            assert_eq!(
                model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                expected
            );
            assert_eq!(
                model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
                Action::Launch(LaunchBackend::ValveHost)
            );
            assert_eq!(
                model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                expected
            );
        }
    }

    #[test]
    fn double_click_uses_enter_policy_and_keeps_cloud_guard() {
        let mut model = loaded();
        model.games[model.order[0]].app_id = 440;
        model.library_hits = vec![(ratatui::layout::Rect::new(0, 0, 20, 1), 0)];
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let now = Instant::now();
        assert_eq!(model.mouse_at(click, now), Action::None);
        assert_eq!(
            model.mouse_at(click, now + Duration::from_millis(200)),
            Action::Launch(LaunchBackend::ValveHost)
        );
        // A third click starts a new pair, and a slow second click only selects.
        assert_eq!(
            model.mouse_at(click, now + Duration::from_millis(300)),
            Action::None
        );
        assert_eq!(
            model.mouse_at(click, now + Duration::from_millis(800)),
            Action::None
        );
        model.cloud_busy.insert(440);
        assert_eq!(
            model.mouse_at(click, now + Duration::from_millis(900)),
            Action::None
        );
        assert!(model.status.contains("Steam Cloud"));
    }

    #[test]
    fn enter_finishes_search_before_launch_and_escape_clears_committed_filter() {
        let mut model = loaded();
        model.key(key('/'));
        model.key(key('q'));
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert!(!model.searching);
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Launch(LaunchBackend::BuiltIn)
        );
        model.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(model.order.len(), 3);
        assert_eq!(model.selected_game().unwrap().app_id, 30);
    }

    fn modal(free_bytes: Option<u64>) -> InstallModal {
        InstallModal {
            plan: InstallPlan {
                app_id: 40,
                name: "New game".into(),
                install_dir_name: "NewGame".into(),
                build_id: 1,
                platform: steam_core::catalog::InstallPlatform::Linux,
                branch: "public".into(),
                language: "english".into(),
                shared_depots: Default::default(),
                dlc_depots: Default::default(),
                depots: vec![],
                download_bytes: 50,
                installed_bytes: 100,
            },
            existing: None,
            steam_root: "/steam".into(),
            destinations: vec![Destination {
                path: "/library".into(),
                free_bytes,
                available: true,
                message: None,
            }],
            selected: 0,
            custom: None,
            checking: false,
            low_space_confirmed: false,
            error: None,
        }
    }

    #[test]
    fn installed_filter_cycles_and_sort_keeps_identity() {
        let mut model = loaded();
        let mut uninstalled = game(40, "A new game", Some(100));
        uninstalled.installed = None;
        model.set_catalog(CatalogSnapshot {
            items: vec![game(10, "Alpha", Some(10)), uninstalled],
            warnings: vec![],
        });
        model.selected = 1;
        model.key(key('v'));
        assert_eq!(model.filter, LibraryFilter::Installed);
        assert_eq!(model.selected_game().unwrap().app_id, 10);
        model.key(key('v'));
        assert_eq!(model.filter, LibraryFilter::NotInstalled);
        assert_eq!(model.selected_game().unwrap().app_id, 40);
        model.key(key('2'));
        assert_eq!(model.selected_game().unwrap().app_id, 40);
        model.key(key('v'));
        assert_eq!(model.order.len(), 2);
        assert_eq!(model.selected_game().unwrap().app_id, 40);
    }

    #[test]
    fn destination_blocks_insufficient_space_and_custom_edits_are_modal() {
        let mut model = loaded();
        model.modal = Some(modal(Some(99)));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(model.key(enter), Action::None);
        assert!(model.modal.as_ref().unwrap().error.is_some());
        assert_eq!(model.key(enter), Action::StartInstall);
        model.modal.as_mut().unwrap().low_space_confirmed = false;
        model.key(key('n'));
        model.key(key('q'));
        assert_eq!(model.modal.as_ref().unwrap().custom.as_deref(), Some("q"));
        assert_eq!(model.key(enter), Action::CheckDestination);
        assert!(model.modal.as_ref().unwrap().checking);
        assert_eq!(model.key(enter), Action::None);
    }

    #[test]
    fn install_cancel_and_duplicate_job_states() {
        let mut model = loaded();
        model.games[0].installed = None;
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Install
        );
        assert_eq!(model.key(key('i')), Action::None);
        model.preparing_install = false;
        model.install_active = true;
        assert_eq!(model.key(key('i')), Action::None);
        assert_eq!(model.key(key('c')), Action::CancelInstall);
        assert!(model.cancel_requested);
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        model.modal = Some(modal(Some(100)));
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::StartInstall
        );
        model.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(model.modal.is_none());
    }

    #[test]
    fn commit_changes_filter_membership_and_current_update_action() {
        let mut model = loaded();
        let installed = model.games[0].installed.take().unwrap();
        model.key(key('v'));
        model.key(key('v'));
        assert_eq!(model.selected_game().unwrap().app_id, 10);
        model.installed(installed);
        assert!(model.order.is_empty());
        model.key(key('v'));
        assert_eq!(model.selected_game().unwrap().app_id, 10);
        assert!(matches!(
            model.selected_game().unwrap().update,
            UpdateStatus::UpToDate
        ));
        assert_eq!(model.key(key('u')), Action::None);
        assert!(!model.preparing_install);
    }

    #[test]
    fn active_update_blocks_only_target_direct_launch() {
        let mut model = loaded();
        model.install_active = true;
        model.active_install = Some((10, "/library".into()));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(model.key(enter), Action::None);
        model.active_shader = true;
        assert_eq!(model.key(enter), Action::Launch(LaunchBackend::BuiltIn));
        model.active_shader = false;
        model.selected = 1;
        assert_eq!(model.key(enter), Action::Launch(LaunchBackend::BuiltIn));
    }

    #[test]
    fn dependency_install_prevents_duplicate_launch_and_manual_install_requests() {
        let mut model = loaded();
        model.pending_launch = Some((
            model.selected_game().unwrap().installed.clone().unwrap(),
            LaunchBackend::BuiltIn,
        ));
        model.preparing_install = true;
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            Action::None
        );
        assert_eq!(model.key(key('u')), Action::None);
        model.preparing_install = false;
        model.install_active = true;
        model.active_install = Some((300, "/library".into()));
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        model.pending_launch = None;
        model.install_active = false;
        model.active_install = None;
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Launch(LaunchBackend::BuiltIn)
        );
    }
    #[test]
    fn installed_version_is_explicit_and_private_password_typing_never_triggers_shortcuts() {
        let mut model = loaded();
        assert_eq!(
            model.key(key('l')),
            Action::LaunchInstalled(LaunchBackend::BuiltIn)
        );
        // Shift+Enter is no longer a second spelling of `l`.
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Action::Launch(LaunchBackend::BuiltIn)
        );
        assert_eq!(model.key(key('m')), Action::ContentProperties);
        let props = model.properties.as_mut().unwrap();
        props.busy = false;
        props.data = Some(steam_core::content_options::Properties {
            app_id: props.game.app_id,
            branches: vec![steam_core::content_options::Branch {
                name: "secret".into(),
                password_required: true,
            }],
            languages: vec!["english".into()],
            dlc: vec![],
        });
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        for c in ['q', 'd', 'm', 'r'] {
            assert_eq!(model.key(key(c)), Action::None);
        }
        assert_eq!(
            model
                .properties
                .as_ref()
                .unwrap()
                .password
                .as_ref()
                .unwrap()
                .as_str(),
            "qdmr"
        );
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)),
            Action::None
        );
        assert!(!model.properties.as_ref().unwrap().remember_key);
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::UnlockBranch
        );
    }
}

#[cfg(test)]
mod speed_tests {
    use super::*;
    use steam_core::install::InstallPhase;

    const MIB: u64 = 1024 * 1024;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn sample(phase: InstallPhase, downloaded: u64) -> InstallProgress {
        InstallProgress {
            app_id: 440,
            phase,
            completed_bytes: 0,
            total_bytes: 100 * MIB,
            downloaded_bytes: downloaded,
            total_download_bytes: 50 * MIB,
            verify_bytes_done: 0,
            verify_bytes_total: 100 * MIB,
            message: String::new(),
        }
    }

    #[test]
    fn steady_rate_is_measured_over_the_window() {
        let base = Instant::now();
        let mut tracker = SpeedTracker::default();
        tracker.start(base);
        for second in 0..=10 {
            tracker.record(at(base, second), second * 2 * MIB);
        }
        let speed = tracker.speed(at(base, 10)).unwrap();
        assert!((speed - 2.0 * MIB as f64).abs() < 1.0, "{speed}");
    }

    #[test]
    fn unknown_before_one_second_of_history() {
        let base = Instant::now();
        let mut tracker = SpeedTracker::default();
        tracker.start(base);
        assert_eq!(tracker.speed(base), None);
        tracker.record(base, 0);
        tracker.record(base + Duration::from_millis(500), MIB);
        assert_eq!(tracker.speed(base + Duration::from_millis(500)), None);
    }

    #[test]
    fn stall_reports_no_speed_and_no_eta() {
        let base = Instant::now();
        let mut tracker = SpeedTracker::default();
        tracker.start(base);
        for second in 0..=5 {
            tracker.record(at(base, second), second * MIB);
        }
        assert!(tracker.speed(at(base, 5)).is_some());
        tracker.record(at(base, 15), 5 * MIB);
        tracker.record(at(base, 20), 5 * MIB);
        assert_eq!(tracker.speed(at(base, 20)), None);
        assert_eq!(tracker.eta(at(base, 20), MIB), None);
    }

    #[test]
    fn eta_is_remaining_over_speed() {
        let base = Instant::now();
        let mut tracker = SpeedTracker::default();
        tracker.start(base);
        for second in 0..=4 {
            tracker.record(at(base, second), second * 4 * MIB);
        }
        let eta = tracker.eta(at(base, 4), 40 * MIB).unwrap();
        assert_eq!(eta.as_secs(), 10);
        assert_eq!(tracker.eta(at(base, 4), 0), Some(Duration::ZERO));
    }

    #[test]
    fn decreasing_counter_restarts_the_window() {
        let base = Instant::now();
        let mut tracker = SpeedTracker::default();
        tracker.start(base);
        tracker.record(base, 0);
        tracker.record(at(base, 2), 100 * MIB);
        tracker.record(at(base, 3), MIB);
        assert_eq!(tracker.speed(at(base, 3)), None);
        tracker.record(at(base, 5), 3 * MIB);
        let speed = tracker.speed(at(base, 5)).unwrap();
        assert!((speed - MIB as f64).abs() < 1.0);
    }

    #[test]
    fn new_install_resets_and_terminal_phase_freezes() {
        let base = Instant::now();
        let mut model = Model::new(true, None);
        model.begin_install_at(base);
        for second in 0..=3 {
            model.apply_progress_at(
                sample(InstallPhase::Downloading, second * MIB),
                at(base, second),
            );
        }
        assert!(model.speed.speed(at(base, 3)).is_some());
        model.apply_progress_at(sample(InstallPhase::Finished, 3 * MIB), at(base, 4));
        assert_eq!(model.speed.speed(at(base, 5)), None);
        assert_eq!(
            model.speed.elapsed(at(base, 60)),
            Some(Duration::from_secs(4))
        );
        model.begin_install_at(at(base, 100));
        assert!(model.progress.is_none());
        assert_eq!(model.speed.speed(at(base, 103)), None);
        assert_eq!(
            model.speed.elapsed(at(base, 103)),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn complete_install_marks_finished_and_freezes() {
        let base = Instant::now();
        let mut model = Model::new(true, None);
        model.begin_install_at(base);
        model.apply_progress_at(sample(InstallPhase::Committing, MIB), at(base, 2));
        model.complete_install_at(at(base, 3));
        let progress = model.progress.as_ref().unwrap();
        assert_eq!(progress.phase, InstallPhase::Finished);
        assert_eq!(progress.completed_bytes, progress.total_bytes);
        assert_eq!(
            model.speed.elapsed(at(base, 90)),
            Some(Duration::from_secs(3))
        );
    }
}

#[cfg(test)]
mod pause_tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn downloading(downloaded: u64) -> InstallProgress {
        InstallProgress {
            app_id: 1,
            phase: InstallPhase::Downloading,
            completed_bytes: 0,
            total_bytes: 100 * MIB,
            downloaded_bytes: downloaded,
            total_download_bytes: 50 * MIB,
            verify_bytes_done: 0,
            verify_bytes_total: 100 * MIB,
            message: String::new(),
        }
    }

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    #[test]
    fn paused_time_is_excluded_from_speed_and_elapsed() {
        let base = Instant::now();
        let mut tracker = SpeedTracker::default();
        tracker.start(base);
        for second in 0..=4 {
            tracker.record(at(base, second), second * 2 * MIB);
        }
        tracker.pause(at(base, 4));
        assert_eq!(tracker.speed(at(base, 20)), None);
        assert_eq!(tracker.eta(at(base, 20), MIB), None);
        assert_eq!(tracker.elapsed(at(base, 20)), Some(Duration::from_secs(4)));
        // Samples reported while paused (an in-flight chunk) are ignored.
        tracker.record(at(base, 10), 20 * MIB);
        tracker.resume(at(base, 24));
        assert_eq!(tracker.speed(at(base, 24)), None);
        tracker.record(at(base, 24), 8 * MIB);
        tracker.record(at(base, 26), 12 * MIB);
        let speed = tracker.speed(at(base, 26)).unwrap();
        assert!((speed - 2.0 * MIB as f64).abs() < 1.0, "{speed}");
        assert_eq!(tracker.elapsed(at(base, 26)), Some(Duration::from_secs(6)));
    }

    #[test]
    fn freezing_while_paused_does_not_count_the_pause() {
        let base = Instant::now();
        let mut tracker = SpeedTracker::default();
        tracker.start(base);
        tracker.pause(at(base, 3));
        tracker.freeze(at(base, 10));
        assert_eq!(tracker.elapsed(at(base, 99)), Some(Duration::from_secs(3)));
    }

    #[test]
    fn p_toggles_only_while_an_install_is_active() {
        let mut model = Model::new(true, None);
        assert_eq!(model.key(key('p')), Action::None);
        assert!(!model.paused);
        model.install_active = true;
        assert_eq!(model.key(key('p')), Action::TogglePause);
        assert!(model.paused);
        assert_eq!(model.status, "Paused — p resumes");
        assert_eq!(model.key(key('p')), Action::TogglePause);
        assert!(!model.paused);
        assert_eq!(model.status, "Resumed");
    }

    #[test]
    fn p_is_text_while_filtering() {
        let mut model = Model::new(true, None);
        model.install_active = true;
        model.key(key('/'));
        assert_eq!(model.key(key('p')), Action::None);
        assert_eq!(model.search, "p");
        assert!(!model.paused);
    }

    #[test]
    fn cancel_while_paused_clears_pause_and_pause_stops_after_downloading() {
        let mut model = Model::new(true, None);
        model.install_active = true;
        model.begin_install();
        model.key(key('p'));
        assert_eq!(model.key(key('c')), Action::CancelInstall);
        assert!(!model.paused && model.cancel_requested);
        assert_eq!(model.key(key('p')), Action::None);
        assert!(!model.paused);

        let mut model = Model::new(true, None);
        model.install_active = true;
        let base = Instant::now();
        model.begin_install_at(base);
        model.apply_progress_at(downloading(MIB), at(base, 1));
        model.key(key('p'));
        assert!(model.paused);
        let mut verifying = downloading(MIB);
        verifying.phase = InstallPhase::Verifying;
        model.apply_progress_at(verifying.clone(), at(base, 2));
        assert!(!model.paused);
        assert_eq!(model.key(key('p')), Action::None);
        assert!(!model.paused);
    }

    #[test]
    fn new_install_and_completion_clear_pause() {
        let mut model = Model::new(true, None);
        model.install_active = true;
        model.key(key('p'));
        model.begin_install();
        assert!(!model.paused);
        model.key(key('p'));
        model.clear_progress();
        assert!(!model.paused);
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    fn code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn note(id: u64, title: &str) -> NotificationEvent {
        NotificationEvent::Received(Notification {
            id,
            timestamp: 1_700_000_000 + id,
            label: "General".into(),
            title: title.into(),
            body: String::new(),
            read: false,
        })
    }

    fn titles(model: &Model) -> Vec<&str> {
        model
            .notifications
            .iter()
            .map(|note| note.title.as_str())
            .collect()
    }

    #[test]
    fn feed_is_newest_first_deduplicated_and_keeps_all_entries() {
        let mut model = Model::new(false, None);
        model.notification_event(note(1, "a"));
        model.notification_event(note(2, "b"));
        assert_eq!(titles(&model), ["b", "a"]);
        model.notification_event(note(1, "a2"));
        assert_eq!(titles(&model), ["b", "a2"]);
        // Id 0 means unknown and is never merged.
        model.notification_event(note(0, "z"));
        model.notification_event(note(0, "z"));
        assert_eq!(model.notifications.len(), 4);
        for id in 10..300 {
            model.notification_event(note(id, "n"));
        }
        assert_eq!(model.notifications.len(), 294);
        assert_eq!(model.notif_unseen, 294);
    }

    #[test]
    fn history_merges_chronologically_without_new_badges_or_overwriting_live_updates() {
        let mut model = Model::new(false, None);
        model.notification_event(note(500, "live"));
        let history = (1..=200)
            .rev()
            .chain([500, 3])
            .map(|id| Notification {
                id,
                timestamp: 1_700_000_000 + id,
                label: "General".into(),
                title: if id == 500 {
                    "live".into()
                } else {
                    format!("old {id}")
                },
                body: String::new(),
                read: true,
            })
            .collect();
        model.notification_event(NotificationEvent::History(history));
        assert_eq!(model.notifications.len(), 201);
        assert_eq!(model.notifications[0].title, "live");
        assert_eq!(model.notifications[1].id, 200);
        assert_eq!(model.notifications.last().unwrap().id, 1);
        assert_eq!(model.notif_unseen, 1);
        model.notification_event(note(3, "updated old entry"));
        assert_eq!(model.notif_unseen, 1);
        assert_eq!(model.notifications[0].id, 500);
        model.notification_event(NotificationEvent::Removed(500));
        assert_eq!(model.notifications.len(), 200);
        assert_eq!(model.notif_unseen, 0);
        model.notification_event(NotificationEvent::History(vec![Notification {
            id: 900,
            timestamp: 1_700_000_900,
            label: "bad\u{1b}".into(),
            title: "safe\u{1b}[H".into(),
            body: "text\u{07}".into(),
            read: true,
        }]));
        assert_eq!(model.notifications[0].title, "safe[H");
        assert_eq!(model.notif_unseen, 0);
    }

    #[test]
    fn counts_replace_and_status_is_sanitized() {
        let mut model = Model::new(false, None);
        model.notification_event(NotificationEvent::Counts(vec![(CountKind::Gifts, 2)]));
        model.notification_event(NotificationEvent::Counts(vec![(CountKind::Items, 1)]));
        assert_eq!(model.notif_counts, [(CountKind::Items, 1)]);
        model.notification_event(NotificationEvent::Status("x\u{1b}[2Jy".into()));
        assert_eq!(model.notif_status, "x[2Jy");
        model.notification_event(NotificationEvent::Received(Notification {
            id: 5,
            timestamp: 0,
            label: "a\u{1b}b".into(),
            title: "c\u{7}d".into(),
            body: "e\u{9b}f".into(),
            read: false,
        }));
        let stored = &model.notifications[0];
        assert_eq!(
            (
                stored.label.as_str(),
                stored.title.as_str(),
                stored.body.as_str()
            ),
            ("ab", "cd", "ef")
        );
    }

    #[test]
    fn offline_and_failed_login_start_with_an_explanatory_status() {
        let offline = Model::new(true, None);
        assert!(!offline.notifications_live && offline.notif_status.contains("Offline"));
        let failed = Model::new(false, Some("no token".into()));
        assert!(!failed.notifications_live && failed.notif_status.contains("no token"));
        let live = Model::new(false, None);
        assert!(live.notifications_live && live.notif_status.is_empty());
    }

    #[test]
    fn tab_cycles_library_and_friends_without_acknowledging_notifications() {
        let mut model = Model::new(false, None);
        model.notification_event(note(1, "a"));
        for _ in 0..3 {
            model.key(code(KeyCode::Tab));
            assert_eq!(model.focus, Focus::Friends);
            model.key(code(KeyCode::Tab));
            assert_eq!(model.focus, Focus::Library);
            model.key(code(KeyCode::BackTab));
            assert_eq!(model.focus, Focus::Friends);
            model.key(code(KeyCode::BackTab));
            assert_eq!(model.focus, Focus::Library);
        }
        assert_eq!(model.notif_unseen, 1);
        model.key(code(KeyCode::PageDown));
        assert_eq!(model.notif_unseen, 0);
        assert!(model.notif_fresh.is_empty());
        assert_eq!(model.focus, Focus::Library);
        model.notification_event(note(2, "b"));
        assert_eq!(model.notif_unseen, 1);
    }

    #[test]
    fn page_keys_scroll_notifications_without_changing_focus_or_selection() {
        let mut model = Model::new(false, None);
        model.notif_rows = 3;
        model.friend_rows = 3;
        for id in 1..=10 {
            model.notification_event(note(id, "n"));
        }
        for focus in [Focus::Library, Focus::Friends] {
            model.focus = focus;
            model.notif_offset = 0;
            let selections = (
                model.selected,
                model.friend_selected,
                model.library_offset,
                model.friend_offset,
            );
            model.key(code(KeyCode::PageDown));
            assert_eq!(model.notif_offset, 3);
            for _ in 0..10 {
                model.key(code(KeyCode::PageDown));
            }
            assert_eq!(model.notif_offset, 7);
            for _ in 0..10 {
                model.key(code(KeyCode::PageUp));
            }
            assert_eq!(model.notif_offset, 0);
            assert_eq!(model.focus, focus);
            assert_eq!(
                (
                    model.selected,
                    model.friend_selected,
                    model.library_offset,
                    model.friend_offset
                ),
                selections
            );
        }
        model.notif_offset = 4;
        model.notification_event(note(99, "fresh"));
        assert_eq!(
            model.notif_offset, 4,
            "new arrivals must not reset the scroll position"
        );
        model.searching = true;
        model.key(code(KeyCode::PageUp));
        assert_eq!(model.notif_offset, 1);
        assert!(model.searching);
    }

    #[test]
    fn x_clears_the_feed_but_is_text_while_filtering() {
        let mut model = Model::new(false, None);
        model.notification_event(note(1, "a"));
        model.key(key('/'));
        model.key(key('x'));
        assert_eq!(model.search, "x");
        assert_eq!(model.notifications.len(), 1);
        model.key(code(KeyCode::Enter));
        model.key(code(KeyCode::Esc));
        model.key(key('x'));
        assert!(model.notifications.is_empty());
        model.notification_event(note(2, "b"));
        model.focus = Focus::Friends;
        model.key(key('x'));
        assert!(model.notifications.is_empty());
        assert_eq!(model.notif_unseen, 0);
    }

    #[test]
    fn install_modal_custom_path_keeps_x_and_n_as_text() {
        let mut model = Model::new(false, None);
        model.notification_event(note(1, "a"));
        model.modal = Some(InstallModal {
            plan: steam_core::catalog::InstallPlan {
                app_id: 1,
                name: "G".into(),
                install_dir_name: "G".into(),
                build_id: 1,
                platform: steam_core::catalog::InstallPlatform::Linux,
                branch: "public".into(),
                language: "english".into(),
                shared_depots: Default::default(),
                dlc_depots: Default::default(),
                depots: vec![],
                download_bytes: 1,
                installed_bytes: 1,
            },
            existing: None,
            steam_root: "/steam".into(),
            destinations: vec![],
            selected: 0,
            custom: None,
            checking: false,
            low_space_confirmed: false,
            error: None,
        });
        model.key(key('n'));
        model.key(key('x'));
        model.key(key('n'));
        assert_eq!(model.modal.as_ref().unwrap().custom.as_deref(), Some("xn"));
        assert_eq!(model.notifications.len(), 1);
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn lost_and_restored_connection_update_panel_messages() {
        let mut model = Model::new(false, Some("login failed".into()));
        assert!(!model.notifications_live);
        model.connection_lost("Steam connection lost — reconnecting…");
        assert_eq!(
            model.friends_status,
            "Steam connection lost — reconnecting…"
        );
        assert_eq!(model.notif_status, "Steam connection lost — reconnecting…");
        model.inventory_error = Some("old".into());
        model.connection_restored();
        assert!(model.notifications_live && model.notif_status.is_empty());
        assert_eq!(model.friends_status, "Loading friends…");
        assert!(model.inventory_error.is_none());
    }
}

#[cfg(test)]
mod verify_tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn verifying(done: u64) -> InstallProgress {
        InstallProgress {
            app_id: 1,
            phase: InstallPhase::Verifying,
            completed_bytes: 30 * GIB,
            total_bytes: 30 * GIB,
            downloaded_bytes: 2 * GIB,
            total_download_bytes: 2 * GIB,
            verify_bytes_done: done,
            verify_bytes_total: 30 * GIB,
            message: String::new(),
        }
    }

    #[test]
    fn verification_rate_and_eta_come_from_hashed_bytes() {
        let base = Instant::now();
        let mut model = Model::new(true, None);
        model.begin_install_at(base);
        for second in 0..=4 {
            model.apply_progress_at(verifying(second * 2 * GIB), at(base, second));
        }
        let speed = model.verify_speed.speed(at(base, 4)).unwrap();
        assert!((speed - 2.0 * GIB as f64).abs() < 1.0, "{speed}");
        let eta = model
            .verify_speed
            .eta(at(base, 4), 30 * GIB - 8 * GIB)
            .unwrap();
        assert_eq!(eta.as_secs(), 11);
        // The download tracker is untouched by verification samples.
        assert_eq!(model.speed.speed(at(base, 4)), None);
    }

    #[test]
    fn verification_clock_freezes_on_finish_and_resets_with_a_new_install() {
        let base = Instant::now();
        let mut model = Model::new(true, None);
        model.begin_install_at(base);
        model.apply_progress_at(verifying(0), base);
        model.apply_progress_at(verifying(4 * GIB), at(base, 2));
        model.complete_install_at(at(base, 3));
        assert_eq!(model.verify_speed.speed(at(base, 10)), None);
        let progress = model.progress.as_ref().unwrap();
        assert_eq!(progress.verify_bytes_done, progress.verify_bytes_total);
        model.begin_install_at(at(base, 20));
        model.apply_progress_at(verifying(GIB), at(base, 21));
        assert_eq!(model.verify_speed.speed(at(base, 21)), None, "fresh window");
    }

    #[test]
    fn pause_is_refused_during_verification_and_clears_a_pending_pause() {
        let base = Instant::now();
        let mut model = Model::new(true, None);
        model.install_active = true;
        model.begin_install_at(base);
        model.apply_progress_at(verifying(GIB), at(base, 1));
        assert_eq!(model.toggle_pause_at(at(base, 2)), Action::None);
        assert!(!model.paused);
        assert_eq!(model.status, "Pause only applies while downloading");
    }
}

#[cfg(test)]
mod cloud_tests {
    use super::*;

    fn game(app_id: u32, installed: bool) -> CatalogItem {
        CatalogItem {
            app_id,
            name: format!("Game {app_id}"),
            installed: installed.then(|| InstalledGame {
                app_id,
                name: format!("Game {app_id}"),
                size_bytes: None,
                last_played: None,
                playtime_minutes: None,
                install_dir: PathBuf::new(),
                library_root: PathBuf::new(),
                steam_root: PathBuf::new(),
                launch_options: None,
                build_id: None,
                branch: "public".into(),
                language: "english".into(),
                platform: None,
                installed_depots: Default::default(),
            }),
            plan: None,
            update: UpdateStatus::Unknown,
            last_played: None,
            playtime_minutes: None,
            size_bytes: None,
            unavailable_reason: None,
        }
    }

    fn model() -> Model {
        let mut model = Model::new(false, None);
        model.set_catalog(CatalogSnapshot {
            items: vec![game(10, true), game(20, false)],
            warnings: Vec::new(),
        });
        model
    }

    fn sync_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
    }

    #[test]
    fn y_starts_a_cloud_sync_of_the_selected_installed_game() {
        let mut model = model();
        assert_eq!(model.key(sync_key()), Action::CloudSync);
        assert!(model.cloud_busy.contains(&10));
        assert!(model.status.starts_with("Steam Cloud: syncing"));
        // Not twice at once, and launching waits for the sync.
        assert_eq!(model.key(sync_key()), Action::None);
        assert!(model.status.contains("already syncing"));
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert!(model.status.contains("wait for it to finish"));
        model.cloud_busy.clear();
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Launch(LaunchBackend::BuiltIn)
        );
    }

    #[test]
    fn cloud_sync_is_refused_when_it_cannot_run() {
        let mut model = model();
        model.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(model.selected_game().unwrap().app_id, 20);
        assert_eq!(model.key(sync_key()), Action::None);
        assert!(model.status.contains("needs an installed game"));
        model.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        model.launching.insert(10);
        assert_eq!(model.key(sync_key()), Action::None);
        model.launching.clear();
        model.install_active = true;
        model.active_install = Some((10, "/lib".into()));
        assert_eq!(model.key(sync_key()), Action::None);
        assert!(model.status.contains("install/update"));
        assert!(model.cloud_busy.is_empty());
        assert_eq!(Model::new(true, None).key(sync_key()), Action::None);
    }

    #[test]
    fn the_sync_key_is_filter_text_while_typing() {
        let mut model = model();
        model.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(model.searching);
        assert_eq!(model.key(sync_key()), Action::None);
        assert_eq!(model.search, "y");
        assert!(model.cloud_busy.is_empty());
    }
}

#[cfg(test)]
mod cloud_icon_tests {
    use super::*;

    fn item(app_id: u32, installed: bool) -> CatalogItem {
        CatalogItem {
            app_id,
            name: format!("G{app_id}"),
            installed: installed.then(|| InstalledGame {
                app_id,
                name: format!("G{app_id}"),
                size_bytes: None,
                last_played: None,
                playtime_minutes: None,
                install_dir: PathBuf::new(),
                library_root: PathBuf::new(),
                steam_root: PathBuf::new(),
                launch_options: None,
                build_id: None,
                branch: "public".into(),
                language: "english".into(),
                platform: None,
                installed_depots: Default::default(),
            }),
            plan: None,
            update: UpdateStatus::Unknown,
            last_played: None,
            playtime_minutes: None,
            size_bytes: None,
            unavailable_reason: None,
        }
    }

    #[test]
    fn icon_priority_and_blanks() {
        let mut model = Model::new(true, None);
        let (installed, other) = (item(1, true), item(2, false));
        // No cloud config known: blank. Not installed: blank even with a badge.
        assert_eq!(model.cloud_icon(&installed, 0), None);
        model.cloud_badges.insert(2, CloudBadge::Synced);
        assert_eq!(model.cloud_icon(&other, 0), None);
        for (badge, icon) in [
            (CloudBadge::Never, CloudIcon::Never),
            (CloudBadge::Synced, CloudIcon::Synced),
            (CloudBadge::Pending, CloudIcon::Pending),
        ] {
            model.cloud_badges.insert(1, badge);
            assert_eq!(model.cloud_icon(&installed, 0), Some(icon));
        }
        // A good last result changes nothing; failure/conflicts show a problem.
        model.cloud_last.insert(1, CloudResult::Ok);
        assert_eq!(model.cloud_icon(&installed, 0), Some(CloudIcon::Pending));
        for bad in [CloudResult::Failed, CloudResult::Conflicts] {
            model.cloud_last.insert(1, bad);
            assert_eq!(model.cloud_icon(&installed, 0), Some(CloudIcon::Problem));
        }
        // Syncing wins over everything and carries the spinner frame.
        model.cloud_busy.insert(1);
        assert_eq!(model.cloud_icon(&installed, 3), Some(CloudIcon::Syncing(3)));
        model.cloud_busy.clear();
        // A launch (which syncs first) spins only for cloud-configured games.
        model.launching.insert(1);
        assert_eq!(model.cloud_icon(&installed, 1), Some(CloudIcon::Syncing(1)));
        model.launching.clear();
        model.launching.insert(5);
        assert_eq!(model.cloud_icon(&item(5, true), 0), None);
    }
}
