//! The key bindings of the main screen and the navigation rules every overlay
//! shares. Dispatch (`Model::key`), the footer hints and the help overlay all read
//! this table, so a key can no longer drift between what it does and what the
//! screen says it does.
//!
//! Rules the table follows:
//! * no case variants with different meanings (`s`/`S`, `c`/`C`, ...);
//! * keys act on the focused pane; only the commands that belong to one pane
//!   (`o` offline friends, `x` clear feed) are pane-scoped;
//! * overlays close with `Esc`, the key that opened them, or `q`.

use super::model::Focus;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One physical key. Letters ignore Shift (an upper-case letter arrives as its
/// own char), but never match with Ctrl or Alt held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Key {
    Char(char),
    Code(KeyCode),
    Ctrl(KeyCode),
}

impl Key {
    pub(super) fn matches(self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match self {
            Self::Char(ch) => key.code == KeyCode::Char(ch) && !ctrl && !alt,
            Self::Code(code) => key.code == code && !ctrl && !alt,
            Self::Ctrl(code) => key.code == code && ctrl,
        }
    }

    /// How the key is written on screen: `Enter`, `↑`, `Ctrl+Enter`, `u`.
    pub(super) fn label(self) -> String {
        match self {
            Self::Char(ch) => ch.to_string(),
            Self::Code(code) => code_label(code),
            Self::Ctrl(code) => format!("Ctrl+{}", code_label(code)),
        }
    }
}

fn code_label(code: KeyCode) -> String {
    match code {
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "Shift+Tab".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::PageUp => "PgUp".into(),
        KeyCode::PageDown => "PgDn".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Char(ch) => ch.to_string(),
        other => format!("{other:?}"),
    }
}

/// What a main-screen key asks for.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Cmd {
    // Selected game.
    Activate,
    ValveHost,
    LaunchInstalled,
    Update,
    Pause,
    Cancel,
    Manage,
    Downloads,
    LaunchOption,
    Shaders,
    SyncSaves,
    News,
    Achievements,
    Pin,
    // Library view.
    Filter,
    ClearFilter,
    View,
    SortName,
    SortSize,
    SortLast,
    SortPlayed,
    CycleSort,
    // Panes.
    NextPane,
    PrevPane,
    ToggleOffline,
    ClearFeed,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    // Application.
    Refresh,
    Backend,
    Help,
    Quit,
}

