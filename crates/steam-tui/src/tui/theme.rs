//! Palette and small pure classifiers shared by the renderer.
//!
//! Named ANSI colors and modifiers let the terminal theme decide
//! the exact shades: "dim" is the default foreground plus `DIM`, never a fixed
//! gray that can vanish into a background.

use ratatui::style::{Color, Modifier, Style};
use steam_core::catalog::{CatalogItem, InstallPlatform, UpdateStatus};
use steam_core::friends::Presence;
use steam_core::install::InstallPhase;

/// Explicit orange marks games that require Valve's host, installed or not.
pub(super) const VALVE_HOST_COLOR: Color = Color::Rgb(255, 165, 0);

pub(super) const PROTON_NOTE: &str =
    "Windows build · runs via Proton; needs Proton + prefix for direct launch";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Severity {
    Error,
    Warning,
    Success,
    Progress,
    Info,
}

/// Classifies a free-form status message. Error outranks warning, which
/// outranks success, which outranks progress ("finished; refreshing" is success).
pub(super) fn classify(message: &str) -> Severity {
    let text = message.to_lowercase();
    let has = |tokens: &[&str]| tokens.iter().any(|token| text.contains(token));
    if has(&[
        "failed",
        "error",
        "cannot",
        "unable",
        "denied",
        "disconnected",
        "rejected",
        "invalid",
    ]) {
        Severity::Error
    } else if has(&[
        "cancel",
        "already",
        "required",
        "wait",
        "unavailable",
        "select an",
        "no game",
        "warnings",
        "below",
    ]) {
        Severity::Warning
    } else if has(&[
        "finished",
        "launched",
        "refreshed",
        "up to date",
        "committed",
        "success",
    ]) {
        Severity::Success
    } else if has(&[
        "…",
        "refreshing",
        "loading",
        "scanning",
        "resolving",
        "preparing",
    ]) {
        Severity::Progress
    } else {
        Severity::Info
    }
}

pub(super) fn severity_style(severity: Severity) -> Style {
    match severity {
        Severity::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Severity::Warning => Style::default().fg(Color::Yellow),
        Severity::Success => Style::default().fg(Color::Green),
        Severity::Progress => Style::default().fg(Color::Cyan),
        Severity::Info => Style::default(),
    }
}

pub(super) fn message_style(message: &str) -> Style {
    severity_style(classify(message))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlatformMarker {
    /// Windows-only content run through Proton.
    Proton,
    Native,
    Unknown,
}

/// Installed platform wins; otherwise the resolved install plan's platform.
pub(super) fn game_platform(game: &CatalogItem) -> Option<InstallPlatform> {
    game.installed
        .as_ref()
        .and_then(|installed| installed.platform)
        .or_else(|| game.plan.as_ref().map(|plan| plan.platform))
}

pub(super) fn marker(game: &CatalogItem) -> PlatformMarker {
    PlatformMarker::of(game_platform(game))
}

impl PlatformMarker {
    pub fn of(platform: Option<InstallPlatform>) -> Self {
        match platform {
            Some(InstallPlatform::Windows) => Self::Proton,
            Some(InstallPlatform::Linux) => Self::Native,
            None => Self::Unknown,
        }
    }

    /// Exactly three cells: width-1 symbols (squared plus = Windows build via Proton, lambda = native).
    pub fn badge(self) -> &'static str {
        match self {
            Self::Proton => " ⊞ ",
            Self::Native => " λ ",
            Self::Unknown => "   ",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Proton => "Windows · Proton",
            Self::Native => "Linux native",
            Self::Unknown => "unknown",
        }
    }

    pub fn style(self) -> Style {
        match self {
            Self::Proton => Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            Self::Native => Style::default().fg(Color::Green),
            Self::Unknown => dim(),
        }
    }
}

pub(super) fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub(super) fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub(super) fn accent() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn key_cap() -> Style {
    accent()
}

pub(super) fn heading() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// Selected row: a quiet bar behind the row's own colors, as in btop.
pub(super) fn highlight() -> Style {
    Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

/// Panels with their own btop-style border color.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Panel {
    Library,
    Friends,
    Notifications,
    Overlay,
}

/// Each panel owns a hue; the focused one switches to the bright variant and bold.
pub(super) fn panel_border(panel: Panel, focused: bool) -> Style {
    let (base, bright) = match panel {
        Panel::Library => (Color::Blue, Color::LightBlue),
        Panel::Friends => (Color::Green, Color::LightGreen),
        Panel::Notifications => (Color::Magenta, Color::LightMagenta),
        Panel::Overlay => (Color::Cyan, Color::LightCyan),
    };
    if focused {
        Style::default().fg(bright).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(base)
    }
}

