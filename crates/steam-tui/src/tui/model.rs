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
    Notifications,
}

impl Focus {
    pub fn next(self) -> Self {
        match self {
            Self::Library => Self::Friends,
            Self::Friends => Self::Notifications,
            Self::Notifications => Self::Library,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Library => Self::Notifications,
            Self::Friends => Self::Library,
            Self::Notifications => Self::Friends,
        }
    }
}

const MAX_NOTIFICATIONS: usize = 100;

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
    pub achievements: Option<super::achievements::AchievementsView>,
    pub cloud_conflicts: Option<CloudConflicts>,
    pub cloud_conflict_queue: VecDeque<CloudConflicts>,
    news_request: u64,
    achievements_request: u64,
    last_library_click: Option<(u32, Instant)>,
    pub friends: Vec<Friend>,
    pub friend_order: Vec<usize>,
    pub friend_offset: usize,
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
    /// Newest first; capped at `MAX_NOTIFICATIONS`.
    pub notifications: Vec<Notification>,
    /// Non-zero counts; each update replaces the whole summary.
    pub notif_counts: Vec<(CountKind, u32)>,
    /// Offline/error text; empty while the live feed is healthy.
    pub notif_status: String,
    /// False without a Steam connection (offline mode or failed login).
    pub notifications_live: bool,
    pub account: Option<String>,
    pub notif_offset: usize,
    pub notif_rows: usize,
    /// Items received since the feed was last focused.
    pub notif_unseen: usize,
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
            achievements: None,
            cloud_conflicts: None,
            cloud_conflict_queue: VecDeque::new(),
            news_request: 0,
            achievements_request: 0,
            last_library_click: None,
            friends: Vec::new(),
            friend_order: Vec::new(),
            friend_offset: 0,
            friend_rows: 1,
            account: None,
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
            "sort={sort}\nreverse={}\nfilter={filter}\nshow_offline={}\npins={}\n",
            self.reverse,
            self.show_offline,
            pins.join(",")
        )
    }

    /// Unknown keys and malformed values are ignored.
    pub fn apply_settings(&mut self, text: &str) {
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
        self.friends = friends;
        self.filter_friends();
    }

    fn filter_friends(&mut self) {
        self.friend_order = self
            .friends
            .iter()
            .enumerate()
            .filter_map(|(index, friend)| {
                (self.show_offline || !friend.state.is_offline()).then_some(index)
            })
            .collect();
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

    fn page_friends(&mut self, down: bool, page: bool) {
        let amount = if page { self.friend_rows.max(1) } else { 1 };
        self.friend_offset = if down {
            self.friend_offset.saturating_add(amount)
        } else {
            self.friend_offset.saturating_sub(amount)
        };
        self.friend_offset = bounded_offset(
            self.friend_offset,
            self.friend_order.len(),
            self.friend_rows,
        );
    }

    /// PgUp/PgDn scroll the notification feed when it has focus, else the friends.
    fn page_pane(&mut self, down: bool) {
        if self.focus == Focus::Notifications {
            self.page_notifications(down, true);
        } else {
            self.page_friends(down, true);
        }
    }

    fn page_notifications(&mut self, down: bool, page: bool) {
        let amount = if page { self.notif_rows.max(1) } else { 1 };
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
        self.status = "Notification feed cleared".into();
    }

    /// The Steam session is gone: say so in the panels that depend on it.
    pub fn connection_lost(&mut self, message: &str) {
        self.friends_status = message.to_owned();
        self.notif_status = message.to_owned();
    }

    /// A fresh session replaced the old one; its workers are about to report.
    pub fn connection_restored(&mut self) {
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
                if notification.id != 0 {
                    self.notifications
                        .retain(|existing| existing.id != notification.id);
                }
                self.notifications.insert(0, notification);
                self.notifications.truncate(MAX_NOTIFICATIONS);
                if self.focus != Focus::Notifications {
                    self.notif_unseen = (self.notif_unseen + 1).min(self.notifications.len());
                } else {
                    self.notif_offset = 0;
                }
            }
        }
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
        if self.modal.is_some()
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
        if self.modal.is_some() {
            return self.modal_key(key);
        }
        if let Some(modal) = &mut self.cloud_conflicts {
            if key.code == KeyCode::Char('q') {
                return Action::Quit;
            }
            if modal.busy {
                return Action::None;
            }
            match key.code {
                KeyCode::Esc => self.cloud_conflicts = self.cloud_conflict_queue.pop_front(),
                KeyCode::Char('q') => return Action::Quit,
                KeyCode::Up => {
                    modal.index = modal.index.saturating_sub(1);
                    modal.choice = None;
                }
                KeyCode::Down => {
                    modal.index = (modal.index + 1).min(modal.items.len().saturating_sub(1));
                    modal.choice = None;
                }
                KeyCode::Char('l') => modal.choice = Some(steam_core::cloud::Choice::Local),
                KeyCode::Char('r') => modal.choice = Some(steam_core::cloud::Choice::Cloud),
                KeyCode::Enter if modal.choice.is_some() && !modal.items.is_empty() => {
                    modal.busy = true;
                    self.cloud_busy.insert(modal.game.app_id);
                    return Action::ResolveCloud;
                }
                _ => {}
            }
            return Action::None;
        }
        if let Some(view) = &mut self.achievements {
            match key.code {
                KeyCode::Char('q') => return Action::Quit,
                KeyCode::Esc | KeyCode::Char('a') => {
                    self.achievements = None;
                    return Action::None;
                }
                KeyCode::Down | KeyCode::Char('j') => view.step(1),
                KeyCode::Up | KeyCode::Char('k') => view.step(-1),
                KeyCode::PageDown => view.step(10),
                KeyCode::PageUp => view.step(-10),
                KeyCode::Home => view.selected = 0,
                KeyCode::End => view.selected = usize::MAX,
                KeyCode::Char('s') => view.cycle_sort(),
                KeyCode::Char('S') => view.flip(),
                KeyCode::Char('f') => view.cycle_filter(),
                KeyCode::Char('r') => {
                    view.result = None;
                    return Action::Achievements;
                }
                _ => {}
            }
            let last = view.visible().len().saturating_sub(1);
            view.selected = view.selected.min(last);
            return Action::None;
        }
        if let Some(news) = &mut self.news {
            match key.code {
                KeyCode::Char('q') => return Action::Quit,
                KeyCode::Esc | KeyCode::Char('n') => self.news = None,
                KeyCode::Down | KeyCode::Char('j') => news.offset = news.offset.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => news.offset = news.offset.saturating_sub(1),
                KeyCode::PageDown => news.offset = news.offset.saturating_add(10),
                KeyCode::PageUp => news.offset = news.offset.saturating_sub(10),
                KeyCode::Home => news.offset = 0,
                KeyCode::End => news.offset = u16::MAX,
                _ => {}
            }
            return Action::None;
        }
        if self.queue_view {
            match key.code {
                KeyCode::Esc | KeyCode::Char('J') => self.queue_view = false,
                KeyCode::Char('q') => return Action::Quit,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.queue_selected =
                        (self.queue_selected + 1).min(self.queue_jobs.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.queue_selected = self.queue_selected.saturating_sub(1)
                }
                KeyCode::Char('p' | 'r' | 'c' | '+' | '-' | 'a') => {
                    if let Some(job) = self.queue_jobs.get(self.queue_selected) {
                        return match key.code {
                            KeyCode::Char('p') => Action::QueueControl(
                                job.id,
                                if job.state == steam_core::updates::JobState::Paused {
                                    steam_core::updates::JobState::Pending
                                } else {
                                    steam_core::updates::JobState::Paused
                                },
                            ),
                            KeyCode::Char('r') => {
                                Action::QueueControl(job.id, steam_core::updates::JobState::Pending)
                            }
                            KeyCode::Char('c') => Action::QueueControl(
                                job.id,
                                steam_core::updates::JobState::Cancelled,
                            ),
                            KeyCode::Char('+') => Action::QueuePriority(job.intent.app_id, 1),
                            KeyCode::Char('-') => Action::QueuePriority(job.intent.app_id, -1),
                            _ => Action::QueuePolicy(job.intent.app_id),
                        };
                    }
                }
                _ => {}
            }
            return Action::None;
        }
        if let Some(props) = &mut self.properties {
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
                    KeyCode::Char(c)
                        if !c.is_control() && password.len() + c.len_utf8() <= 4096 =>
                    {
                        password.push(c)
                    }
                    _ => {}
                }
                return Action::None;
            }
            if key.code == KeyCode::Char('q') {
                return Action::Quit;
            }
            if key.code == KeyCode::Esc {
                self.properties = None;
                return Action::None;
            }
            if props.busy {
                return Action::None;
            }
            if let Some(data) = &props.data {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        props.selected = (props.selected + 1).min(data.dlc.len() + 1)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        props.selected = props.selected.saturating_sub(1)
                    }
                    KeyCode::Left | KeyCode::Right if props.selected < 2 => {
                        let (index, count) = if props.selected == 0 {
                            (&mut props.branch, data.branches.len())
                        } else {
                            (&mut props.language, data.languages.len())
                        };
                        if count > 0 {
                            *index = if key.code == KeyCode::Right {
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
                            props.password =
                                Some(zeroize::Zeroizing::new(String::with_capacity(4096)));
                        } else {
                            props.busy = true;
                            return Action::ApplyContent;
                        }
                    }
                    KeyCode::Char(' ') | KeyCode::Enter => {
                        if let Some(dlc) =
                            props.selected.checked_sub(2).and_then(|i| data.dlc.get(i))
                        {
                            props.busy = true;
                            return Action::ToggleDlc(dlc.app_id, !dlc.enabled);
                        }
                    }
                    _ => {}
                }
            }
            return Action::None;
        }
        if self.backend_view {
            match key.code {
                KeyCode::Char('q') => return Action::Quit,
                KeyCode::Esc | KeyCode::Char('b' | 'B') => self.backend_view = false,
                KeyCode::Char('x') => {
                    self.backend_log.clear();
                    self.backend_offset = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.backend_offset = self.backend_offset.saturating_add(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.backend_offset = self.backend_offset.saturating_sub(1)
                }
                KeyCode::PageUp => self.backend_offset = self.backend_offset.saturating_add(10),
                KeyCode::PageDown => self.backend_offset = self.backend_offset.saturating_sub(10),
                KeyCode::End => self.backend_offset = 0,
                KeyCode::Home => self.backend_offset = usize::MAX / 2,
                _ => {}
            }
            return Action::None;
        }
        if self.help {
            match key.code {
                KeyCode::Char('q') => return Action::Quit,
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') => self.help = false,
                KeyCode::Down | KeyCode::PageDown => {
                    self.help_offset = self.help_offset.saturating_add(1)
                }
                KeyCode::Up | KeyCode::PageUp => {
                    self.help_offset = self.help_offset.saturating_sub(1)
                }
                _ => {}
            }
            return Action::None;
        }
        // Navigation is global, including while typing a search.
        match key.code {
            KeyCode::Up => {
                self.move_library(false);
                return Action::None;
            }
            KeyCode::Down => {
                self.move_library(true);
                return Action::None;
            }
            KeyCode::Home => {
                self.selected = 0;
                self.keep_visible();
                return Action::None;
            }
            KeyCode::End => {
                self.selected = self.order.len().saturating_sub(1);
                self.keep_visible();
                return Action::None;
            }
            KeyCode::PageUp => {
                self.page_pane(false);
                return Action::None;
            }
            KeyCode::PageDown => {
                self.page_pane(true);
                return Action::None;
            }
            KeyCode::F(1) => {
                self.help = true;
                return Action::None;
            }
            _ => {}
        }
        if self.searching {
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
            return Action::None;
        }
        let preserve = self.selected_game().map(|game| game.app_id);
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('S')
                if self
                    .selected_game()
                    .is_some_and(|g| self.launching.contains(&g.app_id)) =>
            {
                return Action::SkipShaders(self.selected_game().unwrap().app_id);
            }
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Char('n') => return self.open_news(),
            KeyCode::Char('a') => return self.open_achievements(),
            KeyCode::Enter | KeyCode::Char('L') => {
                if self.pending_launch.is_some() {
                    self.status =
                        "A launch is waiting for its dependency installation to finish".into();
                    return Action::None;
                }
                let backend = if key.modifiers.contains(KeyModifiers::CONTROL)
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
                    return Action::None;
                }
                if let Some(game) = self.selected_game().filter(|game| {
                    game.installed.is_some() && self.cloud_busy.contains(&game.app_id)
                }) {
                    self.status = format!(
                        "Steam Cloud is syncing {}; wait for it to finish",
                        game.name
                    );
                    return Action::None;
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
                        "Cannot launch a game while its install/update transaction is active"
                            .into();
                    return Action::None;
                }
                return if self
                    .selected_game()
                    .is_some_and(|game| game.installed.is_some())
                {
                    if key.code == KeyCode::Char('L') || key.modifiers.contains(KeyModifiers::SHIFT)
                    {
                        Action::LaunchInstalled(backend)
                    } else {
                        Action::Launch(backend)
                    }
                } else {
                    self.request_install(false)
                };
            }
            KeyCode::Char('i') => return self.request_install(false),
            KeyCode::Char('u') => return self.request_install(true),
            KeyCode::Char('c') if self.install_active => {
                self.set_paused(false, Instant::now());
                self.cancel_requested = true;
                self.status =
                    "Cancellation requested; waiting for a safe transaction boundary…".into();
                return Action::CancelInstall;
            }
            KeyCode::Char('p') if self.install_active => {
                return self.toggle_pause_at(Instant::now());
            }
            KeyCode::Char('J') => {
                self.queue_view = true;
                return Action::Queue;
            }
            KeyCode::Char('P') => {
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
            }
            KeyCode::Char('O') => match self.selected_game() {
                Some(game) if game.installed.is_some() => {
                    return Action::CycleLaunchOption(game.app_id);
                }
                _ => self.status = "Install the game before choosing a launch option".into(),
            },
            KeyCode::Char('Z') => match self.selected_game() {
                Some(game) if game.installed.is_some() => {
                    return Action::PrecacheShaders(game.app_id);
                }
                _ => self.status = "Install the game before pre-caching its shaders".into(),
            },
            KeyCode::Char('b') => self.backend_view = true,
            KeyCode::Char('C') => return self.request_cloud_sync(),
            KeyCode::Char('v') => {
                self.filter = self.filter.next();
                self.reorder(preserve);
            }
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('f') => {
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
            KeyCode::Esc => {
                self.search.clear();
                self.reorder(preserve);
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if key.code == KeyCode::Tab {
                    self.focus.next()
                } else {
                    self.focus.previous()
                };
                if self.focus == Focus::Notifications {
                    self.notif_unseen = 0;
                }
            }
            KeyCode::Char('o') => {
                self.show_offline = !self.show_offline;
                self.filter_friends();
            }
            KeyCode::Char('j' | 'k') => {
                let down = key.code == KeyCode::Char('j');
                match self.focus {
                    Focus::Library => self.move_library(down),
                    Focus::Friends => self.page_friends(down, false),
                    Focus::Notifications => self.page_notifications(down, false),
                }
            }
            KeyCode::Char('x') => self.clear_notifications(),
            KeyCode::Char('1' | '2' | '3' | '4' | 's' | 'S') => {
                match key.code {
                    KeyCode::Char('1') => self.sort = Sort::Name,
                    KeyCode::Char('2') => self.sort = Sort::Size,
                    KeyCode::Char('3') => self.sort = Sort::LastPlayed,
                    KeyCode::Char('4') => self.sort = Sort::PlayTime,
                    KeyCode::Char('s') => self.sort = self.sort.next(),
                    KeyCode::Char('S') => self.reverse = !self.reverse,
                    _ => unreachable!(),
                }
                self.reorder(preserve);
            }
            _ => {}
        }
        Action::None
    }

    /// `C`: sync Steam Cloud saves of the selected installed game (download,
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
                "Already installed; use u to update or Enter to launch"
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
        model.key(key('S'));
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
        model.key(key('S'));
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
    fn pinned_games_sort_first_and_settings_round_trip() {
        let mut model = loaded();
        model.key(key('j'));
        model.key(key('j'));
        model.key(key('f'));
        assert_eq!(model.selected_game().unwrap().app_id, 30);
        assert_eq!(model.games[model.order[0]].app_id, 30);
        model.key(key('S'));
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
    fn arrows_and_friend_pages_ignore_focus_and_clamp() {
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
        assert_eq!(model.selected, 1);
        model.focus = Focus::Library;
        for _ in 0..8 {
            model.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        }
        assert_eq!(model.friend_offset, 5);
        assert_eq!(model.selected, 1);
        for _ in 0..8 {
            model.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        }
        assert_eq!(model.friend_offset, 0);
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
        model.key(key('o'));
        model.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
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
            model.key(key('L')),
            Action::LaunchInstalled(LaunchBackend::BuiltIn)
        );
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Action::LaunchInstalled(LaunchBackend::BuiltIn)
        );
        assert_eq!(model.key(key('P')), Action::ContentProperties);
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
        for c in ['q', 'J', 'P', 'r'] {
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
            "qJPr"
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
    fn feed_is_newest_first_deduplicated_and_capped() {
        let mut model = Model::new(false, None);
        model.notification_event(note(1, "a"));
        model.notification_event(note(2, "b"));
        assert_eq!(titles(&model), ["b", "a"]);
        model.notification_event(note(1, "a2"));
        assert_eq!(titles(&model), ["a2", "b"]);
        // Id 0 means unknown and is never merged.
        model.notification_event(note(0, "z"));
        model.notification_event(note(0, "z"));
        assert_eq!(model.notifications.len(), 4);
        for id in 10..300 {
            model.notification_event(note(id, "n"));
        }
        assert_eq!(model.notifications.len(), MAX_NOTIFICATIONS);
        assert!(model.notif_unseen <= MAX_NOTIFICATIONS);
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
    fn tab_cycles_panes_and_focusing_the_feed_clears_unseen() {
        let mut model = Model::new(false, None);
        model.notification_event(note(1, "a"));
        assert_eq!(model.notif_unseen, 1);
        model.key(code(KeyCode::Tab));
        assert_eq!(model.focus, Focus::Friends);
        model.key(code(KeyCode::Tab));
        assert_eq!(model.focus, Focus::Notifications);
        assert_eq!(model.notif_unseen, 0);
        // Items arriving while focused are already in view.
        model.notification_event(note(2, "b"));
        assert_eq!(model.notif_unseen, 0);
        model.key(code(KeyCode::Tab));
        assert_eq!(model.focus, Focus::Library);
        model.key(code(KeyCode::BackTab));
        assert_eq!(model.focus, Focus::Notifications);
        model.key(code(KeyCode::BackTab));
        assert_eq!(model.focus, Focus::Friends);
    }

    #[test]
    fn page_keys_scroll_the_focused_feed_and_friends_otherwise() {
        let mut model = Model::new(false, None);
        model.notif_rows = 3;
        model.friend_rows = 3;
        for id in 1..=10 {
            model.notification_event(note(id, "n"));
        }
        model.key(code(KeyCode::PageDown));
        assert_eq!(
            model.notif_offset, 0,
            "friends handle PgDn unless the feed is focused"
        );
        model.focus = Focus::Notifications;
        model.key(code(KeyCode::PageDown));
        assert_eq!(model.notif_offset, 3);
        for _ in 0..10 {
            model.key(code(KeyCode::PageDown));
        }
        assert_eq!(model.notif_offset, 7);
        model.key(key('k'));
        assert_eq!(model.notif_offset, 6);
        model.key(code(KeyCode::PageUp));
        model.key(code(KeyCode::PageUp));
        assert_eq!(model.notif_offset, 0);
        // A new item scrolls the focused feed back to the top.
        model.notif_offset = 4;
        model.notification_event(note(99, "fresh"));
        assert_eq!(model.notif_offset, 0);
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

    fn shift_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT)
    }

    #[test]
    fn shift_c_starts_a_cloud_sync_of_the_selected_installed_game() {
        let mut model = model();
        assert_eq!(model.key(shift_c()), Action::CloudSync);
        assert!(model.cloud_busy.contains(&10));
        assert!(model.status.starts_with("Steam Cloud: syncing"));
        // Not twice at once, and launching waits for the sync.
        assert_eq!(model.key(shift_c()), Action::None);
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
        assert_eq!(model.key(shift_c()), Action::None);
        assert!(model.status.contains("needs an installed game"));
        model.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        model.launching.insert(10);
        assert_eq!(model.key(shift_c()), Action::None);
        model.launching.clear();
        model.install_active = true;
        model.active_install = Some((10, "/lib".into()));
        assert_eq!(model.key(shift_c()), Action::None);
        assert!(model.status.contains("install/update"));
        assert!(model.cloud_busy.is_empty());
        assert_eq!(Model::new(true, None).key(shift_c()), Action::None);
    }

    #[test]
    fn capital_c_is_filter_text_while_typing() {
        let mut model = model();
        model.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(model.searching);
        assert_eq!(model.key(shift_c()), Action::None);
        assert_eq!(model.search, "C");
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