impl Cmd {
    /// Movement keys: they keep working while a filter is being typed.
    pub(super) fn is_navigation(self) -> bool {
        matches!(
            self,
            Self::Up | Self::Down | Self::PageUp | Self::PageDown | Self::Home | Self::End
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Section {
    Library,
    Panes,
    Application,
}

/// Which focused pane a binding applies to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pane {
    Any,
    Only(Focus),
}

pub(super) struct Binding {
    pub cmd: Cmd,
    pub keys: &'static [Key],
    pane: Pane,
    pub section: Section,
    /// One help-screen line; the keys are prepended.
    pub help: &'static str,
}

const fn bind(
    cmd: Cmd,
    keys: &'static [Key],
    pane: Pane,
    section: Section,
    help: &'static str,
) -> Binding {
    Binding {
        cmd,
        keys,
        pane,
        section,
        help,
    }
}

use Key::{Char, Code, Ctrl};
use Pane::{Any, Only};
use Section::{Application, Library, Panes};

pub(super) static BINDINGS: &[Binding] = &[
    bind(
        Cmd::Activate,
        &[Code(KeyCode::Enter)],
        Any,
        Library,
        "Open chat with the selected friend; in the library launch the selected game (VAC games use Valve's host) or install it. Double-click does the same",
    ),
    bind(
        Cmd::ValveHost,
        &[Ctrl(KeyCode::Enter)],
        Any,
        Library,
        "Launch on Valve's host (needs a terminal with the enhanced keyboard protocol)",
    ),
    bind(
        Cmd::LaunchInstalled,
        &[Char('l')],
        Any,
        Library,
        "Launch the installed version without checking for an update first",
    ),
    bind(
        Cmd::Update,
        &[Char('u')],
        Any,
        Library,
        "Update the selected installed game",
    ),
    bind(
        Cmd::Pause,
        &[Char('p')],
        Any,
        Library,
        "Pause / resume the download; the job stays alive, no new destination step",
    ),
    bind(
        Cmd::Cancel,
        &[Char('c')],
        Any,
        Library,
        "Cancel the install or update safely (also while paused); while the game is launching, skip its optional shader processing",
    ),
    bind(
        Cmd::Manage,
        &[Char('m')],
        Any,
        Library,
        "Manage content of the selected game: branch, language, DLC",
    ),
    bind(
        Cmd::Downloads,
        &[Char('d')],
        Any,
        Library,
        "Downloads: the persistent job queue (p pause, r retry, c cancel, +/- priority, a policy)",
    ),
    bind(
        Cmd::LaunchOption,
        &[Char('o')],
        Only(Focus::Library),
        Library,
        "Cycle the game's launch option (e.g. DirectX 11 / 12); the choice is remembered and shown in the status bar",
    ),
    bind(
        Cmd::Shaders,
        &[Char('z')],
        Any,
        Library,
        "Download a Vulkan shader pre-cache for the game (off unless requested; refreshed hourly afterwards, processed before launch)",
    ),
    bind(
        Cmd::SyncSaves,
        &[Char('y')],
        Any,
        Library,
        "Sync Steam Cloud saves of the selected installed game: download newer saves, then upload local changes (blocked while it runs)",
    ),
    bind(
        Cmd::News,
        &[Char('n')],
        Any,
        Library,
        "Game changelog / developer updates (Esc closes). Right-click does the same",
    ),
    bind(
        Cmd::Achievements,
        &[Char('a')],
        Any,
        Library,
        "Achievements: unlocked state and date, rarity; 1-4 / s sort (status, name, rarity, date; the same key again reverses), f filter all / unlocked / locked, r reload",
    ),
    bind(
        Cmd::Pin,
        &[Char('f')],
        Any,
        Library,
        "Pin / unpin the selected game at the top (★)",
    ),
    bind(
        Cmd::Filter,
        &[Char('/')],
        Any,
        Library,
        "Filter library names; type to filter, Enter finishes",
    ),
    bind(
        Cmd::ClearFilter,
        &[Code(KeyCode::Esc)],
        Any,
        Library,
        "Clear the filter",
    ),
    bind(
        Cmd::View,
        &[Char('v')],
        Any,
        Library,
        "Cycle All / Installed / Not installed",
    ),
    bind(
        Cmd::SortName,
        &[Char('1')],
        Any,
        Library,
        "Sort by Name¹; pressing the active sort again reverses it",
    ),
    bind(Cmd::SortSize, &[Char('2')], Any, Library, "Sort by Size²"),
    bind(
        Cmd::SortLast,
        &[Char('3')],
        Any,
        Library,
        "Sort by Last played³",
    ),
    bind(
        Cmd::SortPlayed,
        &[Char('4')],
        Any,
        Library,
        "Sort by Play time⁴",
    ),
    bind(
        Cmd::CycleSort,
        &[Char('s')],
        Any,
        Library,
        "Cycle the sort column (unknowns last; shown as sort: in the title)",
    ),
    bind(
        Cmd::NextPane,
        &[Code(KeyCode::Tab)],
        Any,
        Panes,
        "Focus the next pane: Library → Friends → Notifications",
    ),
    bind(
        Cmd::PrevPane,
        &[Code(KeyCode::BackTab)],
        Any,
        Panes,
        "Focus the previous pane",
    ),
    bind(
        Cmd::ToggleOffline,
        &[Char('o')],
        Only(Focus::Friends),
        Panes,
        "Friends: show / hide offline friends",
    ),
    bind(
        Cmd::ClearFeed,
        &[Char('x')],
        Only(Focus::Notifications),
        Panes,
        "Notifications: clear the feed (the count badges are Steam's and stay)",
    ),
    bind(
        Cmd::Up,
        &[Code(KeyCode::Up), Char('k')],
        Any,
        Panes,
        "Move up in the focused pane",
    ),
    bind(
        Cmd::Down,
        &[Code(KeyCode::Down), Char('j')],
        Any,
        Panes,
        "Move down in the focused pane",
    ),
    bind(
        Cmd::PageUp,
        &[Code(KeyCode::PageUp)],
        Any,
        Panes,
        "Page up in the focused pane",
    ),
    bind(
        Cmd::PageDown,
        &[Code(KeyCode::PageDown)],
        Any,
        Panes,
        "Page down in the focused pane",
    ),
    bind(
        Cmd::Home,
        &[Code(KeyCode::Home)],
        Any,
        Panes,
        "First entry of the focused pane",
    ),
    bind(
        Cmd::End,
        &[Code(KeyCode::End)],
        Any,
        Panes,
        "Last entry of the focused pane",
    ),
    bind(
        Cmd::Refresh,
        &[Char('r')],
        Any,
        Application,
        "Refresh the library, update metadata and friend presence",
    ),
    bind(
        Cmd::Backend,
        &[Char('b')],
        Any,
        Application,
        "Backend traffic: exactly what the built-in Steam backend sends to / receives from Steam (message ids, names, sizes, hex head, protobuf fields); x clears",
    ),
    bind(
        Cmd::Help,
        &[Char('?'), Code(KeyCode::F(1))],
        Any,
        Application,
        "Toggle this help",
    ),
    bind(
        Cmd::Quit,
        &[Char('q')],
        Any,
        Application,
        "Quit; waits for the installer to stop safely (q is text while typing a filter or path, and closes an overlay)",
    ),
];

/// The command a main-screen key asks for while `focus` has the keyboard.
pub(super) fn lookup(focus: Focus, key: &KeyEvent) -> Option<Cmd> {
    BINDINGS
        .iter()
        .filter(|binding| match binding.pane {
            Pane::Any => true,
            Pane::Only(only) => only == focus,
        })
        .find(|binding| binding.keys.iter().any(|spec| spec.matches(key)))
        .map(|binding| binding.cmd)
}

/// The first key of `cmd` as written on screen. Panics for a command without a
/// binding, which `every_command_has_a_binding` rules out.
pub(super) fn label(cmd: Cmd) -> String {
    BINDINGS
        .iter()
        .find(|binding| binding.cmd == cmd)
        .and_then(|binding| binding.keys.first())
        .map(|key| key.label())
        .unwrap_or_else(|| panic!("{cmd:?} has no key binding"))
}

/// Movement inside any list-like view. Every overlay and pane uses the same keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Nav {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

/// Rows a PgUp / PgDn moves in an overlay.
pub(super) const PAGE: usize = 10;

pub(super) fn nav(key: &KeyEvent) -> Option<Nav> {
    let cmd = BINDINGS
        .iter()
        .filter(|binding| binding.cmd.is_navigation())
        .find(|binding| binding.keys.iter().any(|spec| spec.matches(key)))?
        .cmd;
    Some(match cmd {
        Cmd::Up => Nav::Up,
        Cmd::Down => Nav::Down,
        Cmd::PageUp => Nav::PageUp,
        Cmd::PageDown => Nav::PageDown,
        Cmd::Home => Nav::Home,
        _ => Nav::End,
    })
}

/// `Esc`, `q` or the key that opened the overlay closes it.
pub(super) fn closes(key: &KeyEvent, opener: &[Key]) -> bool {
    key.code == KeyCode::Esc
        || Key::Char('q').matches(key)
        || opener.iter().any(|spec| spec.matches(key))
}

/// Section lines of the help overlay: keys, then the description. Several keys
/// of one command are joined with ` / `.
fn rows(section: Section) -> String {
    let mut text = String::new();
    for binding in BINDINGS.iter().filter(|b| b.section == section) {
        let keys: Vec<String> = binding.keys.iter().map(|key| key.label()).collect();
        text.push_str(&format!("{:<14}{}\n", keys.join(" / "), binding.help));
    }
    text
}

/// The whole help overlay, built from the binding table plus the explanations
/// that belong to no single key.
pub(super) fn help_text() -> String {
    let mut text = String::from("LIBRARY\n");
    text.push_str(&rows(Library));
    text.push_str("LMB / wheel   Select the clicked game (double-click launches); the wheel moves the selection\n");
    text.push_str(
        "              Sort, filter, pins and the offline-friends toggle are remembered\n\
         \x20             Launching always downloads cloud saves first (never uploads); conflicts keep the local file, backups go to ~/.local/state/steam-tui/cloud/backup\n\
         \x20             Conflict picker: l local / r cloud; Enter confirms this file; Esc keeps both\n\
         INSTALL DESTINATION\n\
         ↑ ↓ / j k     Select library (available space shown)\n\
         n             Enter custom library path (new installs only)\n\
         Enter         Confirm destination / check custom path\n\
         Esc / q       Cancel modal / stop editing custom path\n\
         INSTALL PROGRESS\n\
         \x20             Install panel: phase (Prepare, Download, Verify, Install, Done), bar,\n\
         \x20             verified and downloaded bytes, speed, ETA and elapsed time (paused time excluded)\n\
         \x20             The game's State column shows download / verify / install and the percent, or paused and the percent\n\
         \x20             One install/update job at a time; no Steam required\n\
         PANES\n",
    );
    text.push_str(&rows(Panes));
    text.push_str(
        "              Notifications: new items appear on top, ● marks those not yet seen\n\
         \x20             Badges show pending trade offers, messages, items, gifts, invites\n\
         \x20             Only pushes received while running are listed; history cannot be fetched\n\
         APPLICATION\n",
    );
    text.push_str(&rows(Application));
    text.push_str(
        "OVERLAYS\n\
         Esc / q       Close any overlay or modal (also the key that opened it)\n\
         ↑ ↓ / j k     Move; PgUp / PgDn jump 10; Home / End go to the ends\n\
         MARKERS\n\
         Orange names  Automatic Valve host: TF2, Dota 2, CS2, Deadlock\n\
         ⊞             Proton: Windows build run through Proton (needs Proton + prefix for direct launch)\n\
         λ             Native Linux build\n\
         (blank)       Platform not known yet\n\
         ↻             Library title: live owned library is still refreshing (rows are local or cached)\n\
         ☁             Steam Cloud column (installed games with cloud saves): dim ☁ never synced · green ☁ synced · cyan ↑ local saves not uploaded (press y) · yellow ◐◓◑◒ syncing · red ✗ last sync failed or had conflicts\n\
         State         new (not installed) · installed · update · blocked (reason in the status bar)\n\
         NOTES\n\
         Dates are UTC (feed times are local); ? means metadata is unavailable.\n\
         Blocked games show their reason in the status bar when selected.\n\
         Update status: update available, installed (current or unknown), or blocked.\n\
         Direct launch cannot bypass DRM or guarantee Steam-free compatibility.\n\
         Updates stay in their existing library; close Steam before installing.\n\
         Only depot-verified Windows install scripts run, inside the game's Proton prefix; unsupported setup directives block launch.\n\
         Friends login uses saved credentials; run `steam-tui login` to sign in.",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn every_command_has_a_binding_and_help_text() {
        use std::collections::HashSet;
        let bound: HashSet<_> = BINDINGS.iter().map(|b| b.cmd).collect();
        assert_eq!(bound.len(), BINDINGS.len(), "a command is bound twice");
        for binding in BINDINGS {
            assert!(!binding.keys.is_empty(), "{:?}", binding.cmd);
            assert!(!binding.help.is_empty(), "{:?}", binding.cmd);
            assert!(!label(binding.cmd).is_empty());
        }
    }

    #[test]
    fn no_key_means_two_things_in_the_same_pane() {
        for focus in [Focus::Library, Focus::Friends, Focus::Notifications] {
            let mut seen: Vec<(Key, Cmd)> = Vec::new();
            for binding in BINDINGS {
                let applies = match binding.pane {
                    Pane::Any => true,
                    Pane::Only(only) => only == focus,
                };
                if !applies {
                    continue;
                }
                for &key in binding.keys {
                    if let Some((_, other)) = seen.iter().find(|(k, _)| *k == key) {
                        panic!(
                            "{key:?} is both {other:?} and {:?} in {focus:?}",
                            binding.cmd
                        );
                    }
                    seen.push((key, binding.cmd));
                }
            }
        }
    }

    #[test]
    fn no_letter_has_an_upper_case_twin_with_another_meaning() {
        for binding in BINDINGS {
            for key in binding.keys {
                if let Key::Char(ch) = key {
                    assert!(
                        !ch.is_uppercase(),
                        "{:?} uses upper-case {ch}; keep one meaning per letter",
                        binding.cmd
                    );
                }
            }
        }
    }

    #[test]
    fn pane_scoped_keys_follow_the_focus() {
        let o = press(KeyCode::Char('o'));
        assert_eq!(lookup(Focus::Library, &o), Some(Cmd::LaunchOption));
        assert_eq!(lookup(Focus::Friends, &o), Some(Cmd::ToggleOffline));
        assert_eq!(lookup(Focus::Notifications, &o), None);
        let x = press(KeyCode::Char('x'));
        assert_eq!(lookup(Focus::Library, &x), None);
        assert_eq!(lookup(Focus::Notifications, &x), Some(Cmd::ClearFeed));
    }

    #[test]
    fn letters_ignore_shift_but_not_control_and_enter_variants_differ() {
        let shifted = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::SHIFT);
        assert_eq!(lookup(Focus::Library, &shifted), Some(Cmd::LaunchInstalled));
        let ctrl = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(lookup(Focus::Library, &ctrl), None);
        assert_eq!(
            lookup(Focus::Library, &press(KeyCode::Enter)),
            Some(Cmd::Activate)
        );
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(lookup(Focus::Library, &ctrl_enter), Some(Cmd::ValveHost));
    }

    #[test]
    fn navigation_and_closing_are_shared_by_every_overlay() {
        for (code, expected) in [
            (KeyCode::Up, Nav::Up),
            (KeyCode::Char('k'), Nav::Up),
            (KeyCode::Down, Nav::Down),
            (KeyCode::Char('j'), Nav::Down),
            (KeyCode::PageUp, Nav::PageUp),
            (KeyCode::PageDown, Nav::PageDown),
            (KeyCode::Home, Nav::Home),
            (KeyCode::End, Nav::End),
        ] {
            assert_eq!(nav(&press(code)), Some(expected), "{code:?}");
        }
        assert_eq!(nav(&press(KeyCode::Char('x'))), None);
        let opener = [Key::Char('n')];
        assert!(closes(&press(KeyCode::Esc), &opener));
        assert!(closes(&press(KeyCode::Char('q')), &opener));
        assert!(closes(&press(KeyCode::Char('n')), &opener));
        assert!(!closes(&press(KeyCode::Char('x')), &opener));
    }

    #[test]
    fn labels_are_written_the_way_the_footers_show_them() {
        assert_eq!(label(Cmd::Activate), "Enter");
        assert_eq!(label(Cmd::ValveHost), "Ctrl+Enter");
        assert_eq!(label(Cmd::Update), "u");
        assert_eq!(label(Cmd::Up), "↑");
        assert_eq!(label(Cmd::PageDown), "PgDn");
    }

    #[test]
    fn the_help_text_lists_every_binding_and_none_of_the_removed_keys() {
        let text = help_text();
        for binding in BINDINGS {
            assert!(
                text.contains(binding.help),
                "{:?} is not in the help",
                binding.cmd
            );
        }
        for removed in [
            "Shift+Enter / L",
            "i / u",
            "Content properties",
            "Skip selected",
        ] {
            assert!(!text.contains(removed), "{removed}");
        }
    }
}