/// Install bar/border color by phase; cancelling, pausing or cancelled is a caution.
pub(super) fn phase_color(phase: InstallPhase, caution: bool) -> Color {
    if caution || phase == InstallPhase::Cancelled {
        Color::Yellow
    } else {
        match phase {
            InstallPhase::Finished => Color::Green,
            InstallPhase::Verifying => Color::Magenta,
            InstallPhase::Committing => Color::Blue,
            InstallPhase::Preparing | InstallPhase::Downloading | InstallPhase::Cancelled => {
                Color::Cyan
            }
        }
    }
}

pub(super) fn size_style() -> Style {
    Style::default().fg(Color::Cyan)
}

pub(super) fn playtime_style() -> Style {
    Style::default().fg(Color::LightBlue)
}

/// Short state word for the library's State column. Games with an active install
/// show live progress instead (`render::install_state`).
pub(super) fn state_tag(game: &CatalogItem) -> (&'static str, Style) {
    if game.installed.is_none() {
        return if game.unavailable_reason.is_some() {
            ("blocked", Style::default().fg(Color::Red))
        } else {
            ("new", dim())
        };
    }
    match &game.update {
        UpdateStatus::Available { .. } => (
            "update",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        UpdateStatus::UpToDate => ("installed", Style::default().fg(Color::Green)),
        UpdateStatus::Unknown => ("installed", dim()),
        UpdateStatus::Unavailable(_) => ("blocked", Style::default().fg(Color::Red)),
    }
}

pub(super) fn presence_style(presence: Presence) -> Style {
    match presence {
        Presence::Online | Presence::LookingToTrade | Presence::LookingToPlay => {
            Style::default().fg(Color::Green)
        }
        Presence::Away | Presence::Snooze => Style::default().fg(Color::Yellow),
        Presence::Busy => Style::default().fg(Color::Red),
        Presence::Offline | Presence::Unknown => dim(),
    }
}

pub(super) fn playing_style() -> Style {
    Style::default().fg(Color::Cyan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use steam_core::catalog::InstallPlan;
    use steam_core::installed::InstalledGame;

    #[test]
    fn severity_classification() {
        use Severity::*;
        for (message, expected) in [
            ("Launch [1] failed: no such file", Error),
            ("Catalog refresh failed: timeout", Error),
            ("Cannot install: no plan", Error),
            ("Install [4] failed: cancelled by user", Error),
            (
                "Cancellation requested; waiting for a safe boundary…",
                Warning,
            ),
            (
                "An online account connection is required to install",
                Warning,
            ),
            (
                "Installation unavailable: Depot 1 needs a prerequisite",
                Warning,
            ),
            ("Library refreshed: 40 games; 2 scan warnings", Warning),
            ("Library refreshed: 40 games", Success),
            ("Install [4] finished; refreshing library", Success),
            ("Launched [4] PID 9: proton run", Success),
            ("This game is up to date", Success),
            ("Install [4]: preparing transaction…", Progress),
            ("3 installed games · refreshing owned library…", Progress),
            ("No selected game", Info),
        ] {
            assert_eq!(classify(message), expected, "{message}");
        }
    }

    fn item(installed: Option<InstallPlatform>, plan: Option<InstallPlatform>) -> CatalogItem {
        CatalogItem {
            app_id: 1,
            name: "Game".into(),
            installed: installed.map(|platform| InstalledGame {
                app_id: 1,
                name: "Game".into(),
                size_bytes: None,
                last_played: None,
                playtime_minutes: None,
                install_dir: Default::default(),
                library_root: Default::default(),
                steam_root: Default::default(),
                launch_options: None,
                build_id: None,
                branch: "public".into(),
                language: "english".into(),
                platform: Some(platform),
                installed_depots: Default::default(),
            }),
            plan: plan.map(|platform| InstallPlan {
                app_id: 1,
                name: "Game".into(),
                install_dir_name: "Game".into(),
                build_id: 1,
                platform,
                branch: "public".into(),
                language: "english".into(),
                shared_depots: Default::default(),
                dlc_depots: Default::default(),
                depots: vec![],
                download_bytes: 0,
                installed_bytes: 0,
            }),
            update: UpdateStatus::Unknown,
            last_played: None,
            playtime_minutes: None,
            size_bytes: None,
            unavailable_reason: None,
        }
    }

    #[test]
    fn marker_selection_prefers_installed_platform_then_plan() {
        use InstallPlatform::{Linux, Windows};
        assert_eq!(marker(&item(Some(Windows), None)), PlatformMarker::Proton);
        assert_eq!(marker(&item(None, Some(Windows))), PlatformMarker::Proton);
        assert_eq!(
            marker(&item(Some(Linux), Some(Windows))),
            PlatformMarker::Native
        );
        assert_eq!(marker(&item(None, Some(Linux))), PlatformMarker::Native);
        assert_eq!(marker(&item(None, None)), PlatformMarker::Unknown);
        for marker in [
            PlatformMarker::Proton,
            PlatformMarker::Native,
            PlatformMarker::Unknown,
        ] {
            assert_eq!(unicode_width::UnicodeWidthStr::width(marker.badge()), 3);
        }
    }
}
