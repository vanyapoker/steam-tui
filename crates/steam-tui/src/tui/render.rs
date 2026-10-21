use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table, TableState, Wrap,
};
use steam_core::catalog::{CatalogItem, InstallPlatform, UpdateStatus};
use steam_core::install::{InstallPhase, InstallProgress};
use steam_core::notifications::{CountKind, Notification};
use unicode_width::UnicodeWidthStr;

use super::keys::{self, Cmd};
use super::model::{CloudIcon, Focus, Model, Sort};
use super::theme::{
    self, PROTON_NOTE, Panel, PlatformMarker, Severity, accent, bold, dim, heading, highlight,
    key_cap, message_style, severity_style,
};

/// One styled run of text; lines are built from these and truncated by display width.
type Seg = (String, Style);

fn safe(value: &str) -> String {
    super::super::terminal_text(value).into_owned()
}

fn fit(value: &str, width: usize) -> String {
    let value = safe(value);
    if UnicodeWidthStr::width(value.as_str()) <= width {
        return value;
    }
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let next = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + next > width.saturating_sub(1) {
            break;
        }
        output.push(ch);
        used += next;
    }
    output.push('…');
    output
}

fn seg(text: impl Into<String>, style: Style) -> Seg {
    (text.into(), style)
}

fn sep() -> Seg {
    seg(" · ", dim())
}

fn spans(segments: Vec<Seg>) -> Vec<Span<'static>> {
    segments
        .into_iter()
        .map(|(text, style)| Span::styled(text, style))
        .collect()
}

/// Joins segments, truncating the first one that overflows with `…` and dropping the rest.
fn fit_line(segments: Vec<Seg>, width: usize) -> Line<'static> {
    let mut output = Vec::new();
    let mut used = 0;
    for (text, style) in segments {
        let text = safe(&text);
        let text_width = UnicodeWidthStr::width(text.as_str());
        if used + text_width <= width {
            used += text_width;
            output.push(Span::styled(text, style));
            continue;
        }
        let remaining = width - used;
        if remaining > 0 {
            output.push(Span::styled(fit(&text, remaining), style));
        }
        break;
    }
    Line::from(output)
}

/// Key caps in bold cyan, descriptions dim; whole pairs are dropped rather than cut.
/// The first pair and the last `keep_tail` pairs (help and quit) are dropped
/// last: the pairs between them go first, from the right.
fn hotkey_line(pairs: &[(String, String)], keep_tail: usize, width: usize) -> Line<'static> {
    let cost = |index: usize, (key, description): &(String, String)| {
        let gap = if index == 0 { 0 } else { 2 };
        gap + UnicodeWidthStr::width(key.as_str())
            + 1
            + UnicodeWidthStr::width(description.as_str())
    };
    let mut kept: Vec<usize> = (0..pairs.len()).collect();
    let total = |kept: &[usize]| -> usize {
        kept.iter()
            .enumerate()
            .map(|(position, &index)| cost(position, &pairs[index]))
            .sum()
    };
    let tail_start = pairs.len().saturating_sub(keep_tail).max(1);
    while total(&kept) > width {
        let middle = kept
            .iter()
            .rposition(|&index| index >= 1 && index < tail_start);
        let drop = middle.or_else(|| kept.len().checked_sub(1));
        match drop {
            Some(position) if !kept.is_empty() => {
                kept.remove(position);
            }
            _ => break,
        }
    }
    let mut output = Vec::new();
    for (position, &index) in kept.iter().enumerate() {
        let (key, description) = &pairs[index];
        if position > 0 {
            output.push(Span::raw("  "));
        }
        output.push(Span::styled(key.clone(), key_cap()));
        output.push(Span::styled(format!(" {description}"), dim()));
    }
    Line::from(output)
}

fn size(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "?".to_owned();
    };
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn last_played(timestamp: Option<u64>) -> String {
    timestamp
        .and_then(|time| i64::try_from(time).ok())
        .and_then(|time| DateTime::<Utc>::from_timestamp(time, 0))
        .map(|time| time.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".to_owned())
}

fn play_time(minutes: Option<u32>) -> String {
    minutes
        .map(|minutes| format!("{}h {:02}m", minutes / 60, minutes % 60))
        .unwrap_or_else(|| "?".to_owned())
}

/// Unknown values are dim so known ones stand out.
fn value_span_seg(text: String) -> Seg {
    seg(text, theme::size_style())
}

fn value_span(text: String, style: Style) -> Span<'static> {
    if text == "?" {
        Span::styled(text, dim())
    } else {
        Span::styled(text, style)
    }
}

fn name_style(game: &CatalogItem) -> Style {
    let style = if game.installed.is_some() {
        bold()
    } else {
        Style::default()
    };
    if steam_core::launch::requires_valve_host(game.app_id) {
        style.fg(theme::VALVE_HOST_COLOR)
    } else {
        style
    }
}

fn error_style() -> Style {
    severity_style(Severity::Error)
}

const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;
/// At this width the right-hand column (friends over notifications) sits beside the library.
const WIDE_WIDTH: u16 = 110;
const INSTALL_HEIGHT: u16 = 6;
const MIN_MAIN_HEIGHT: u16 = 8;
/// Least height for the Friends/Notifications panels; below it they are hidden.
const MIN_PANEL_HEIGHT: u16 = 6;
/// The Library takes this share of the width (wide layout).
const LIBRARY_PERCENT: u16 = 56;
const MIN_LOWER_HEIGHT: u16 = 5;
const MAX_LOWER_HEIGHT: u16 = 8;
/// A stacked lower row narrower than this shows Friends only.
const NOTIFICATIONS_MIN_WIDTH: u16 = 56;
const NAME_MIN: usize = 20;

struct Areas {
    library: Rect,
    friends: Option<Rect>,
    notifications: Option<Rect>,
    install: Option<Rect>,
    status: Rect,
}

/// Narrow layout: the Friends/Notifications row is a quarter of the height,
/// between 5 and 8 rows including borders, and grows (up to half the height) to
/// show everything Friends or Notifications has: `lower_wanted` rows with borders.
fn lower_height(main_height: u16, lower_wanted: u16) -> u16 {
    let base = (main_height / 4).clamp(MIN_LOWER_HEIGHT, MAX_LOWER_HEIGHT);
    base.max(lower_wanted.min(main_height / 2))
}

/// Optional panels are dropped, never squeezed below usefulness; Notifications
/// goes first. Narrow terminals give the library all the height that remains
/// after a small fixed Friends/Notifications row.
fn areas(area: Rect, install_shown: bool, lower_wanted: u16) -> Areas {
    let install_height = if install_shown {
        INSTALL_HEIGHT.min(area.height.saturating_sub(MIN_MAIN_HEIGHT + 1))
    } else {
        0
    };
    let install_height = if install_height < 3 {
        0
    } else {
        install_height
    };
    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(install_height),
        Constraint::Length(1),
    ])
    .split(area);
    let main = rows[0];
    let (library, friends, notifications) = if area.width >= WIDE_WIDTH {
        let columns = Layout::horizontal([
            Constraint::Percentage(LIBRARY_PERCENT),
            Constraint::Percentage(100 - LIBRARY_PERCENT),
        ])
        .split(main);
        if columns[1].height >= 2 * MIN_PANEL_HEIGHT {
            let right = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(columns[1]);
            (columns[0], Some(right[0]), Some(right[1]))
        } else {
            (columns[0], Some(columns[1]), None)
        }
    } else if main.height >= MIN_MAIN_HEIGHT + lower_height(main.height, lower_wanted) {
        // The library takes everything the fixed lower row leaves.
        let split = Layout::vertical([
            Constraint::Min(MIN_MAIN_HEIGHT),
            Constraint::Length(lower_height(main.height, lower_wanted)),
        ])
        .split(main);
        if split[1].width >= NOTIFICATIONS_MIN_WIDTH {
            let lower =
                Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .split(split[1]);
            (split[0], Some(lower[0]), Some(lower[1]))
        } else {
            (split[0], Some(split[1]), None)
        }
    } else {
        (main, None, None)
    };
    Areas {
        library,
        friends,
        notifications,
        install: (install_height > 0).then_some(rows[1]),
        status: rows[2],
    }
}

fn group_width(group: &[Seg]) -> usize {
    2 + group
        .iter()
        .map(|(text, _)| UnicodeWidthStr::width(safe(text).as_str()))
        .sum::<usize>()
}

/// Which border a title or hint line sits on. btop draws each segment as a
/// notch in the line; with rounded corners the notch uses the rounded corner
/// glyphs, opening downward on the top border and upward on the bottom one.
#[derive(Clone, Copy)]
enum Edge {
    Top,
    Bottom,
}

impl Edge {
    /// Glyphs before and after a segment, e.g. `─╮` and `╭` on the top border.
    fn caps(self) -> (&'static str, &'static str) {
        match self {
            Self::Top => ("─╮", "╭"),
            Self::Bottom => ("─╯", "╰"),
        }
    }
}

/// btop-style border title: `─╮group╭─╮group╭─` (`╯ ╰` on the bottom edge). Groups that do
/// not fit are dropped from the end, so list the most important first.
fn title_groups(groups: Vec<Vec<Seg>>, border: Style, width: usize, edge: Edge) -> Line<'static> {
    let (open, close) = edge.caps();
    let mut output = Vec::new();
    let mut used = 0;
    for group in groups {
        let needed = 1 + group_width(&group);
        if used + needed > width {
            break;
        }
        used += needed;
        output.push(Span::styled(open, border));
        output.extend(spans(
            group.into_iter().map(|(t, s)| (safe(&t), s)).collect(),
        ));
        output.push(Span::styled(close, border));
    }
    if used < width && !output.is_empty() {
        output.push(Span::styled("─", border));
    }
    Line::from(output)
}

/// Rounded panel (btop style) whose top border carries `left`/`right` title groups and whose
/// bottom border carries `bottom` key hints.
fn panel_block(
    border: Style,
    width: u16,
    left: Vec<Vec<Seg>>,
    right: Vec<Vec<Seg>>,
    bottom: Vec<Vec<Seg>>,
) -> Block<'static> {
    let inner = width.saturating_sub(2) as usize;
    let right = title_groups(right, border, inner, Edge::Top);
    let left_budget = inner.saturating_sub(right.width());
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title_top(title_groups(left, border, left_budget, Edge::Top))
        .title_top(right.right_aligned())
        .title_bottom(title_groups(bottom, border, inner, Edge::Bottom))
}

/// Centered modal rectangle of at most `width`×`height`.
fn modal_area(screen: Rect, width: u16, height: u16) -> Rect {
    let width = screen.width.min(width);
    let height = screen.height.min(height);
    Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    )
}

/// Dims everything behind a modal (btop's pop-up look), then clears its own area.
fn open_modal(frame: &mut Frame<'_>, area: Rect) {
    let screen = frame.area();
    frame
        .buffer_mut()
        .set_style(screen, Style::new().add_modifier(Modifier::DIM));
    frame.render_widget(Clear, area);
}

/// btop-style key hint. A single-letter key is highlighted inside its word
/// (`install` with the `i` lit); other keys (`↑↓`, `Enter`) lead the description.
/// Hint words use the terminal's own foreground so they stand out from the border color.
fn word() -> Style {
    Style::default().fg(Color::Reset)
}

fn hint(key: &str, description: &str) -> Vec<Seg> {
    let mut letters = key.chars();
    if let (Some(letter), None) = (letters.next(), letters.next())
        && let Some((at, found)) = description
            .char_indices()
            .find(|(_, ch)| ch.eq_ignore_ascii_case(&letter))
    {
        let end = at + found.len_utf8();
        return [
            (&description[..at], word()),
            (&description[at..end], key_cap()),
            (&description[end..], word()),
        ]
        .into_iter()
        .filter(|(text, _)| !text.is_empty())
        .map(|(text, style)| seg(text, style))
        .collect();
    }
    vec![seg(key, key_cap()), seg(format!(" {description}"), word())]
}

fn percent(progress: &InstallProgress) -> u32 {
    (progress_ratio(progress) * 100.0).floor() as u32
}

/// Download phases show verified-and-staged bytes; verification shows the
/// bytes of staged files hashed so far.
fn progress_ratio(progress: &InstallProgress) -> f64 {
    let (done, total) = if progress.phase == InstallPhase::Verifying {
        (progress.verify_bytes_done, progress.verify_bytes_total)
    } else {
        (progress.completed_bytes, progress.total_bytes)
    };
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    }
}

fn phase_label(phase: InstallPhase) -> &'static str {
    match phase {
        InstallPhase::Preparing => "Prepare",
        InstallPhase::Downloading => "Download",
        InstallPhase::Verifying => "Verify",
        InstallPhase::Committing => "Install",
        InstallPhase::Finished => "Done",
        InstallPhase::Cancelled => "Cancelled",
    }
}

/// Phase name for display. While the download is held the phase stays visible
/// with a pause marker in front of it.
fn phase_text(model: &Model, phase: InstallPhase) -> String {
    if model.paused {
        format!("⏸ {}", phase_label(phase))
    } else {
        phase_label(phase).to_owned()
    }
}

/// State cell for the game being installed/updated: the phase word and the same
/// percent as the Install panel (`download 42%`, `verify 97%`, `install 100%`,
/// `paused 42%`). Fits the 13-cell State column.
fn install_state(model: &Model, game: &CatalogItem) -> Option<(String, Style)> {
    let (app_id, _) = model.active_install.as_ref()?;
    if *app_id != game.app_id {
        return None;
    }
    let bold_fg = |color| Style::default().fg(color).add_modifier(Modifier::BOLD);
    if model.cancel_requested {
        return Some(("cancel…".into(), bold_fg(Color::Yellow)));
    }
    let Some(progress) = model.progress.as_ref().filter(|p| p.app_id == *app_id) else {
        return Some(if model.paused {
            ("paused".into(), bold_fg(Color::Yellow))
        } else {
            ("preparing".into(), severity_style(Severity::Progress))
        });
    };
    let percent = percent(progress);
    Some(if model.paused {
        (format!("paused {percent}%"), bold_fg(Color::Yellow))
    } else {
        let word = match progress.phase {
            InstallPhase::Preparing => "prepare",
            InstallPhase::Downloading => "download",
            InstallPhase::Verifying => "verify",
            InstallPhase::Committing => "install",
            InstallPhase::Finished => "done",
            InstallPhase::Cancelled => "cancelled",
        };
        (
            format!("{word} {percent}%"),
            bold_fg(theme::phase_color(progress.phase, false)),
        )
    })
}

fn rate_text(bytes_per_second: Option<f64>) -> String {
    bytes_per_second
        .map(|rate| format!("{}/s", size(Some(rate as u64))))
        .unwrap_or_else(|| "--".to_owned())
}

fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
        3600..=86_399 => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3600),
    }
}

fn eta_text(eta: Option<Duration>) -> String {
    eta.map(duration_text).unwrap_or_else(|| "--".to_owned())
}

/// The install message is a progress note, not a free-form status: it is styled
/// by phase instead of by words it happens to contain ("cancel", "verified").
fn install_message_style(phase: InstallPhase) -> Style {
    match phase {
        InstallPhase::Finished => severity_style(Severity::Success),
        InstallPhase::Cancelled => severity_style(Severity::Warning),
        _ => dim(),
    }
}

/// One-line progress for places without room for the Install panel.
fn progress_summary(model: &Model, now: Instant) -> Option<String> {
    let progress = model.progress.as_ref()?;
    let mut text = format!(
        "[{}] {} {}%",
        progress.app_id,
        phase_text(model, progress.phase),
        percent(progress)
    );
    if progress.phase == InstallPhase::Downloading {
        let remaining = progress
            .total_download_bytes
            .saturating_sub(progress.downloaded_bytes);
        text.push_str(&format!(
            " · {} · ETA {}",
            rate_text(model.speed.speed(now)),
            eta_text(model.speed.eta(now, remaining))
        ));
    } else if progress.phase == InstallPhase::Verifying {
        let remaining = progress
            .verify_bytes_total
            .saturating_sub(progress.verify_bytes_done);
        text.push_str(&format!(
            " · {} · ETA {}",
            rate_text(model.verify_speed.speed(now)),
            eta_text(model.verify_speed.eta(now, remaining))
        ));
    }
    Some(text)
}

fn too_small(frame: &mut Frame<'_>, model: &Model, now: Instant) {
    let text = format!(
        "Steam TUI — terminal too small\nResize to at least {MIN_WIDTH}×{MIN_HEIGHT}\n↑↓ select · / filter · ? help · q quit\n{}{}",
        safe(&model.status),
        progress_summary(model, now)
            .map(|summary| format!("\n{summary}"))
            .unwrap_or_default()
    );
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }),
        frame.area(),
    );
}

pub(super) fn draw(frame: &mut Frame<'_>, model: &mut Model) {
    model.library_hits.clear();
    let area = frame.area();
    let now = Instant::now();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        model.library_icons.visible(&Default::default());
        too_small(frame, model, now);
    } else {
        // Library chrome: two border rows, the filter row and the header row.
        // The pinned/unpinned divider takes one line of the table.
        let divider = usize::from(model.has_divider());
        // Rows (plus borders) Friends or Notifications could use: a line per friend,
        // or the badge line, an optional status line and the feed.
        let feed = 1 + usize::from(!model.notif_status.is_empty()) + model.notifications.len();
        let wanted = (model.friend_order.len().max(feed) + 2).min(u16::MAX as usize) as u16;
        let areas = areas(
            area,
            model.install_active || model.progress.is_some(),
            wanted,
        );
        model.library_rows = (areas.library.height.saturating_sub(4) as usize)
            .saturating_sub(divider)
            .max(1);
        model.friend_rows = areas
            .friends
            .map_or(1, |rect| (rect.height.saturating_sub(2) as usize).max(1));
        // Feed rows: borders, the badge line and an optional status line.
        let status_rows = usize::from(!model.notif_status.is_empty());
        model.notif_rows = areas.notifications.map_or(1, |rect| {
            (rect.height.saturating_sub(3) as usize)
                .saturating_sub(status_rows)
                .max(1)
        });
        model.keep_visible();
        draw_library(frame, model, areas.library);
        if let Some(rect) = areas.friends {
            draw_friends(frame, model, rect);
        }
        if let Some(rect) = areas.notifications {
            draw_notifications(frame, model, rect);
        }
        if let Some(rect) = areas.install {
            draw_install(frame, model, rect, now);
        }
        draw_status(frame, model, areas.status, areas.install.is_none(), now);
    }
    if model.queue_view {
        queue_overlay(frame, model);
    }
    if model.properties.is_some() {
        content_overlay(frame, model);
    }
    if model.backend_view {
        backend_overlay(frame, model);
    }
    if model.help {
        help(frame, model);
    }
    if model.news.is_some() {
        news_overlay(frame, model);
    }
    if model.achievements.is_some() {
        achievements_overlay(frame, model);
    }
    if model.cloud_conflicts.is_some() {
        cloud_conflicts(frame, model);
    }
    if model.modal.is_some() {
        destination_modal(frame, model);
    }
    if model.chat.active.is_some() {
        chat_modal(frame, model);
    }
}

fn draw_status(frame: &mut Frame<'_>, model: &Model, area: Rect, summary: bool, now: Instant) {
    let host_game = model
        .selected_game()
        .is_some_and(|game| steam_core::launch::requires_valve_host(game.app_id));
    let enter = keys::label(Cmd::Activate);
    let mut pairs = vec![if model.focus == Focus::Friends {
        (enter, "chat".to_owned())
    } else if host_game {
        (enter, "launch (Valve host)".to_owned())
    } else {
        (enter, "launch".to_owned())
    }];
    if !host_game && model.focus == Focus::Library {
        pairs.push((keys::label(Cmd::ValveHost), "Valve host".to_owned()));
    }
    pairs.push((keys::label(Cmd::Backend), "backend".to_owned()));
    pairs.push((keys::label(Cmd::Help), "help".to_owned()));
    pairs.push((keys::label(Cmd::Quit), "quit".to_owned()));
    let right = hotkey_line(&pairs, 2, (area.width as usize).saturating_sub(34));
    let right_width = right.width() as u16;
    let columns =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width + 1)]).split(area);
    let mut left = vec![
        seg("◆ ", accent()),
        seg(
            model.account.as_deref().unwrap_or("not signed in"),
            accent(),
        ),
        sep(),
    ];
    if !model.running_games.is_empty() {
        let names = model
            .running_games
            .iter()
            .map(|id| {
                model
                    .games
                    .iter()
                    .find(|game| game.app_id == *id)
                    .map_or_else(|| format!("[{id}]"), |game| game.name.clone())
            })
            .collect::<Vec<_>>()
            .join(", ");
        left.push(seg(
            format!("in game: {names}"),
            severity_style(Severity::Progress),
        ));
        if !model.notifications_live {
            left.push(seg(" (offline)", error_style()));
        }
    } else if model.notifications_live {
        left.push(seg("● online", severity_style(Severity::Progress)));
    } else {
        left.push(seg("○ offline", error_style()));
    }
    left.push(sep());
    if summary && let Some(text) = progress_summary(model, now) {
        left.push(seg(text, severity_style(Severity::Progress)));
        left.push(sep());
    }
    left.push(seg(model.status.clone(), message_style(&model.status)));
    if let Some(game) = model.selected_game() {
        let marker = theme::marker(game);
        if game.installed.is_none()
            && let Some(reason) = &game.unavailable_reason
        {
            left.push(sep());
            left.push(seg(format!("blocked: {reason}"), error_style()));
        } else if marker == PlatformMarker::Proton && game.installed.is_some() {
            left.push(sep());
            left.push(seg("⊞ Windows build via Proton", marker.style()));
        }
    }
    frame.render_widget(
        Paragraph::new(fit_line(left, columns[0].width as usize)),
        columns[0],
    );
    frame.render_widget(Paragraph::new(right.right_aligned()), columns[1]);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Col {
    Os,
    Cloud,
    State,
    Name,
    Size,
    Last,
    Played,
}

impl Col {
    fn width(self) -> usize {
        match self {
            Self::Os => 3,
            Self::Cloud => 1,
            Self::State => 13,
            Self::Name => NAME_MIN,
            Self::Size => 10,
            Self::Last => 10,
            Self::Played => 10,
        }
    }

    /// Header text, the sort it selects and its hotkey digit.
    fn header(self) -> (&'static str, Option<(Sort, &'static str)>) {
        match self {
            Self::Os => ("OS", None),
            Self::Cloud => ("☁", None),
            Self::State => ("State", None),
            Self::Name => ("Name", Some((Sort::Name, "¹"))),
            Self::Size => ("Size", Some((Sort::Size, "²"))),
            Self::Last => ("Last", Some((Sort::LastPlayed, "³"))),
            Self::Played => ("Played", Some((Sort::PlayTime, "⁴"))),
        }
    }
}

/// Visible columns for the inner width and the width left for the name; the least
/// important columns disappear first.
fn library_columns(inner_width: usize) -> (Vec<Col>, usize) {
    let mut columns = vec![
        Col::Os,
        Col::Cloud,
        Col::State,
        Col::Name,
        Col::Size,
        Col::Last,
        Col::Played,
    ];
    // The row highlight symbol takes two cells.
    let available = inner_width.saturating_sub(2);
    let fixed = |columns: &[Col]| {
        columns
            .iter()
            .filter(|column| **column != Col::Name)
            .map(|column| column.width())
            .sum::<usize>()
            + columns.len().saturating_sub(1)
    };
    for dropped in [
        Col::Cloud,
        Col::Last,
        Col::Played,
        Col::State,
        Col::Size,
        Col::Os,
    ] {
        if fixed(&columns) + NAME_MIN <= available {
            break;
        }
        columns.retain(|column| *column != dropped);
    }
    let name_width = available.saturating_sub(fixed(&columns)).max(1);
    (columns, name_width)
}

/// Glyph and color for the cloud column: `☁` dim never synced, green synced,
/// cyan `↑` local changes not uploaded, rotating yellow quarter circle while
/// syncing, red `✗` after a failed or conflicting sync. Blank without cloud.
fn cloud_cell(icon: Option<CloudIcon>) -> Cell<'static> {
    let (glyph, style) = match icon {
        None => return Cell::from(""),
        Some(CloudIcon::Never) => ("☁", dim()),
        Some(CloudIcon::Synced) => ("☁", Style::default().fg(Color::Green)),
        Some(CloudIcon::Pending) => ("↑", bold().fg(Color::Cyan)),
        Some(CloudIcon::Syncing(frame)) => {
            (["◐", "◓", "◑", "◒"][frame % 4], bold().fg(Color::Yellow))
        }
        Some(CloudIcon::Problem) => ("✗", bold().fg(Color::Red)),
    };
    Cell::from(Span::styled(glyph, style))
}

fn header_cell(column: Col, model: &Model) -> Cell<'static> {
    let (title, sort) = column.header();
    let right = matches!(column, Col::Size | Col::Last | Col::Played);
    let base = bold().fg(Color::White);
    let line = match sort {
        None => Line::from(Span::styled(title, base)),
        Some((sort, digit)) if model.sort == sort => Line::from(vec![
            Span::styled(format!("{title}{digit}"), accent()),
            Span::styled(if model.reverse { "▼" } else { "▲" }, accent()),
        ]),
        Some((_, digit)) => Line::from(vec![
            Span::styled(title, base),
            Span::styled(digit, key_cap()),
        ]),
    };
    Cell::from(if right { line.right_aligned() } else { line })
}

fn filter_line(model: &Model, width: usize) -> Line<'static> {
    let typed = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let segments = if model.searching {
        vec![
            seg("/", key_cap()),
            seg(" filter: ", dim()),
            seg(model.search.clone(), typed),
            seg("▏", typed),
            seg("  ↵ done · Esc clears", dim()),
        ]
    } else if !model.search.is_empty() {
        vec![
            seg("/", key_cap()),
            seg(" filter: ", dim()),
            seg(model.search.clone(), typed),
            seg("  Esc clears", dim()),
        ]
    } else {
        vec![seg("/", key_cap()), seg(" to filter", dim())]
    };
    fit_line(segments, width)
}

fn draw_library(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    let focused = model.focus == Focus::Library;
    let border = theme::panel_border(Panel::Library, focused);
    let mut left = vec![vec![seg("Library", bold())]];
    if model.loading {
        left.push(vec![seg(
            "↻ refreshing…",
            severity_style(Severity::Progress),
        )]);
    }
    let mut view = hint("v", "view");
    view.push(seg(": ", dim()));
    view.push(seg(
        model.filter.label().to_lowercase(),
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    ));
    left.push(view);
    let mut sort = hint("s", "sort");
    sort.push(seg(": ", dim()));
    sort.push(seg(
        format!(
            "{} {}",
            model.sort.label(),
            if model.reverse { "▼" } else { "▲" }
        ),
        Style::default().fg(Color::Yellow),
    ));
    left.push(sort);
    let position = if model.order.is_empty() {
        0
    } else {
        model.selected + 1
    };
    let mut count = vec![
        seg(position.to_string(), bold()),
        seg("/", dim()),
        seg(model.order.len().to_string(), bold()),
    ];
    if model.order.len() != model.listed_count() {
        count.push(seg(format!(" of {}", model.listed_count()), dim()));
    }
    let right = vec![count];
    let installed_selected = model
        .selected_game()
        .is_some_and(|game| game.installed.is_some());
    let selected = model.selected_game();
    let launching = selected.is_some_and(|game| model.launching.contains(&game.app_id));
    let mut bottom = vec![hint(
        &keys::label(Cmd::Activate),
        if installed_selected {
            "launch"
        } else {
            "install"
        },
    )];
    if installed_selected
        && !selected.is_some_and(|game| matches!(game.update, UpdateStatus::UpToDate))
    {
        bottom.push(hint(&keys::label(Cmd::Update), "update"));
    }
    if model.install_active {
        bottom.push(hint(
            &keys::label(Cmd::Pause),
            if model.paused { "resume" } else { "pause" },
        ));
    }
    if launching {
        bottom.push(hint(&keys::label(Cmd::Cancel), "skip shaders"));
    } else if model.install_active {
        bottom.push(hint(&keys::label(Cmd::Cancel), "cancel"));
    }
    if installed_selected {
        bottom.push(hint(&keys::label(Cmd::Manage), "manage"));
    }
    bottom.push(hint(&keys::label(Cmd::Downloads), "downloads"));
    bottom.push(hint(&keys::label(Cmd::News), "news"));
    bottom.push(hint(&keys::label(Cmd::Achievements), "achievements"));
    if installed_selected {
        bottom.push(hint(&keys::label(Cmd::SyncSaves), "sync saves"));
    }
    bottom.push(hint(&keys::label(Cmd::NextPane), "pane"));
    bottom.push(hint(&keys::label(Cmd::Refresh), "refresh"));
    let block = panel_block(border, area.width, left, right, bottom);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        model.library_icons.visible(&Default::default());
        return;
    }
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    frame.render_widget(
        Paragraph::new(filter_line(model, inner.width as usize)),
        parts[0],
    );
    let body = parts[1];
    if model.order.is_empty() {
        model.library_icons.visible(&Default::default());
        let (text, style) = if model.loading && model.games.is_empty() {
            (
                if model.inventory_ready {
                    "No installed games found · loading owned library from Steam…".to_owned()
                } else {
                    "Scanning installed Steam libraries…".to_owned()
                },
                severity_style(Severity::Progress),
            )
        } else if let Some(error) = &model.inventory_error {
            (
                format!("Inventory unavailable: {}\nr retries the scan", safe(error)),
                error_style(),
            )
        } else if !model.search.is_empty() {
            (
                "No names match this filter. Esc clears it.".to_owned(),
                Style::default(),
            )
        } else {
            (
                format!(
                    "No games in {} view. v changes view; r refreshes the library. Online login is required for owned, not-installed games.",
                    model.filter.label()
                ),
                Style::default(),
            )
        };
        frame.render_widget(
            Paragraph::new(text).style(style).wrap(Wrap { trim: false }),
            body,
        );
        return;
    }
    let (columns, name_width) = library_columns(body.width as usize);
    let show_icons = name_width >= NAME_MIN + 3;
    let visible_ids = if show_icons {
        model
            .order
            .iter()
            .skip(model.library_offset)
            .take(model.library_rows)
            .map(|&index| model.games[index].app_id)
            .collect()
    } else {
        Default::default()
    };
    model.library_icons.visible(&visible_ids);
    let icon_x = body.x
        + 2
        + columns
            .iter()
            .take_while(|column| **column != Col::Name)
            .map(|column| column.width() + 1)
            .sum::<usize>() as u16;
    let mut icon_positions = Vec::new();
    // Spinner frame: a quarter turn every 250 ms.
    let spin = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| (elapsed.as_millis() / 250) as usize);
    let pinned = model.pinned_count();
    let divider_at = model
        .has_divider()
        .then(|| pinned.checked_sub(model.library_offset))
        .flatten()
        .filter(|at| *at <= model.library_rows);
    let mut rows: Vec<Row<'_>> = model
        .order
        .iter()
        .skip(model.library_offset)
        .take(model.library_rows)
        .map(|&index| {
            let game = &model.games[index];
            let marker = theme::marker(game);
            let (tag, tag_style) = theme::state_tag(game);
            Row::new(columns.iter().map(|column| {
                match column {
                    Col::Os => Cell::from(Span::styled(marker.badge(), marker.style())),
                    Col::Cloud => cloud_cell(model.cloud_icon(game, spin)),
                    Col::State => match install_state(model, game) {
                        Some((text, style)) => Cell::from(Span::styled(text, style)),
                        None => Cell::from(Span::styled(tag, tag_style)),
                    },
                    Col::Name => {
                        let pinned = model.pins.contains(&game.app_id);
                        let label = if pinned {
                            format!("★ {}", game.name)
                        } else {
                            game.name.clone()
                        };
                        let mut spans = Vec::new();
                        if show_icons {
                            spans.push(Span::styled("▣  ", dim()));
                        }
                        spans.push(Span::styled(
                            fit(
                                &label,
                                name_width.saturating_sub(if show_icons { 3 } else { 0 }),
                            ),
                            name_style(game),
                        ));
                        Cell::from(Line::from(spans))
                    }
                    Col::Size => Cell::from(
                        Line::from(value_span(size(game.size_bytes), theme::size_style()))
                            .right_aligned(),
                    ),
                    Col::Last => Cell::from(
                        Line::from(value_span(last_played(game.last_played), Style::default()))
                            .right_aligned(),
                    ),
                    Col::Played => Cell::from(
                        Line::from(value_span(
                            play_time(game.playtime_minutes),
                            theme::playtime_style(),
                        ))
                        .right_aligned(),
                    ),
                }
            }))
        })
        .collect();
    if let Some(at) = divider_at {
        // Painted as one rule after the table; the row only reserves the line.
        rows.insert(at.min(rows.len()), Row::new(Vec::<Cell<'_>>::new()));
    }
    for index in 0..model
        .library_rows
        .min(model.order.len().saturating_sub(model.library_offset))
    {
        let visual = index + usize::from(divider_at.is_some_and(|at| index >= at));
        let y = body.y.saturating_add(1 + visual as u16);
        if y < body.bottom() {
            if show_icons {
                let app_id = model.games[model.order[model.library_offset + index]].app_id;
                model.library_icons.prepare(app_id);
                icon_positions.push((app_id, Rect::new(icon_x, y, 2, 1)));
            }
            model.library_hits.push((
                Rect::new(body.x, y, body.width, 1),
                model.library_offset + index,
            ));
        }
    }
    let widths: Vec<Constraint> = columns
        .iter()
        .map(|column| match column {
            Col::Name => Constraint::Length(name_width as u16),
            other => Constraint::Length(other.width() as u16),
        })
        .collect();
    let table = Table::new(rows, widths)
        .header(Row::new(
            columns.iter().map(|column| header_cell(*column, model)),
        ))
        .row_highlight_style(
            if model
                .selected_game()
                .is_some_and(|game| steam_core::launch::requires_valve_host(game.app_id))
            {
                highlight().fg(theme::VALVE_HOST_COLOR)
            } else {
                highlight()
            },
        )
        .highlight_symbol("› ");
    let mut selected = model.selected.saturating_sub(model.library_offset);
    if divider_at.is_some_and(|at| selected >= at) {
        selected += 1;
    }
    let mut state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, body, &mut state);
    if let Some(at) = divider_at {
        let y = body.y.saturating_add(1 + at as u16);
        if y < body.bottom() {
            frame.render_widget(
                Paragraph::new(Span::styled("─".repeat(body.width as usize), dim())),
                Rect::new(body.x, y, body.width, 1),
            );
        }
    }
    // Covered native graphics must not be marked transmitted before their
    // escape sequences can reach the terminal. Text cells remain safe to dim.
    let native_icons = model.chat.active.is_none()
        && model.modal.is_none()
        && !model.help
        && !model.backend_view
        && !model.queue_view
        && model.properties.is_none()
        && model.news.is_none()
        && model.achievements.is_none()
        && model.cloud_conflicts.is_none();
    for (app_id, rect) in icon_positions {
        model
            .library_icons
            .render(app_id, rect, native_icons, frame.buffer_mut());
    }
    // btop-style scrollbar on the right border, only when the list overflows.
    if model.order.len() > model.library_rows && area.height > 2 {
        let mut scroll = ScrollbarState::new(model.order.len().saturating_sub(model.library_rows))
            .position(model.library_offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("┆"))
                .thumb_symbol("█")
                .style(border),
            Rect::new(area.x, area.y + 1, area.width, area.height - 2),
            &mut scroll,
        );
    }
}

fn draw_friends(frame: &mut Frame<'_>, model: &Model, area: Rect) {
    let focused = model.focus == Focus::Friends;
    let border = theme::panel_border(Panel::Friends, focused);
    let count = model.friend_order.len();
    let first = if count == 0 {
        0
    } else {
        model.friend_offset + 1
    };
    let last = model
        .friend_offset
        .saturating_add(model.friend_rows)
        .min(count);
    let left = vec![
        vec![seg(format!("Friends {first}-{last}/{count}"), bold())],
        {
            let mut group = hint("o", "offline");
            group.push(seg(
                if model.show_offline {
                    " shown"
                } else {
                    " hidden"
                },
                dim(),
            ));
            group
        },
    ];
    let mut bottom = vec![hint("↑/↓", "select"), hint("Enter", "chat")];
    if !model.chat.available {
        bottom.insert(0, vec![seg("Chat offline", dim())]);
    } else if !model.chat.status.is_empty() {
        bottom.insert(0, vec![seg("Chat needs attention", dim())]);
    }
    let block = panel_block(border, area.width, left, Vec::new(), bottom);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if count == 0 {
        let (message, style) = if model.friends.is_empty() {
            (
                safe(&model.friends_status),
                message_style(&model.friends_status),
            )
        } else {
            (
                "No online friends. Press o to show offline friends.".to_owned(),
                Style::default(),
            )
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(style)
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let width = inner.width as usize;
    let lines: Vec<Line<'static>> = model
        .friend_order
        .iter()
        .enumerate()
        .skip(model.friend_offset)
        .take(model.friend_rows)
        .map(|(position, &index)| {
            let friend = &model.friends[index];
            let offline = friend.state.is_offline();
            let presence = theme::presence_style(friend.state);
            let mut line = vec![
                seg(
                    if focused && position == model.friend_selected {
                        "› "
                    } else {
                        "  "
                    },
                    accent(),
                ),
                seg("● ", presence),
                seg(friend.name.clone(), if offline { dim() } else { bold() }),
                seg(" · ", dim()),
            ];
            if let Some(c) = model.chat.conversations.get(&friend.steam_id)
                && c.unread > 0
            {
                line.insert(0, seg(format!("[{}] ", c.unread), accent()));
            }
            match friend.game.as_deref() {
                Some(game) if !offline => line.push(seg(game, theme::playing_style())),
                _ => line.push(seg(friend.state.label(), presence)),
            }
            let line = fit_line(line, width);
            if focused && position == model.friend_selected {
                line.style(highlight())
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Badge glyph and color per count kind.
fn count_style(kind: CountKind) -> (&'static str, Color) {
    match kind {
        CountKind::TradeOffers => ("⇄", Color::Yellow),
        CountKind::CommunityMessages | CountKind::Comments => ("✉", Color::Cyan),
        CountKind::Items => ("◆", Color::Green),
        CountKind::Gifts => ("✦", Color::Magenta),
        CountKind::FriendInvites | CountKind::FamilyInvites => ("☻", Color::LightBlue),
        CountKind::Other(_) => ("•", Color::White),
    }
}

fn label_color(label: &str) -> Color {
    match label {
        "Trade offer" => Color::Yellow,
        "Chat message" | "Comment" => Color::Cyan,
        "Gift" | "Partner event" => Color::Magenta,
        "Wishlist" | "Major sale" | "Item" | "Preload available" => Color::Green,
        "Friend invite" | "Family invite" => Color::LightBlue,
        "Moderator message" | "2FA prompt" | "Mobile confirmation" => Color::Red,
        _ => Color::White,
    }
}

/// Local clock time for today's notifications, month-day for older ones.
fn clock(timestamp: u64, now: DateTime<Local>) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|time| Local.timestamp_opt(time, 0).single())
        .map(|time| {
            if time.date_naive() == now.date_naive() {
                time.format("%H:%M").to_string()
            } else {
                time.format("%m-%d").to_string()
            }
        })
        .unwrap_or_else(|| "--:--".to_owned())
}

/// `✉ 2 community messages · ⇄ 1 trade offers`, or a dim note when nothing is pending.
fn badge_segments(counts: &[(CountKind, u32)]) -> Vec<Seg> {
    if counts.is_empty() {
        return vec![seg("nothing pending", dim())];
    }
    let mut segments = Vec::new();
    for (index, (kind, count)) in counts.iter().enumerate() {
        let (icon, color) = count_style(*kind);
        if index > 0 {
            segments.push(sep());
        }
        let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        segments.push(seg(format!("{icon} {count}"), style));
        segments.push(seg(format!(" {}", kind.label().to_lowercase()), dim()));
    }
    segments
}

fn notification_line(
    model: &Model,
    _index: usize,
    notification: &Notification,
    now: DateTime<Local>,
    width: usize,
) -> Line<'static> {
    let fresh = model.notif_fresh.contains(&notification.id);
    let mut line = vec![
        seg(
            if fresh {
                "● "
            } else if !notification.read {
                "○ "
            } else {
                "  "
            },
            accent(),
        ),
        seg(clock(notification.timestamp, now), dim()),
        seg(" ", Style::default()),
        seg(
            notification.label.clone(),
            Style::default()
                .fg(label_color(&notification.label))
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if notification.title != notification.label {
        line.push(seg(format!(" {}", notification.title), bold()));
    }
    if !notification.body.is_empty() {
        line.push(seg(" — ", dim()));
        line.push(seg(notification.body.clone(), Style::default()));
    }
    fit_line(line, width)
}

fn draw_notifications(frame: &mut Frame<'_>, model: &Model, area: Rect) {
    let focused = model.focus == Focus::Notifications;
    let border = theme::panel_border(Panel::Notifications, focused);
    let mut title = vec![seg("Notifications", bold())];
    if model.notif_unseen > 0 {
        title.push(seg(format!(" {}", model.notif_unseen), accent()));
    }
    let bottom = vec![
        hint("PgUp/PgDn", "scroll"),
        hint("x", "clear"),
        hint("Tab", "pane"),
    ];
    let block = panel_block(border, area.width, vec![title], Vec::new(), bottom);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let mut lines = vec![fit_line(badge_segments(&model.notif_counts), width)];
    if !model.notif_status.is_empty() {
        lines.push(fit_line(
            vec![seg(
                model.notif_status.clone(),
                message_style(&model.notif_status),
            )],
            width,
        ));
    }
    if model.notifications.is_empty() {
        if model.notifications_live && model.notif_status.is_empty() {
            lines.push(fit_line(
                vec![seg("No recent notifications — new ones appear live", dim())],
                width,
            ));
        }
    } else {
        let now = Local::now();
        lines.extend(
            model
                .notifications
                .iter()
                .enumerate()
                .skip(model.notif_offset)
                .take(model.notif_rows)
                .map(|(index, notification)| {
                    notification_line(model, index, notification, now, width)
                }),
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Text first, then a bar made only of fill/empty cells: the fill never covers text.
fn bar_segments(ratio: f64, width: usize, color: Color) -> Vec<Seg> {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    vec![
        seg("█".repeat(filled), Style::default().fg(color)),
        seg("░".repeat(width - filled), dim()),
    ]
}

/// Width of the phase column, so the bar starts at the same cell in every phase.
const PHASE_COLUMN: usize = 11;

fn draw_install(frame: &mut Frame<'_>, model: &Model, area: Rect, now: Instant) {
    let progress = model.progress.as_ref();
    let phase = progress.map_or(InstallPhase::Preparing, |progress| progress.phase);
    let cancelling = model.cancel_requested;
    let color = theme::phase_color(phase, cancelling || model.paused);
    let border = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let app_id = progress
        .map(|progress| progress.app_id)
        .or(model.active_install.as_ref().map(|(app_id, _)| *app_id));
    let name = app_id.and_then(|app_id| {
        model
            .games
            .iter()
            .find(|game| game.app_id == app_id)
            .map(|game| game.name.as_str())
    });
    let mut title = vec![seg(" Install", bold())];
    if let Some(name) = name {
        title.push(sep());
        title.push(seg(fit(name, 36), accent()));
    }
    if let Some(app_id) = app_id {
        title.push(seg(format!(" [{app_id}]"), dim()));
    }
    title.push(seg(" ", Style::default()));
    let right = if cancelling {
        vec![vec![seg(
            "cancelling safely…",
            severity_style(Severity::Warning),
        )]]
    } else if phase == InstallPhase::Committing {
        vec![vec![seg("cancel waits for install", dim())]]
    } else if model.install_active {
        vec![
            hint(
                &keys::label(Cmd::Pause),
                if model.paused { "resume" } else { "pause" },
            ),
            hint(&keys::label(Cmd::Cancel), "cancel"),
        ]
    } else {
        Vec::new()
    };
    let block = panel_block(border, area.width, vec![title], right, Vec::new());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let Some(progress) = progress else {
        let (text, style, hint_text) = if model.paused {
            (
                "⏸ Paused before the download starts",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                "p resumes · c cancels safely",
            )
        } else {
            (
                "Preparing installer…",
                severity_style(Severity::Progress),
                "p pauses · c cancels safely",
            )
        };
        frame.render_widget(
            Paragraph::new(vec![
                fit_line(vec![seg(text, style)], width),
                fit_line(vec![seg(hint_text, dim())], width),
            ]),
            inner,
        );
        return;
    };

    let phase_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let mut bar_line = vec![
        seg(
            format!("{:<PHASE_COLUMN$}", phase_text(model, progress.phase)),
            phase_style,
        ),
        seg(format!(" {:>3}% ", percent(progress)), bold()),
    ];
    let bar_width = width.saturating_sub(PHASE_COLUMN + 6);
    if bar_width >= 4 {
        bar_line.extend(bar_segments(progress_ratio(progress), bar_width, color));
    }
    let bytes_line = if progress.phase == InstallPhase::Verifying {
        vec![
            seg("hashed ", dim()),
            seg(size(Some(progress.verify_bytes_done)), theme::size_style()),
            seg(" / ", dim()),
            seg(size(Some(progress.verify_bytes_total)), theme::size_style()),
            sep(),
            seg("downloaded ", dim()),
            seg(size(Some(progress.downloaded_bytes)), theme::size_style()),
        ]
    } else {
        vec![
            seg("verified ", dim()),
            seg(size(Some(progress.completed_bytes)), theme::size_style()),
            seg(" / ", dim()),
            seg(size(Some(progress.total_bytes)), theme::size_style()),
            sep(),
            seg("downloaded ", dim()),
            seg(size(Some(progress.downloaded_bytes)), theme::size_style()),
            seg(" / ", dim()),
            seg(
                size(Some(progress.total_download_bytes)),
                theme::size_style(),
            ),
        ]
    };
    let mut timing = Vec::new();
    if progress.phase == InstallPhase::Downloading {
        let remaining = progress
            .total_download_bytes
            .saturating_sub(progress.downloaded_bytes);
        let speed = model.speed.speed(now);
        timing.push(seg("↓ ", dim()));
        timing.push(if model.paused {
            seg("paused", phase_style)
        } else {
            seg(
                rate_text(speed),
                if speed.is_some() {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    dim()
                },
            )
        });
        timing.extend([
            sep(),
            seg("ETA ", dim()),
            seg(eta_text(model.speed.eta(now, remaining)), bold()),
            sep(),
        ]);
    } else if progress.phase == InstallPhase::Verifying {
        let remaining = progress
            .verify_bytes_total
            .saturating_sub(progress.verify_bytes_done);
        let speed = model.verify_speed.speed(now);
        timing.push(seg("⟳ ", dim()));
        timing.push(seg(
            rate_text(speed),
            if speed.is_some() {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                dim()
            },
        ));
        timing.extend([
            sep(),
            seg("ETA ", dim()),
            seg(eta_text(model.verify_speed.eta(now, remaining)), bold()),
            sep(),
        ]);
    }
    timing.extend([
        seg("elapsed ", dim()),
        seg(eta_text(model.speed.elapsed(now)), Style::default()),
    ]);
    frame.render_widget(
        Paragraph::new(vec![
            fit_line(bar_line, width),
            fit_line(bytes_line, width),
            fit_line(timing, width),
            fit_line(
                vec![seg(
                    progress.message.clone(),
                    install_message_style(progress.phase),
                )],
                width,
            ),
        ]),
        inner,
    );
}

fn plan_platform_segments(platform: InstallPlatform) -> Vec<Seg> {
    let marker = PlatformMarker::of(Some(platform));
    vec![
        seg(marker.badge(), marker.style()),
        seg(" ", Style::default()),
        seg(marker.label(), marker.style()),
    ]
}

fn destination_modal(frame: &mut Frame<'_>, model: &Model) {
    let Some(modal) = &model.modal else {
        return;
    };
    let area = modal_area(frame.area(), 94, 22);
    open_modal(frame, area);
    let bottom = if modal.custom.is_some() {
        vec![hint("Enter", "check space"), hint("Esc", "back")]
    } else if modal.existing.is_some() {
        vec![hint("Enter", "update here"), hint("Esc", "cancel")]
    } else {
        vec![
            hint("↑↓", "choose"),
            hint("n", "custom library"),
            hint("Enter", "confirm"),
            hint("Esc", "cancel"),
        ]
    };
    let block = panel_block(
        theme::panel_border(Panel::Overlay, true),
        area.width,
        vec![vec![seg(
            if modal.existing.is_some() {
                "Update destination"
            } else {
                "Install destination"
            },
            bold(),
        )]],
        vec![vec![seg(safe(&modal.plan.name), dim())]],
        bottom,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let regions = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(4),
    ])
    .split(inner);
    let inner_width = inner.width as usize;
    let mut heading_line = vec![
        seg(modal.plan.name.clone(), bold()),
        seg(format!(" [{}]", modal.plan.app_id), dim()),
        sep(),
    ];
    heading_line.extend(plan_platform_segments(modal.plan.platform));
    heading_line.extend([
        sep(),
        seg("branch ", dim()),
        seg(
            modal.plan.branch.clone(),
            Style::default().fg(Color::Yellow),
        ),
    ]);
    let note = if modal.plan.platform == InstallPlatform::Windows {
        seg(
            PROTON_NOTE,
            PlatformMarker::of(Some(InstallPlatform::Windows)).style(),
        )
    } else {
        seg("Native Linux build · no Proton needed", dim())
    };
    frame.render_widget(
        Paragraph::new(vec![
            fit_line(heading_line, inner_width),
            fit_line(
                vec![
                    seg("Staged files ", dim()),
                    seg(size(Some(modal.plan.installed_bytes)), theme::size_style()),
                    seg(" · download ", dim()),
                    seg(size(Some(modal.plan.download_bytes)), theme::size_style()),
                ],
                inner_width,
            ),
            fit_line(vec![note], inner_width),
            Line::from(Span::styled(
                "Exact manifest space is checked before downloading.",
                dim(),
            )),
        ]),
        regions[0],
    );
    let items: Vec<ListItem<'_>> = modal
        .destinations
        .iter()
        .map(|destination| {
            let blocked = !destination.available
                || destination
                    .free_bytes
                    .is_some_and(|free| free < modal.plan.installed_bytes);
            ListItem::new(vec![
                Line::from(fit(
                    &destination.path.to_string_lossy(),
                    inner.width.saturating_sub(2) as usize,
                )),
                Line::from(Span::styled(
                    fit(
                        &format!(
                            "  Free: {}{}{}",
                            size(destination.free_bytes),
                            if !destination.available {
                                " · unavailable"
                            } else if blocked {
                                " · below full size; Enter twice for exact check"
                            } else {
                                ""
                            },
                            destination
                                .message
                                .as_ref()
                                .map(|message| format!(" · {message}"))
                                .unwrap_or_default()
                        ),
                        inner.width.saturating_sub(2) as usize,
                    ),
                    Style::default().fg(if blocked { Color::Red } else { Color::Green }),
                )),
            ])
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(modal.selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(highlight()),
        regions[1],
        &mut state,
    );
    let mut footer: Vec<Line<'static>> = Vec::new();
    if let Some(path) = &modal.custom {
        footer.push(fit_line(
            vec![
                seg("Library path ", dim()),
                seg(path.clone(), bold()),
                seg(
                    if modal.checking {
                        " (checking…)"
                    } else {
                        "▏"
                    },
                    severity_style(Severity::Progress),
                ),
            ],
            inner_width,
        ));
    } else if let Some(destination) = modal.selected_destination() {
        footer.push(fit_line(
            vec![
                seg("Selected ", dim()),
                seg(destination.path.display().to_string(), bold()),
            ],
            inner_width,
        ));
    }
    if let Some(error) = modal
        .error
        .as_ref()
        .cloned()
        .or_else(|| modal.blocked_reason())
    {
        // Low-space confirmations are cautions; everything else blocks.
        let style = if theme::classify(&error) == Severity::Warning {
            severity_style(Severity::Warning)
        } else {
            error_style()
        };
        footer.push(fit_line(vec![seg(error, style)], inner_width));
    }
    frame.render_widget(Paragraph::new(footer), regions[2]);
}

/// Splits a help line into the key column (14 cells) and its description.
fn help_entry(raw: &str) -> Vec<Seg> {
    let split = raw
        .char_indices()
        .nth(14)
        .map(|(index, _)| index)
        .unwrap_or(raw.len());
    let (key, description) = raw.split_at(split);
    let key_style = match key.trim() {
        "⊞" | "λ" => {
            let marker = if key.trim() == "⊞" {
                PlatformMarker::Proton
            } else {
                PlatformMarker::Native
            };
            marker.style()
        }
        "" => Style::default(),
        _ => key_cap(),
    };
    vec![seg(key, key_style), seg(description, Style::default())]
}

fn is_help_heading(raw: &str) -> bool {
    raw.starts_with(char::is_uppercase) && !raw.chars().any(char::is_lowercase)
}

fn help_lines(model: &Model) -> Vec<Vec<Seg>> {
    let mut lines: Vec<Vec<Seg>> = Vec::new();
    let mut keyed = true;
    let help = keys::help_text();
    for raw in help.lines() {
        if is_help_heading(raw) {
            keyed = raw != "NOTES";
            lines.push(vec![seg(raw, heading())]);
        } else if keyed {
            lines.push(help_entry(raw));
        } else {
            lines.push(vec![seg(raw, Style::default())]);
        }
    }
    if !model.warnings.is_empty() {
        lines.push(Vec::new());
        lines.push(vec![seg(
            format!("SCAN WARNINGS ({})", model.warnings.len()),
            heading(),
        )]);
        for warning in &model.warnings {
            lines.push(vec![seg(safe(warning), severity_style(Severity::Warning))]);
        }
    }
    if let Some(game) = model.selected_game() {
        lines.push(Vec::new());
        lines.push(vec![seg("SELECTED GAME", heading())]);
        lines.push(vec![
            seg(safe(&game.name), name_style(game)),
            seg(format!(" [{}]", game.app_id), dim()),
        ]);
        let (update, style) = match &game.update {
            UpdateStatus::Available {
                installed_build,
                latest_build,
            } => (
                format!("Update available: build {installed_build} → {latest_build}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            UpdateStatus::UpToDate => (
                "Current Steam build is installed".to_owned(),
                Style::default().fg(Color::Green),
            ),
            UpdateStatus::Unknown => (
                "Current build status is unknown".to_owned(),
                Style::default().fg(Color::Magenta),
            ),
            UpdateStatus::Unavailable(reason) => (
                format!("Update unavailable: {}", safe(reason)),
                error_style(),
            ),
        };
        lines.push(vec![seg(update, style)]);
        let marker = theme::marker(game);
        let mut platform = vec![
            seg("Platform: ", dim()),
            seg(marker.label(), marker.style()),
        ];
        if marker == PlatformMarker::Proton {
            platform.push(sep());
            platform.push(seg(PROTON_NOTE, marker.style()));
        }
        lines.push(platform);
        if steam_core::launch::requires_valve_host(game.app_id) {
            lines.push(vec![seg(
                "VAC game: Enter / double-click automatically uses Valve host",
                bold().fg(theme::VALVE_HOST_COLOR),
            )]);
        }
        if let Some(installed) = &game.installed {
            lines.push(vec![
                seg("Installed: ", dim()),
                seg(
                    installed.install_dir.display().to_string(),
                    Style::default(),
                ),
            ]);
            lines.push(vec![
                seg("Branch: ", dim()),
                seg(safe(&installed.branch), Style::default().fg(Color::Yellow)),
                seg(" · language: ", dim()),
                seg(
                    safe(&installed.language),
                    Style::default().fg(Color::Yellow),
                ),
            ]);
        }
        if let Some(message) = model.install_results.get(&game.app_id) {
            lines.push(vec![seg("INSTALL RESULT", heading())]);
            lines.push(vec![seg(safe(message), message_style(message))]);
        }
        if let Some(message) = model.launches.get(&game.app_id) {
            lines.push(vec![seg("DIRECT LAUNCH", heading())]);
            lines.push(vec![seg(safe(message), message_style(message))]);
        }
    }
    lines.push(vec![seg("FRIENDS STATUS", heading())]);
    lines.push(vec![seg(
        safe(&model.friends_status),
        message_style(&model.friends_status),
    )]);
    lines
}

/// Width of the key column of a help entry (`help_entry` splits at this many cells).
const HELP_KEY_COLUMN: usize = 14;

/// Wraps a `key  description` help entry at word boundaries; continuation lines
/// hang under the description. Anything else wraps by character.
fn wrap_help_entry(segments: &[Seg], width: usize) -> Vec<Line<'static>> {
    let [(key, key_style), (description, text_style)] = segments else {
        return wrap_segments(segments, width);
    };
    let room = width.saturating_sub(HELP_KEY_COLUMN);
    if UnicodeWidthStr::width(key.as_str()) != HELP_KEY_COLUMN || room < 8 {
        return wrap_segments(segments, width);
    }
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in description.split(' ') {
        let candidate = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if UnicodeWidthStr::width(candidate.as_str()) > room && !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            current = word.to_owned();
        } else {
            current = candidate;
        }
    }
    rows.push(current);
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let lead = if index == 0 {
                Span::styled(key.clone(), *key_style)
            } else {
                Span::raw(" ".repeat(HELP_KEY_COLUMN))
            };
            Line::from(vec![lead, Span::styled(row, *text_style)])
        })
        .collect()
}

/// Character-wraps one logical line across segments, preserving each run's style.
fn wrap_segments(segments: &[Seg], width: usize) -> Vec<Line<'static>> {
    let mut output = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (text, style) in segments {
        let mut chunk = String::new();
        for ch in text.chars() {
            let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + char_width > width && used > 0 {
                if !chunk.is_empty() {
                    line.push(Span::styled(std::mem::take(&mut chunk), *style));
                }
                output.push(Line::from(std::mem::take(&mut line)));
                used = 0;
            }
            chunk.push(ch);
            used += char_width;
        }
        if !chunk.is_empty() {
            line.push(Span::styled(chunk, *style));
        }
    }
    output.push(Line::from(line));
    output
}

/// Full-width traffic log of the per-launch Steam backend, newest at the bottom.
fn backend_overlay(frame: &mut Frame<'_>, model: &mut Model) {
    use steam_core::backend::Direction;
    let screen = frame.area();
    let area = Rect::new(
        screen.x,
        screen.y,
        screen.width,
        screen.height.saturating_sub(2).max(3),
    );
    open_modal(frame, area);
    let width = area.width.saturating_sub(2).max(1) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in &model.backend_log {
        let event = &entry.event;
        let (arrow, style) = match event.direction {
            Direction::ToSteam => (
                "game → Steam",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Direction::FromSteam => (
                "Steam → game",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Direction::Local => ("backend", Style::default().fg(Color::Green)),
        };
        let time = DateTime::<Local>::from(event.at).format("%H:%M:%S");
        let head = vec![
            seg(format!("{time} "), dim()),
            seg(format!("{arrow:<12} "), style),
            seg(
                format!("[{}] ", event.channel),
                Style::default().fg(Color::Yellow),
            ),
            seg(safe(&event.title), bold()),
        ];
        let mut head = head;
        if entry.count > 1 {
            head.push(seg(
                format!(" ×{}", entry.count),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            let last = DateTime::<Local>::from(entry.last_at).format("%H:%M:%S");
            head.push(seg(format!(" (last {last})"), dim()));
        }
        lines.extend(wrap_segments(&head, width));
        if !event.detail.is_empty() {
            let detail = vec![seg(format!("    {}", safe(&event.detail)), dim())];
            lines.extend(wrap_segments(&detail, width));
        }
    }
    if lines.is_empty() {
        lines.extend(wrap_segments(
            &[seg(
                "No backend traffic yet — press Enter to launch with the built-in backend.",
                dim(),
            )],
            width,
        ));
    }
    let rows = area.height.saturating_sub(2) as usize;
    let max_offset = lines.len().saturating_sub(rows);
    model.backend_offset = model.backend_offset.min(max_offset);
    let top = max_offset - model.backend_offset;
    let right = vec![vec![
        seg(
            model
                .backend_log
                .iter()
                .map(|entry| entry.count as usize)
                .sum::<usize>()
                .to_string(),
            bold(),
        ),
        seg(" messages", dim()),
    ]];
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(
                theme::panel_border(Panel::Overlay, true),
                area.width,
                vec![vec![seg("Steam backend traffic", bold())]],
                right,
                vec![
                    hint("↑↓", "scroll"),
                    hint("End", "follow"),
                    hint("x", "clear"),
                    hint("Esc", "close"),
                ],
            ))
            .scroll((top.min(u16::MAX as usize) as u16, 0)),
        area,
    );
}

/// Keep unknown bracketed text (e.g. version numbers), strip presentation tags.
fn news_text(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(['[', '<']) {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let end_char = if rest.starts_with('[') { ']' } else { '>' };
        let Some(end) = rest.find(end_char) else {
            break;
        };
        let tag = rest[1..end].trim().to_ascii_lowercase();
        let name = tag
            .trim_start_matches('/')
            .split(['=', ' ', '/'])
            .next()
            .unwrap_or("");
        let known = matches!(
            name,
            "b" | "i"
                | "u"
                | "s"
                | "h1"
                | "h2"
                | "h3"
                | "p"
                | "br"
                | "ul"
                | "ol"
                | "li"
                | "list"
                | "*"
                | "url"
                | "a"
                | "img"
                | "previewyoutube"
                | "hr"
                | "table"
                | "tr"
                | "td"
                | "th"
                | "quote"
                | "blockquote"
                | "code"
        );
        if known {
            if matches!(
                name,
                "h1" | "h2" | "h3" | "p" | "br" | "li" | "*" | "list" | "ul" | "ol" | "hr" | "tr"
            ) {
                output.push('\n');
            }
            if matches!(name, "li" | "*") && !tag.starts_with('/') {
                output.push_str("• ");
            }
        } else {
            output.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .lines()
        .map(safe)
        .collect::<Vec<_>>()
        .join("\n")
}

fn news_overlay(frame: &mut Frame<'_>, model: &mut Model) {
    let news = model.news.as_mut().expect("news view open");
    let area = modal_area(frame.area(), 100, 36);
    let (width, height) = (area.width, area.height);
    open_modal(frame, area);
    let logical = match &news.result {
        None => vec![seg("Loading developer updates…", dim())],
        Some(Err(error)) => vec![seg(
            format!("Could not load updates: {}", safe(error)),
            error_style(),
        )],
        Some(Ok(items)) if items.is_empty() => vec![seg(
            "No developer announcements available for this game.",
            dim(),
        )],
        Some(Ok(items)) => {
            let mut lines = Vec::new();
            for item in items {
                lines.push(seg(
                    format!("{}  {}", last_played(Some(item.date)), safe(&item.title)),
                    bold(),
                ));
                lines.push(seg(news_text(&item.contents), Style::default()));
                lines.push(seg(safe(&item.url), dim()));
                lines.push(seg("", dim()));
            }
            lines
        }
    };
    let mut lines = Vec::new();
    for (text, style) in logical {
        for line in text.lines().chain(std::iter::once("")) {
            lines.extend(wrap_segments(
                &[seg(line, style)],
                width.saturating_sub(2).max(1) as usize,
            ));
        }
    }
    news.offset = news.offset.min(
        lines
            .len()
            .saturating_sub(height.saturating_sub(2) as usize)
            .min(u16::MAX as usize) as u16,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((news.offset, 0))
            .block(panel_block(
                theme::panel_border(Panel::Overlay, true),
                width,
                vec![vec![seg(format!("{} · Updates", safe(&news.name)), bold())]],
                Vec::new(),
                vec![hint("↑↓", "scroll"), hint("Esc", "close")],
            )),
        area,
    );
}

/// Rarity tiers colour the percentage of players who unlocked an achievement.
fn rarity_style(percent: f32) -> Style {
    if percent < 5.0 {
        Style::default().fg(Color::Yellow)
    } else if percent < 20.0 {
        Style::default().fg(Color::Cyan)
    } else {
        dim()
    }
}

fn achievements_overlay(frame: &mut Frame<'_>, model: &mut Model) {
    let view = model.achievements.as_mut().expect("achievements view open");
    let area = modal_area(frame.area(), 110, 40);
    open_modal(frame, area);
    let (unlocked, total) = view
        .loaded()
        .map_or((0, 0), |all| (all.unlocked(), all.items.len()));
    let mut right = Vec::new();
    if total > 0 {
        right.push(vec![
            seg(format!("{unlocked}/{total}"), bold()),
            seg(
                format!(" ({:.0}%)", unlocked as f64 * 100.0 / total as f64),
                dim(),
            ),
        ]);
    }
    let block = panel_block(
        theme::panel_border(Panel::Overlay, true),
        area.width,
        vec![vec![seg(
            format!("{} · Achievements", safe(&view.name)),
            bold(),
        )]],
        right,
        vec![
            hint("↑↓", "move"),
            hint("1-4", "sort"),
            hint("s", "next sort"),
            hint("f", "filter"),
            hint("r", "reload"),
            hint("Esc", "close"),
        ],
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let message = match &view.result {
        None => Some(seg("Loading achievements…", dim())),
        Some(Err(error)) => Some(seg(
            format!("Could not load achievements: {}", safe(error)),
            error_style(),
        )),
        Some(Ok(all)) if all.items.is_empty() => Some(seg("This game has no achievements.", dim())),
        Some(Ok(_)) => None,
    };
    if let Some(message) = message {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(message.0, message.1)])),
            inner,
        );
        return;
    }
    let regions = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    let width = inner.width as usize;
    // Progress bar, then the active sort and filter.
    let filled = if total == 0 {
        0
    } else {
        width * unlocked / total
    };
    let bar = vec![
        Span::styled("█".repeat(filled), Style::default().fg(Color::Green)),
        Span::styled("░".repeat(width.saturating_sub(filled)), dim()),
    ];
    let order = Line::from(vec![
        Span::styled("sort ", dim()),
        Span::styled(view.sort.label(), accent()),
        Span::styled(if view.reverse { " ↑" } else { " ↓" }, accent()),
        Span::styled("   show ", dim()),
        Span::styled(view.filter.label(), accent()),
        Span::styled(
            if view.loaded().is_some_and(|a| a.has_percentages) {
                ""
            } else {
                "   (rarity unavailable)"
            },
            dim(),
        ),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::from(bar), order]), regions[0]);
    let list_area = regions[1];
    let visible = view.visible();
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Nothing matches this filter.",
                dim(),
            ))),
            list_area,
        );
        return;
    }
    // Right column: rarity and unlock date.
    let right_width = 20usize;
    let left_width = width.saturating_sub(right_width + 4).max(10);
    let rows_visible = (list_area.height as usize / 2).max(1);
    let selected = view.selected.min(visible.len() - 1);
    let mut offset = view.offset.min(visible.len() - 1);
    if selected < offset {
        offset = selected;
    }
    if selected >= offset + rows_visible {
        offset = selected + 1 - rows_visible;
    }
    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(rows_visible)
        .map(|(index, item)| {
            let (mark, mark_style) = if item.unlocked {
                (
                    "✓",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("·", dim())
            };
            let name_style = if item.unlocked {
                bold()
            } else {
                Style::default()
            };
            let name = fit(&item.display_name, left_width);
            let pad = left_width.saturating_sub(UnicodeWidthStr::width(name.as_str()));
            let percent = item
                .percent
                .map_or_else(|| "     ?".to_owned(), |p| format!("{p:>5.1}%"));
            let date = if item.unlocked {
                item.unlock_time.map_or_else(
                    || "unlocked".to_owned(),
                    |t| last_played(Some(u64::from(t))),
                )
            } else {
                String::new()
            };
            let description = if item.hidden && !item.unlocked {
                "Hidden achievement".to_owned()
            } else {
                item.description.clone()
            };
            let first = Line::from(vec![
                Span::styled(format!(" {mark} "), mark_style),
                Span::styled(name, name_style),
                Span::raw(" ".repeat(pad + 1)),
                Span::styled(percent, item.percent.map_or_else(dim, rarity_style)),
                Span::styled(format!("  {date:>10}"), dim()),
            ]);
            let second = Line::from(vec![
                Span::raw("     "),
                Span::styled(fit(&description, width.saturating_sub(6)), dim()),
            ]);
            let item = ListItem::new(vec![first, second]);
            if index == selected {
                item.style(theme::highlight())
            } else {
                item
            }
        })
        .collect();
    view.offset = offset;
    frame.render_widget(List::new(items), list_area);
}

fn cloud_conflicts(frame: &mut Frame<'_>, model: &Model) {
    use steam_core::cloud::Choice;
    let modal = model.cloud_conflicts.as_ref().unwrap();
    let Some(item) = modal.items.get(modal.index) else {
        return;
    };
    let area = modal_area(frame.area(), 100, 14);
    open_modal(frame, area);
    let block = panel_block(
        theme::panel_border(Panel::Overlay, true),
        area.width,
        vec![
            vec![seg("Save conflict", bold())],
            vec![seg(
                format!("{}/{}", modal.index + 1, modal.items.len()),
                accent(),
            )],
        ],
        vec![vec![seg(safe(&modal.game.name), dim())]],
        vec![
            hint("↑↓", "file"),
            hint("l/r", "choose"),
            hint("Enter", "apply"),
            hint("Esc", "keep both"),
        ],
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(6),
        Constraint::Min(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(fit(&item.name, inner.width as usize), bold())),
            Line::from(Span::styled(
                "Both copies changed since the last sync. The replaced copy is backed up.",
                dim(),
            )),
        ]),
        rows[0],
    );
    let cards =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[1]);
    let card = |frame: &mut Frame<'_>,
                rect: Rect,
                title: &str,
                key: &str,
                version: &Option<steam_core::cloud::SaveVersion>,
                missing: &str,
                chosen: bool| {
        let border = if chosen {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            dim()
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border)
            .title_top(title_groups(
                vec![vec![
                    seg(key, key_cap()),
                    seg(
                        format!(" {title}"),
                        if chosen { bold() } else { Style::default() },
                    ),
                ]],
                border,
                rect.width.saturating_sub(2) as usize,
                Edge::Top,
            ))
            .title_bottom(title_groups(
                if chosen {
                    vec![vec![seg("✓ selected", severity_style(Severity::Progress))]]
                } else {
                    Vec::new()
                },
                border,
                rect.width.saturating_sub(2) as usize,
                Edge::Bottom,
            ));
        let content = block.inner(rect);
        frame.render_widget(block, rect);
        let width = content.width as usize;
        let lines = match version {
            Some(v) => vec![
                fit_line(
                    vec![
                        seg("Saved  ", dim()),
                        seg(last_played(Some(v.time)), Style::default()),
                    ],
                    width,
                ),
                fit_line(
                    vec![seg("Size   ", dim()), value_span_seg(size(Some(v.size)))],
                    width,
                ),
                fit_line(
                    vec![
                        seg("SHA1   ", dim()),
                        seg(v.sha1.chars().take(12).collect::<String>(), dim()),
                    ],
                    width,
                ),
            ],
            None => vec![fit_line(vec![seg(missing, dim())], width)],
        };
        frame.render_widget(Paragraph::new(lines), content);
    };
    card(
        frame,
        cards[0],
        "Local",
        "l",
        &item.local,
        "No local copy",
        modal.choice == Some(Choice::Local),
    );
    card(
        frame,
        cards[1],
        "Cloud",
        "r",
        &item.cloud,
        "No cloud copy",
        modal.choice == Some(Choice::Cloud),
    );
    let mut lines = vec![Line::from(match modal.choice {
        Some(Choice::Local) => {
            Span::styled("Enter keeps LOCAL and replaces the cloud copy", bold())
        }
        Some(Choice::Cloud) => {
            Span::styled("Enter keeps CLOUD and replaces the local copy", bold())
        }
        None => Span::styled(
            "Choose l (local) or r (cloud), then Enter. Other files keep their own choice.",
            dim(),
        ),
    })];
    if modal.busy {
        lines.push(Line::from(Span::styled(
            "Applying choice…",
            severity_style(Severity::Progress),
        )));
    }
    if let Some(error) = &modal.error {
        lines.push(Line::from(Span::styled(safe(error), error_style())));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[2]);
}

fn help(frame: &mut Frame<'_>, model: &mut Model) {
    let area = modal_area(frame.area(), 88, 28);
    let (width, height) = (area.width, area.height);
    open_modal(frame, area);
    let inner_width = width.saturating_sub(2).max(1) as usize;
    let lines: Vec<Line<'static>> = help_lines(model)
        .iter()
        .flat_map(|logical| wrap_help_entry(logical, inner_width))
        .collect();
    model.help_offset = model.help_offset.min(
        lines
            .len()
            .saturating_sub(height.saturating_sub(2) as usize)
            .min(u16::MAX as usize) as u16,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(
                theme::panel_border(Panel::Overlay, true),
                area.width,
                vec![vec![seg("Hotkeys", bold())]],
                vec![hint("↑↓", "scroll"), hint("Esc", "close")],
                Vec::new(),
            ))
            .scroll((model.help_offset, 0)),
        area,
    );
}

fn job_state_word(state: steam_core::updates::JobState) -> &'static str {
    use steam_core::updates::JobState;
    match state {
        JobState::Pending => "queued",
        JobState::Running => "running",
        JobState::Paused => "paused",
        JobState::Failed => "failed",
        JobState::Complete => "done",
        JobState::Cancelled => "cancelled",
    }
}

fn policy_word(policy: steam_core::updates::UpdatePolicy) -> &'static str {
    use steam_core::updates::UpdatePolicy;
    match policy {
        UpdatePolicy::Automatic => "auto",
        UpdatePolicy::LaunchOnly => "on launch",
        UpdatePolicy::Manual => "manual",
    }
}

fn queue_overlay(frame: &mut Frame<'_>, model: &Model) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    if model.queue_jobs.is_empty() {
        lines.push(Line::from("No persistent update jobs"));
    }
    for (index, job) in model.queue_jobs.iter().enumerate().skip(
        model
            .queue_selected
            .saturating_sub(area.height.saturating_sub(5) as usize),
    ) {
        let policy = model
            .queue_settings
            .policies
            .get(&job.intent.app_id)
            .copied()
            .unwrap_or_default();
        let text = format!(
            "{} #{}  app {}  {}  {}  priority {}  {}{}{}",
            if index == model.queue_selected {
                "›"
            } else {
                " "
            },
            job.id,
            job.intent.app_id,
            if job.intent.shader {
                "shaders"
            } else {
                "content"
            },
            job_state_word(job.state),
            job.priority,
            policy_word(policy),
            if job.foreground { " · foreground" } else { "" },
            job.error
                .as_ref()
                .map(|e| format!(" · {}", safe(e)))
                .unwrap_or_default()
        );
        lines.push(Line::styled(
            text,
            if index == model.queue_selected {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            },
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(
                theme::panel_border(Panel::Overlay, true),
                area.width,
                vec![vec![seg("Update queue", bold())]],
                Vec::new(),
                vec![
                    hint("↑↓", "select"),
                    hint("p", "pause/resume"),
                    hint("r", "retry"),
                    hint("c", "cancel"),
                    hint("+/-", "priority"),
                    hint("a", "app policy"),
                    hint("Esc", "close"),
                ],
            ))
            .wrap(Wrap { trim: false }),
        area,
    );
}
fn content_overlay(frame: &mut Frame<'_>, model: &Model) {
    let Some(view) = &model.properties else {
        return;
    };
    let mut lines = Vec::new();
    if let Some(data) = &view.data {
        let branch = data
            .branches
            .get(view.branch)
            .map(|b| {
                format!(
                    "{}{}",
                    b.name,
                    if b.password_required {
                        " (password required)"
                    } else {
                        ""
                    }
                )
            })
            .unwrap_or_else(|| "unavailable".into());
        let language = data
            .languages
            .get(view.language)
            .map(String::as_str)
            .unwrap_or("unavailable");
        let mut choices = vec![format!("Branch: {branch}"), format!("Language: {language}")];
        choices.extend(data.dlc.iter().map(|d| {
            format!(
                "[{}] {} [{}] · {}{}",
                if d.enabled { "x" } else { " " },
                safe(&d.name),
                d.app_id,
                if d.owned { "owned" } else { "unowned" },
                if d.installed { " · installed" } else { "" }
            )
        }));
        for (i, text) in choices.into_iter().enumerate() {
            lines.push(Line::styled(
                format!("{} {text}", if i == view.selected { "›" } else { " " }),
                if i == view.selected {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                },
            ));
        }
    }
    if let Some(password) = &view.password {
        lines.push(Line::from(format!(
            "Branch password: {}",
            "•".repeat(password.chars().count().min(80))
        )));
        lines.push(Line::from(format!(
            "F2: {} · Enter unlocks and queues the branch · Esc cancels",
            if view.remember_key {
                "remember returned key in SecretService"
            } else {
                "session only"
            }
        )));
    }
    if view.busy {
        lines.push(Line::from("Resolving licensed content…"));
    }
    if let Some(error) = &view.error {
        lines.push(Line::styled(safe(error), Style::default().fg(Color::Red)));
    }
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((
                view.selected
                    .saturating_sub(area.height.saturating_sub(6) as usize) as u16,
                0,
            ))
            .block(panel_block(
                theme::panel_border(Panel::Overlay, true),
                area.width,
                vec![vec![seg(
                    format!("Content · {}", safe(&view.game.name)),
                    bold(),
                )]],
                Vec::new(),
                vec![
                    hint("↑↓", "select"),
                    hint("←→", "branch/language"),
                    hint("Enter", "apply"),
                    hint("Space", "toggle DLC"),
                    hint("Esc", "close"),
                ],
            ))
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::tui::model::Action;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use steam_core::catalog::CatalogSnapshot;
    use steam_core::install::InstallProgress;
    use steam_core::installed::InstalledGame;
    use steam_core::notifications::NotificationEvent;

    fn game(app_id: u32, name: &str, installed: bool) -> CatalogItem {
        CatalogItem {
            app_id,
            name: name.to_owned(),
            installed: installed.then(|| InstalledGame {
                app_id,
                name: name.to_owned(),
                size_bytes: Some(5 << 30),
                last_played: Some(1_700_000_000),
                playtime_minutes: Some(125),
                install_dir: PathBuf::from("/games/x"),
                library_root: PathBuf::new(),
                steam_root: PathBuf::new(),
                launch_options: None,
                build_id: Some(7),
                branch: "public".into(),
                language: "english".into(),
                platform: None,
                installed_depots: Default::default(),
            }),
            plan: None,
            update: UpdateStatus::Unknown,
            last_played: Some(1_700_000_000),
            playtime_minutes: Some(125),
            size_bytes: Some(5 << 30),
            unavailable_reason: None,
        }
    }

    fn model() -> Model {
        let mut model = Model::new(true, None);
        model.set_catalog(CatalogSnapshot {
            items: (0..60)
                .map(|id| game(id + 1, &format!("Game number {id}"), id % 2 == 0))
                .collect(),
            warnings: Vec::new(),
        });
        model
    }

    fn render(model: &mut Model, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, model)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn mouse_targets_follow_pins_filter_and_scrolling_and_ignore_chrome() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let click = |rect: Rect, button| MouseEvent {
            kind: MouseEventKind::Down(button),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        };
        let mut model = model();
        model.pins.insert(2);
        model.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        model.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        for (width, height) in [(80, 24), (160, 40)] {
            render(&mut model, width, height);
            let hits = model.library_hits.clone();
            assert!(hits.len() > 2);
            let (first, first_index) = hits[0];
            let (second, second_index) = hits[1];
            assert_eq!(second.y, first.y + 2, "pin divider has no hit target");
            assert_eq!(model.mouse(click(second, MouseButton::Left)), Action::None);
            assert_eq!(model.selected, second_index);
            let gap = Rect::new(first.x, first.y + 1, first.width, 1);
            model.mouse(click(gap, MouseButton::Left));
            assert_eq!(model.selected, second_index);
            assert_eq!(model.mouse(click(first, MouseButton::Right)), Action::News);
            assert_eq!(model.selected, first_index);
            assert_eq!(
                model.news.as_ref().unwrap().app_id,
                model.selected_game().unwrap().app_id
            );
            model.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        model.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        render(&mut model, 80, 24);
        let (rect, index) = model.library_hits[0];
        assert!(index > 0);
        model.mouse(click(rect, MouseButton::Left));
        assert_eq!(model.selected, index);
        model.search = "number 5".into();
        // Search goes through the normal input path to rebuild filtered order.
        model.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        render(&mut model, 80, 24);
        let (rect, _) = model.library_hits[0];
        model.mouse(click(rect, MouseButton::Left));
        assert!(model.selected_game().unwrap().name.contains("number 5"));
        render(&mut model, 10, 4);
        assert!(model.library_hits.is_empty());
    }

    #[test]
    fn news_panel_scrolls_sanitizes_and_blocks_library_mouse_and_keys() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut model = model();
        render(&mut model, 80, 24);
        assert_eq!(
            model.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            Action::News
        );
        let request = model.news.as_ref().unwrap().request;
        assert!(render(&mut model, 80, 24).contains("Loading developer updates"));
        model.news.as_mut().unwrap().result = Some(Ok(vec![steam_core::news::NewsItem {
            title: "Patch title".into(),
            date: 1700000000,
            url: "https://example.invalid".into(),
            contents: format!(
                "[h1]Changes[/h1]\n[list][*]Fixed &amp; improved[/list]\n{}",
                "More details\n".repeat(100)
            ),
        }]));
        let output = render(&mut model, 80, 24);
        assert!(output.contains("Patch title"));
        assert!(output.contains("Fixed & improved"));
        assert!(!output.contains("[h1]"));
        model.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(model.news.as_ref().unwrap().offset, 10);
        let selected = model.selected;
        let (rect, _) = model.library_hits[1];
        model.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        model.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(model.selected, selected);
        model.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        model.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_ne!(model.news.as_ref().unwrap().request, request);
        model.news = None;
        model.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        model.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(model.news.is_none());
        assert_eq!(model.search, "n");
        assert_eq!(news_text("[beta] [b]ok[/b]\nnext\u{1b}"), "[beta] ok\nnext");
    }

    #[test]
    fn achievements_viewer_opens_sorts_filters_and_blocks_the_library() {
        use steam_core::achievements::{Achievement, Achievements};
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut model = model();
        render(&mut model, 100, 40);
        assert_eq!(model.key(key(KeyCode::Char('a'))), Action::Achievements);
        let request = model.achievements.as_ref().unwrap().request;
        assert!(render(&mut model, 100, 40).contains("Loading achievements"));
        let item =
            |name: &str, unlocked: bool, time: Option<u32>, percent: Option<f32>, hidden: bool| {
                Achievement {
                    name: name.into(),
                    display_name: format!("{name} title"),
                    description: format!("{name} description\u{1b}"),
                    hidden,
                    unlocked,
                    unlock_time: time,
                    percent,
                }
            };
        let view = model.achievements.as_mut().unwrap();
        view.result = Some(Ok(Achievements {
            items: vec![
                item("Rare", true, Some(1_700_000_000), Some(0.8), false),
                item("Common", false, None, Some(60.0), false),
                item("Secret", false, None, Some(2.0), true),
            ],
            has_percentages: true,
        }));
        view.reselect(None);
        let output = render(&mut model, 100, 40);
        assert!(output.contains("Achievements") && output.contains("1/3"));
        assert!(output.contains("Rare title") && output.contains("2023-11-14"));
        assert!(output.contains("Hidden achievement") && !output.contains("Secret description"));
        assert!(!output.contains('\u{1b}'));
        // Status order lists the unlocked one first; sorting by name flips the first row's owner.
        assert_eq!(
            model.achievements.as_ref().unwrap().visible()[0].name,
            "Rare"
        );
        model.key(key(KeyCode::Char('s')));
        assert_eq!(
            model.achievements.as_ref().unwrap().visible()[0].name,
            "Common"
        );
        assert!(render(&mut model, 100, 40).contains("sort name"));
        // The active sort's own number reverses it.
        model.key(key(KeyCode::Char('2')));
        assert_eq!(
            model.achievements.as_ref().unwrap().visible()[0].name,
            "Secret"
        );
        model.key(key(KeyCode::Char('1')));
        assert_eq!(
            model.achievements.as_ref().unwrap().visible()[0].name,
            "Rare"
        );
        model.key(key(KeyCode::Char('2')));
        model.key(key(KeyCode::Char('f')));
        assert_eq!(model.achievements.as_ref().unwrap().visible().len(), 1);
        // Library keys are blocked while it is open.
        let selected = model.selected;
        model.key(key(KeyCode::Down));
        model.key(key(KeyCode::Char('n')));
        assert_eq!(model.selected, selected);
        assert!(model.news.is_none());
        // Reload restarts the request; Esc closes.
        assert_eq!(model.key(key(KeyCode::Char('r'))), Action::Achievements);
        assert!(model.achievements.as_ref().unwrap().result.is_none());
        model.key(key(KeyCode::Esc));
        assert!(model.achievements.is_none());
        model.key(key(KeyCode::Char('a')));
        assert_ne!(model.achievements.as_ref().unwrap().request, request);
    }

    #[test]
    fn cloud_conflict_picker_requires_a_choice_and_confirmation_and_blocks_game_actions() {
        use steam_core::cloud::{Choice, Conflict, SaveVersion};
        let mut model = model();
        let game = model.selected_game().unwrap().installed.clone().unwrap();
        model.cloud_conflicts = Some(super::super::model::CloudConflicts {
            game,
            items: vec![Conflict {
                name: "slot.sav".into(),
                local: Some(SaveVersion {
                    sha1: "localhash".into(),
                    size: 12,
                    time: 100,
                }),
                cloud: Some(SaveVersion {
                    sha1: "cloudhash".into(),
                    size: 23,
                    time: 200,
                }),
            }],
            index: 0,
            choice: None,
            busy: false,
            error: None,
        });
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let output = render(&mut model, 100, 30);
        assert!(output.contains("Save conflict") && output.contains("slot.sav"));
        assert!(output.contains("localhash") && output.contains("cloudhash"));
        assert_eq!(model.key(key(KeyCode::Enter)), Action::None);
        assert_eq!(model.key(key(KeyCode::Char('i'))), Action::None);
        model.key(key(KeyCode::Char('l')));
        assert_eq!(
            model.cloud_conflicts.as_ref().unwrap().choice,
            Some(Choice::Local)
        );
        assert_eq!(model.key(key(KeyCode::Enter)), Action::ResolveCloud);
        assert!(model.cloud_conflicts.as_ref().unwrap().busy);
        assert_eq!(model.key(key(KeyCode::Char('r'))), Action::None);
        assert_eq!(
            model.cloud_conflicts.as_ref().unwrap().choice,
            Some(Choice::Local)
        );
        // While a resolution is running nothing, not even q, closes the modal.
        assert_eq!(model.key(key(KeyCode::Char('q'))), Action::None);
        assert!(model.cloud_conflicts.is_some());
        model.cloud_conflicts.as_mut().unwrap().busy = false;
        model.key(key(KeyCode::Esc));
        assert!(model.cloud_conflicts.is_none());
    }

    fn installing(model: &mut Model) {
        model.install_active = true;
        model.active_install = Some((1, PathBuf::from("/games")));
        model.apply_progress(InstallProgress {
            app_id: 1,
            phase: InstallPhase::Downloading,
            completed_bytes: 8 << 30,
            total_bytes: 30 << 30,
            downloaded_bytes: 5 << 30,
            total_download_bytes: 16 << 30,
            verify_bytes_done: 0,
            verify_bytes_total: 30 << 30,
            message: "depot 441: file maps/ctf_2fort.bsp".into(),
        });
    }

    #[test]
    fn backend_traffic_overlay_shows_direction_names_and_details() {
        use steam_core::backend::{Direction, Event};
        let mut model = model();
        let at = std::time::SystemTime::now();
        model.push_backend(Event {
            at,
            app_id: 440,
            direction: Direction::ToSteam,
            channel: "GC",
            title: "ClientHello (4006, protobuf)".into(),
            detail: "13 bytes · hex a60f0080 · body f1=1".into(),
        });
        model.push_backend(Event {
            at,
            app_id: 440,
            direction: Direction::FromSteam,
            channel: "GC",
            title: "ClientWelcome (4004, protobuf)".into(),
            detail: "17 bytes".into(),
        });
        model.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(model.backend_view);
        let screen = render(&mut model, 150, 40);
        for expected in [
            "Steam backend traffic",
            "game → Steam",
            "Steam → game",
            "ClientHello (4006, protobuf)",
            "ClientWelcome (4004, protobuf)",
            "body f1=1",
            "2 messages",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
        }
        model.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(model.backend_log.is_empty());
        model.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!model.backend_view);
    }

    #[test]
    fn status_line_shows_launch_mode_and_hotkeys_at_a_typical_width() {
        let mut model = model();
        model.account = Some("alice".into());
        model.status = "Library refreshed: 559 games; 2 scan warnings (? shows them)".into();
        model.set_catalog(CatalogSnapshot {
            items: vec![game(440, "Team Fortress 2", true)],
            warnings: vec![],
        });
        for id in [440, 3124540] {
            model.games[0].app_id = id;
            let screen = render(&mut model, 105, 30);
            let last = screen.lines().last().unwrap();
            assert!(
                (if id == 440 {
                    last.contains("Enter launch (Valve host)") && !last.contains("Ctrl+Enter")
                } else {
                    last.contains("Ctrl+Enter Valve host") && last.contains("Enter launch")
                }),
                "{last}"
            );
            assert!(!last.contains("^H"));
        }
    }

    #[test]
    fn repeated_identical_backend_messages_are_stacked() {
        use steam_core::backend::{Direction, Event};
        let mut model = model();
        let event = |title: &str| Event {
            at: std::time::SystemTime::now(),
            app_id: 440,
            direction: Direction::ToSteam,
            channel: "GC",
            title: title.into(),
            detail: "13 bytes".into(),
        };
        for _ in 0..5 {
            model.push_backend(event("ClientHello (4006, protobuf) · backend nudge"));
        }
        model.push_backend(event("ClientWelcome (4004, protobuf)"));
        model.push_backend(event("ClientHello (4006, protobuf) · backend nudge"));
        assert_eq!(
            model.backend_log.len(),
            3,
            "only consecutive duplicates stack"
        );
        assert_eq!(model.backend_log[0].count, 5);
        model.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        let screen = render(&mut model, 150, 40);
        assert!(screen.contains("×5"), "{screen}");
        assert!(screen.contains("7 messages"), "{screen}");
    }

    #[test]
    fn valve_host_game_names_stay_orange_selected_unselected_pinned_and_uninstalled() {
        let mut model = model();
        let names = [
            "Team Fortress 2",
            "Dota 2",
            "Counter-Strike 2",
            "Deadlock",
            "Far Far West",
        ];
        model.set_catalog(CatalogSnapshot {
            items: vec![
                game(440, names[0], true),
                game(570, names[1], false),
                game(730, names[2], true),
                game(1422450, names[3], false),
                game(3124540, names[4], true),
            ],
            warnings: vec![],
        });
        model.pins.insert(440);
        model.order = (0..5).collect();
        for selected in 0..5 {
            model.selected = selected;
            let mut terminal = Terminal::new(TestBackend::new(150, 40)).unwrap();
            terminal.draw(|frame| draw(frame, &mut model)).unwrap();
            let buffer = terminal.backend().buffer();
            for (index, name) in names.iter().enumerate() {
                let color = (0..40)
                    .find_map(|y| {
                        let row: String = (0..150)
                            .map(|x| buffer[(x, y)].symbol().to_owned())
                            .collect();
                        row.find(name)
                            .map(|at| buffer[(row[..at].chars().count() as u16, y)].fg)
                    })
                    .expect("game name rendered");
                if index < 4 {
                    assert_eq!(
                        color,
                        theme::VALVE_HOST_COLOR,
                        "{name}, selected {selected}"
                    );
                } else {
                    assert_ne!(color, theme::VALVE_HOST_COLOR);
                }
            }
        }
    }

    /// `cargo test -p steam-tui print_screens -- --ignored --nocapture` prints the
    /// main layouts as text for eyeballing border and label changes.
    #[test]
    #[ignore]
    fn print_screens() {
        let mut model = model();
        installing(&mut model);
        with_feed(&mut model);
        for (width, height) in [(110, 30), (70, 22)] {
            println!("{width}x{height}\n{}", render(&mut model, width, height));
        }
        model.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        println!("help\n{}", render(&mut model, 100, 30));
    }

    #[test]
    fn renders_without_panicking_at_many_sizes() {
        for (width, height) in [
            (0, 0),
            (1, 1),
            (20, 5),
            (39, 40),
            (40, 10),
            (60, 14),
            (80, 24),
            (110, 12),
            (110, 40),
            (150, 40),
            (200, 60),
        ] {
            for active in [false, true] {
                for editing in [false, true] {
                    let mut model = model();
                    if active {
                        installing(&mut model);
                    }
                    if editing {
                        with_feed(&mut model);
                    }
                    model.searching = editing;
                    model.focus = if active {
                        Focus::Notifications
                    } else {
                        Focus::Library
                    };
                    model.help = width % 2 == 0 && editing;
                    render(&mut model, width, height);
                }
            }
            render(&mut Model::new(true, None), width, height);
        }
    }

    #[test]
    fn library_panel_shows_sort_keys_filter_and_counts() {
        let mut model = model();
        let screen = render(&mut model, 80, 24);
        for expected in [
            "Library",
            "sort: name ▲",
            "view: all",
            "1/60",
            "/ to filter",
            "Name¹▲",
            "Size²",
            "Played⁴",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
        }
        assert!(screen.contains('╭') && screen.contains('╯'));
        model.sort = Sort::Size;
        model.reverse = true;
        assert!(render(&mut model, 80, 24).contains("Size²▼"));
    }

    #[test]
    fn pinned_games_are_starred_above_a_divider_and_selection_stays_visible() {
        let mut model = model();
        let screen = render(&mut model, 80, 24);
        assert!(!screen.contains('★'));
        let last = model.games[*model.order.last().unwrap()].app_id;
        model.pins.insert(last);
        model.sort = Sort::Name;
        model.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        model.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        let screen = render(&mut model, 80, 24);
        let star = screen.lines().position(|l| l.contains('★')).expect("star");
        let rule = screen
            .lines()
            .position(|l| l.starts_with('│') && l.contains("──────────"))
            .expect("divider");
        assert!(star < rule, "{screen}");
        for _ in 0..59 {
            model.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let screen = render(&mut model, 80, 24);
        assert!(screen.lines().any(|l| l.contains('›')), "{screen}");
    }

    #[test]
    fn filter_text_is_shown_inside_the_library_panel() {
        let mut model = model();
        model.search = "number 1".into();
        model.searching = true;
        let screen = render(&mut model, 120, 40);
        assert!(screen.contains("/ filter: number 1▏"), "{screen}");
        assert!(!screen.contains("to filter"));
    }

    #[test]
    fn install_panel_shows_progress_text_beside_a_bar() {
        let mut model = model();
        installing(&mut model);
        let screen = render(&mut model, 80, 24);
        for expected in [
            "Install · Game number 0 [1]",
            "Download",
            "26%",
            "verified 8.0 GiB / 30.0 GiB",
            "downloaded 5.0 GiB / 16.0 GiB",
            "↓ --",
            "ETA --",
            "elapsed",
            "ctf_2fort.bsp",
            "cancel",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
        }
        let bar_line = screen
            .lines()
            .find(|line| line.contains("Download"))
            .unwrap();
        assert!(bar_line.contains('█') && bar_line.contains('░'));
        assert!(!bar_line.contains("verified"));
    }

    #[test]
    fn bar_is_exact_width_and_text_free() {
        for (ratio, filled) in [(0.0, 0), (0.5, 5), (1.0, 10), (2.0, 10), (-1.0, 0)] {
            let segments = bar_segments(ratio, 10, Color::Cyan);
            assert_eq!(segments[0].0.chars().count(), filled);
            assert_eq!(
                segments[0].0.chars().count() + segments[1].0.chars().count(),
                10
            );
            assert!(
                segments
                    .iter()
                    .all(|(text, _)| text.chars().all(|c| c == '█' || c == '░'))
            );
        }
    }

    #[test]
    fn durations_and_rates_are_compact() {
        assert_eq!(duration_text(Duration::from_secs(45)), "45s");
        assert_eq!(duration_text(Duration::from_secs(192)), "3m 12s");
        assert_eq!(duration_text(Duration::from_secs(3900)), "1h 05m");
        assert_eq!(duration_text(Duration::from_secs(90_000)), "1d 1h");
        assert_eq!(eta_text(None), "--");
        assert_eq!(rate_text(None), "--");
        assert_eq!(rate_text(Some(14.6 * 1024.0 * 1024.0)), "14.6 MiB/s");
    }

    #[test]
    fn optional_columns_disappear_before_the_name_is_squeezed() {
        let (wide, name) = library_columns(100);
        assert_eq!(wide.len(), 7);
        assert!(name > NAME_MIN);
        let (narrow, _) = library_columns(45);
        assert!(!narrow.contains(&Col::Last) && narrow.contains(&Col::Name));
        let (tiny, name) = library_columns(10);
        assert!(tiny.contains(&Col::Name) && name >= 1);
    }

    #[test]
    fn cloud_column_follows_os_and_is_dropped_before_last_and_played() {
        let (all, _) = library_columns(100);
        assert_eq!(all[..2], [Col::Os, Col::Cloud]);
        for width in 0..140 {
            let (columns, _) = library_columns(width);
            if columns.contains(&Col::Cloud) {
                for kept in [Col::Last, Col::Played, Col::State, Col::Size, Col::Os] {
                    assert!(columns.contains(&kept), "{width}: {kept:?} before cloud");
                }
            }
        }
        // There is a width where the cloud column is gone but Last still fits.
        let both = (0..140).any(|w| {
            let (columns, _) = library_columns(w);
            !columns.contains(&Col::Cloud) && columns.contains(&Col::Last)
        });
        assert!(both);
    }

    #[test]
    fn cloud_column_renders_each_state_only_for_installed_cloud_games() {
        use crate::tui::model::CloudResult;
        use steam_core::cloud::CloudBadge;
        let mut model = Model::new(true, None);
        let mut games: Vec<CatalogItem> = (1..=6)
            .map(|id| game(id, &format!("Game {id}"), true))
            .collect();
        games.push(game(7, "Game 7 uninstalled", false));
        model.set_catalog(CatalogSnapshot {
            items: games,
            warnings: Vec::new(),
        });
        for (id, badge) in [
            (1, CloudBadge::Never),
            (2, CloudBadge::Synced),
            (3, CloudBadge::Pending),
            (4, CloudBadge::Synced),
            (5, CloudBadge::Synced),
            (7, CloudBadge::Synced),
        ] {
            model.cloud_badges.insert(id, badge);
        }
        // 4: last sync failed, 5: syncing; 6 has no cloud config; 7 is not installed.
        model.cloud_last.insert(4, CloudResult::Failed);
        model.cloud_busy.insert(5);
        let screen = render(&mut model, 100, 30);
        // Border, 2-cell highlight symbol, 3-cell OS badge and a gap precede the cloud cell.
        let cell = |name: &str| -> String {
            let line = screen.lines().find(|l| l.contains(name)).unwrap();
            line.chars().nth(7).map_or(String::new(), |c| c.to_string())
        };
        assert_eq!(cell("Game 1 "), "☁");
        assert_eq!(cell("Game 2 "), "☁");
        assert_eq!(cell("Game 3 "), "↑");
        assert_eq!(cell("Game 4 "), "✗");
        assert!(["◐", "◓", "◑", "◒"].contains(&cell("Game 5 ").as_str()));
        for blank in ["Game 6 ", "Game 7 "] {
            let line = screen.lines().find(|l| l.contains(blank)).unwrap();
            assert!(!line.contains(['☁', '↑', '✗']), "{line}");
        }
        assert!(
            screen.lines().any(|l| l.contains("OS  ☁ State")),
            "{screen}"
        );
    }

    fn with_feed(model: &mut Model) {
        let now = Local::now().timestamp() as u64;
        model.notification_event(NotificationEvent::Counts(vec![
            (CountKind::TradeOffers, 1),
            (CountKind::CommunityMessages, 2),
        ]));
        for (id, label, title, body) in [
            (1, "Trade offer", "Trade offer", "From a friend"),
            (2, "Wishlist", "Half price", "Game number 3 is on sale"),
            (3, "Chat message", "Chat message", ""),
        ] {
            model.notification_event(NotificationEvent::Received(Notification {
                id,
                timestamp: now,
                label: label.into(),
                title: title.into(),
                body: body.into(),
                read: false,
            }));
        }
    }

    #[test]
    fn wide_layout_never_makes_the_library_shorter_than_its_neighbours() {
        let wide = areas(Rect::new(0, 0, 150, 40), false, 0);
        assert!(wide.library.width <= 150 * 60 / 100, "{:?}", wide.library);
        let (friends, notifications) = (wide.friends.unwrap(), wide.notifications.unwrap());
        assert!(friends.x >= wide.library.right() && notifications.x == friends.x);
        assert!(friends.height >= 15 && notifications.height >= 15);
        assert!(wide.library.height >= friends.height + notifications.height);
    }

    #[test]
    fn narrow_layout_gives_the_library_everything_but_a_small_fixed_lower_row() {
        // (width, height, install shown, library height, lower row height); status is 1 row.
        for (width, height, install, library, lower) in [
            (80, 24, false, 18, 5),
            (80, 24, true, 12, 5),
            (80, 45, false, 36, 8),
            (80, 45, true, 30, 8),
            (100, 40, false, 31, 8),
        ] {
            let layout = areas(Rect::new(0, 0, width, height), install, 0);
            let (friends, notifications) = (layout.friends.unwrap(), layout.notifications.unwrap());
            assert_eq!(layout.library.height, library, "{width}x{height} {install}");
            assert_eq!(friends.height, lower, "{width}x{height} {install}");
            assert_eq!(notifications.height, lower);
            // The lower row sits directly beneath the library.
            assert_eq!(friends.y, layout.library.bottom());
            assert_eq!(friends.y, notifications.y);
            assert!(layout.library.height > friends.height);
            assert_eq!(layout.status.y, height - 1);
            assert_eq!(layout.install.is_some(), install);
        }
    }

    #[test]
    fn notifications_are_hidden_before_friends_and_friends_before_the_library() {
        assert!(
            areas(Rect::new(0, 0, 150, 12), false, 0)
                .notifications
                .is_none()
        );
        assert!(areas(Rect::new(0, 0, 150, 12), false, 0).friends.is_some());
        let narrow = areas(Rect::new(0, 0, 50, 30), false, 0);
        assert!(narrow.notifications.is_none() && narrow.friends.is_some());
        let tiny = areas(Rect::new(0, 0, 40, 12), false, 0);
        assert!(tiny.friends.is_none() && tiny.notifications.is_none());
        assert!(tiny.library.height >= MIN_MAIN_HEIGHT);
    }

    #[test]
    fn notifications_panel_shows_counts_feed_newest_first_and_unseen_marker() {
        let mut model = model();
        model.notifications_live = true;
        model.notif_status.clear();
        let empty = render(&mut model, 150, 40);
        assert!(empty.contains("No recent notifications — new ones appear live"));
        assert!(empty.contains("nothing pending"));
        with_feed(&mut model);
        let screen = render(&mut model, 150, 40);
        for expected in [
            "Notifications 3",
            "⇄ 1 trade offers",
            "✉ 2 community messages",
            "Trade offer",
            "Half price",
            "Game number 3 is on sale",
            "PgUp/PgDn scroll",
            "● ",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
        }
        let line = |needle: &str| screen.lines().position(|l| l.contains(needle)).unwrap();
        assert!(line("Chat message") < line("Half price"));
        assert!(line("Half price") < line("From a friend"));
        // Focusing the feed marks everything seen.
        model.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        model.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let focused = render(&mut model, 150, 40);
        assert!(model.notif_unseen == 0 && !focused.contains("Notifications 3"));
        // And the same data renders at the stacked size.
        assert!(render(&mut model, 80, 24).contains("Notifications"));
    }

    #[test]
    fn notifications_panel_reports_offline_and_errors() {
        let mut offline = Model::new(true, None);
        let screen = render(&mut offline, 150, 40);
        assert!(screen.contains("Offline — no Steam notifications"));
        assert!(!screen.contains("appear live"));
        let mut failing = model_online();
        failing.notification_event(NotificationEvent::Status(
            "Steam notification stream ended".into(),
        ));
        let screen = render(&mut failing, 150, 40);
        assert!(screen.contains("Steam notification stream ended"));
    }

    fn model_online() -> Model {
        let mut model = Model::new(false, None);
        model.set_catalog(CatalogSnapshot {
            items: vec![game(1, "Game number 0", true)],
            warnings: Vec::new(),
        });
        model
    }

    #[test]
    fn status_shows_running_game_independent_of_selection_and_returns_online() {
        let mut model = model_online();
        model.account = Some("alice".into());
        model.games.push(game(3124540, "Far Far West", true));
        model.running_games.insert(3124540);
        assert!(render(&mut model, 150, 40).contains("alice · in game: Far Far West"));
        model.notifications_live = false;
        assert!(render(&mut model, 150, 40).contains("Far Far West (offline)"));
        model.notifications_live = true;
        model.running_games.clear();
        let screen = render(&mut model, 150, 40);
        assert!(screen.contains("alice · ● online"));
        assert!(!screen.contains("in game:"));
    }

    #[test]
    fn server_text_cannot_inject_terminal_sequences() {
        let mut model = model_online();
        model.notification_event(NotificationEvent::Received(Notification {
            id: 9,
            timestamp: 1_700_000_000,
            label: "Gift\u{1b}[2J".into(),
            title: "T\u{9b}itle\u{202e}".into(),
            body: "body\u{7}\u{1b}]0;evil\u{7}".into(),
            read: false,
        }));
        model.notification_event(NotificationEvent::Status("bad\u{1b}[H".into()));
        let screen = render(&mut model, 150, 40);
        assert!(
            !screen
                .chars()
                .any(|ch| ch == '\u{1b}' || ch == '\u{7}' || ch == '\u{9b}')
        );
        assert!(screen.contains("Gift[2J") && screen.contains("Title"));
    }

    #[test]
    fn clock_shows_time_today_and_date_otherwise() {
        let then = Local.timestamp_opt(1_700_000_000, 0).unwrap();
        let same_day = then + chrono::Duration::seconds(30);
        assert_eq!(
            clock(1_700_000_000, same_day),
            then.format("%H:%M").to_string()
        );
        let later = then + chrono::Duration::days(10);
        assert_eq!(
            clock(1_700_000_000, later),
            then.format("%m-%d").to_string()
        );
        assert_eq!(clock(u64::MAX, same_day), "--:--");
    }

    #[test]
    fn blocked_apps_are_hidden_from_the_library() {
        let mut model = model();
        let mut blocked = game(500, "Aaa blocked", false);
        blocked.unavailable_reason = Some("no usable depot".into());
        let mut items = model.games.clone();
        items.push(blocked);
        model.set_catalog(CatalogSnapshot {
            items,
            warnings: Vec::new(),
        });
        model.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let screen = render(&mut model, 120, 30);
        assert!(
            !screen.contains("Aaa blocked") && !screen.contains("blocked"),
            "{screen}"
        );
        assert!(screen.contains("1/60"), "{screen}");
    }

    fn p() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)
    }

    #[test]
    fn library_state_column_shows_install_percent_and_pause() {
        let mut model = model();
        let idle = render(&mut model, 80, 24);
        assert!(!idle.contains("download 26%") && !idle.contains('⏸'));
        installing(&mut model);
        let running = render(&mut model, 80, 24);
        let row = running
            .lines()
            .find(|line| line.contains("Game number 0 "))
            .unwrap();
        assert!(row.contains("download 26%"), "{row}");
        assert!(row.contains("5.0 GiB"), "{row}");
        assert!(running.contains("pause"));

        assert_eq!(model.key(p()), Action::TogglePause);
        let paused = render(&mut model, 80, 24);
        let row = paused
            .lines()
            .find(|line| line.contains("Game number 0 "))
            .unwrap();
        assert!(row.contains("paused 26%"), "{row}");
        for expected in ["⏸ Download", "resume", "↓ paused", "ETA --"] {
            assert!(paused.contains(expected), "missing {expected:?}\n{paused}");
        }

        model.key(p());
        let resumed = render(&mut model, 80, 24);
        assert!(resumed.contains("Download") && !resumed.contains("⏸ Download"));

        model.install_active = false;
        model.active_install = None;
        model.complete_install();
        let done = render(&mut model, 80, 24);
        assert!(!done.contains("download 26%") && done.contains("Done"));
    }

    #[test]
    fn state_column_before_the_first_sample_and_while_cancelling() {
        let mut model = model();
        model.install_active = true;
        model.active_install = Some((1, PathBuf::from("/games")));
        model.begin_install();
        assert!(render(&mut model, 80, 24).contains("preparing"));
        model.key(p());
        let paused = render(&mut model, 80, 24);
        assert!(paused.contains("paused") && paused.contains("Paused before the download"));
        model.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(render(&mut model, 80, 24).contains("cancel…"));
    }

    fn verifying(model: &mut Model, done: u64) {
        model.apply_progress(InstallProgress {
            app_id: 1,
            phase: InstallPhase::Verifying,
            completed_bytes: 30 << 30,
            total_bytes: 30 << 30,
            downloaded_bytes: 2 << 30,
            total_download_bytes: 2 << 30,
            verify_bytes_done: done,
            verify_bytes_total: 30 << 30,
            message: "3/9 files".into(),
        });
    }

    #[test]
    fn state_words_are_short_and_distinguish_update_status() {
        let mut item = game(7, "Seven", false);
        assert_eq!(theme::state_tag(&item).0, "new");
        item.unavailable_reason = Some("region locked".into());
        assert_eq!(theme::state_tag(&item).0, "blocked");
        let mut item = game(7, "Seven", true);
        for (update, word) in [
            (UpdateStatus::UpToDate, "installed"),
            (UpdateStatus::Unknown, "installed"),
            (
                UpdateStatus::Available {
                    installed_build: 1,
                    latest_build: 2,
                },
                "update",
            ),
            (UpdateStatus::Unavailable("gone".into()), "blocked"),
        ] {
            item.update = update;
            let (label, _) = theme::state_tag(&item);
            assert_eq!(label, word);
            assert!(label.chars().count() <= 13);
        }
    }

    #[test]
    fn install_phase_shows_a_calm_message_and_says_cancel_waits() {
        let mut model = model();
        installing(&mut model);
        model.apply_progress(InstallProgress {
            app_id: 1,
            phase: InstallPhase::Committing,
            completed_bytes: 30 << 30,
            total_bytes: 30 << 30,
            downloaded_bytes: 16 << 30,
            total_download_bytes: 16 << 30,
            verify_bytes_done: 30 << 30,
            verify_bytes_total: 30 << 30,
            message: "Moving files into place 42%".into(),
        });
        let screen = render(&mut model, 100, 30);
        for expected in [
            "Install     100%",
            "cancel waits for install",
            "Moving files into place 42%",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
        }
        let row = screen
            .lines()
            .find(|line| line.contains("Game number 0 "))
            .unwrap();
        assert!(row.contains("install 100%"), "{row}");
        let title = screen
            .lines()
            .find(|line| line.contains("Install · Game number 0"))
            .unwrap();
        assert!(
            !title.contains("pause") && !title.contains("cancel╭"),
            "{title}"
        );
    }

    #[test]
    fn verification_shows_its_own_bar_percent_speed_and_state_glyph() {
        let mut model = model();
        installing(&mut model);
        verifying(&mut model, 10 << 30);
        let screen = render(&mut model, 100, 30);
        for expected in [
            "Verify",
            "33%",
            "hashed 10.0 GiB / 30.0 GiB",
            "⟳ --",
            "ETA --",
            "3/9 files",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}\n{screen}");
        }
        let row = screen
            .lines()
            .find(|line| line.contains("Game number 0 "))
            .unwrap();
        assert!(row.contains("verify 33%"), "{row}");
        let bar = screen
            .lines()
            .find(|line| line.contains("Verify") && line.contains("33%") && line.contains('█'))
            .unwrap();
        assert!(bar.contains('█') && bar.contains('░'), "{bar}");
        // Pause stays refused outside the download.
        assert_eq!(model.key(p()), Action::None);
        assert!(!model.paused);
        assert_eq!(model.status, "Pause only applies while downloading");
    }

    /// Draws a mock-data session for the README. Run with
    /// `STEAM_TUI_SCREENSHOT=out.svg cargo test -p steam-tui readme_screenshot -- --ignored`
    /// and convert with `rsvg-convert -z 2 out.svg -o assets/screenshot.png`.
    /// Set `STEAM_TUI_SCREENSHOT_THEME` to a terminal theme (an Omarchy `alacritty.toml`) to colour it.
    #[test]
    #[ignore = "writes a file; run by hand"]
    fn readme_screenshot() {
        use ratatui::style::{Color, Modifier};
        use steam_core::catalog::{InstallPlatform, UpdateStatus};
        use steam_core::friends::{Friend, Presence};
        use steam_core::install::InstallPhase;
        use steam_core::notifications::Notification;

        let Some(path) = std::env::var_os("STEAM_TUI_SCREENSHOT") else {
            return;
        };
        let mut model = Model::new(false, None);
        model.account = Some("alice".into());
        // (id, name, installed, platform, update, GiB, played minutes)
        let library: [(u32, &str, bool, InstallPlatform, bool, u64, u32); 16] = [
            (
                730,
                "Counter-Strike 2",
                true,
                InstallPlatform::Linux,
                false,
                33,
                41_210,
            ),
            (
                620,
                "Portal 2",
                true,
                InstallPlatform::Linux,
                false,
                12,
                1_130,
            ),
            (
                440,
                "Team Fortress 2",
                true,
                InstallPlatform::Linux,
                true,
                26,
                9_480,
            ),
            (
                632360,
                "Risk of Rain 2",
                true,
                InstallPlatform::Windows,
                false,
                8,
                2_960,
            ),
            (
                413150,
                "Stardew Valley",
                true,
                InstallPlatform::Linux,
                false,
                1,
                3_720,
            ),
            (
                1145360,
                "Hades",
                true,
                InstallPlatform::Windows,
                false,
                15,
                2_410,
            ),
            (
                427520,
                "Factorio",
                true,
                InstallPlatform::Linux,
                false,
                2,
                15_800,
            ),
            (
                504230,
                "Celeste",
                true,
                InstallPlatform::Linux,
                false,
                1,
                640,
            ),
            (
                105600,
                "Terraria",
                true,
                InstallPlatform::Linux,
                false,
                1,
                5_390,
            ),
            (
                1086940,
                "Baldur's Gate 3",
                false,
                InstallPlatform::Windows,
                false,
                122,
                0,
            ),
            (
                1091500,
                "Cyberpunk 2077",
                false,
                InstallPlatform::Windows,
                false,
                70,
                0,
            ),
            (
                292030,
                "The Witcher 3: Wild Hunt",
                false,
                InstallPlatform::Windows,
                false,
                50,
                0,
            ),
            (
                367520,
                "Hollow Knight",
                false,
                InstallPlatform::Linux,
                false,
                9,
                0,
            ),
            (
                1245620,
                "ELDEN RING",
                false,
                InstallPlatform::Windows,
                false,
                60,
                0,
            ),
            (
                294100,
                "RimWorld",
                false,
                InstallPlatform::Linux,
                false,
                1,
                0,
            ),
            (
                255710,
                "Cities: Skylines",
                false,
                InstallPlatform::Linux,
                false,
                4,
                0,
            ),
        ];
        let items = library
            .iter()
            .enumerate()
            .map(
                |(index, &(id, name, installed, platform, update, gib, minutes))| {
                    let mut item = game(id, name, installed);
                    let last = 1_760_000_000 - index as u64 * 86_400 * 9;
                    item.size_bytes = Some(gib << 30);
                    item.last_played = (installed).then_some(last);
                    item.playtime_minutes = installed.then_some(minutes);
                    if let Some(installed) = &mut item.installed {
                        installed.platform = Some(platform);
                        installed.size_bytes = item.size_bytes;
                        installed.last_played = item.last_played;
                        installed.playtime_minutes = item.playtime_minutes;
                    }
                    item.update = if update {
                        UpdateStatus::Available {
                            installed_build: 1,
                            latest_build: 2,
                        }
                    } else if installed {
                        UpdateStatus::UpToDate
                    } else {
                        UpdateStatus::Unknown
                    };
                    item
                },
            )
            .collect();
        model.set_catalog(CatalogSnapshot {
            items,
            warnings: Vec::new(),
        });
        model.set_friends(
            [
                ("Mira", Presence::Online, Some("Hades")),
                ("Jonas", Presence::Online, Some("Factorio")),
                ("Priya", Presence::Online, None),
                ("Tomás", Presence::Away, None),
                ("Kenji", Presence::Busy, Some("Counter-Strike 2")),
                ("Lena", Presence::Offline, None),
                ("Sam", Presence::Offline, None),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (name, state, game))| Friend {
                steam_id: 76_561_198_000_000_000 + index as u64,
                name: name.into(),
                state,
                game: game.map(str::to_owned),
            })
            .collect(),
        );
        let now = 1_760_000_000;
        for (id, label, title, body) in [
            (1, "Trade offer", "1 trade offer", "Mira sent you an offer"),
            (2, "Chat message", "Jonas", "Factorio tonight?"),
            (3, "Gift", "Priya sent you a gift", "Celeste"),
        ] {
            model.notification_event(NotificationEvent::Received(Notification {
                id,
                timestamp: now - id * 1_300,
                label: label.into(),
                title: title.into(),
                body: body.into(),
                read: false,
            }));
        }
        model.install_active = true;
        model.active_install = Some((1086940, PathBuf::from("/games")));
        model.apply_progress(InstallProgress {
            app_id: 1086940,
            phase: InstallPhase::Downloading,
            completed_bytes: 51 << 30,
            total_bytes: 122 << 30,
            downloaded_bytes: 38 << 30,
            total_download_bytes: 91 << 30,
            verify_bytes_done: 0,
            verify_bytes_total: 122 << 30,
            message: "depot 1086941: Data/Generated/Public/Shared.pak".into(),
        });
        model.status = "Library refreshed: 559 games".into();

        // Optional: STEAM_TUI_SCREENSHOT_SIZE=WxH and STEAM_TUI_SCREENSHOT_VIEW=conflict|filtered
        let (width, height) = std::env::var("STEAM_TUI_SCREENSHOT_SIZE")
            .ok()
            .and_then(|size| {
                let (w, h) = size.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((132u16, 36u16));
        match std::env::var("STEAM_TUI_SCREENSHOT_VIEW").as_deref() {
            Ok("conflict") => {
                use steam_core::cloud::{Conflict, SaveVersion};
                let game = model.games[2].installed.clone().unwrap();
                model.cloud_conflicts = Some(super::super::model::CloudConflicts {
                    game,
                    items: vec![Conflict {
                        name: "%LinuxXdgDataHome%/tf/cfg/config.cfg".into(),
                        local: Some(SaveVersion {
                            sha1: "9f2c1ab04d7e5513".into(),
                            size: 18_432,
                            time: 1_759_900_000,
                        }),
                        cloud: Some(SaveVersion {
                            sha1: "3b7e90d1c4a8f620".into(),
                            size: 17_920,
                            time: 1_759_990_000,
                        }),
                    }],
                    index: 0,
                    choice: Some(steam_core::cloud::Choice::Cloud),
                    busy: false,
                    error: None,
                });
            }
            Ok("filtered") => {
                model.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
            }
            _ => {}
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, &mut model)).unwrap();
        let buffer = terminal.backend().buffer();

        // Colours come from a terminal theme so the picture matches the terminal it
        // is meant to show: STEAM_TUI_SCREENSHOT_THEME=path/to/alacritty.toml (an
        // Omarchy theme file). Without it the old Catppuccin palette is used.
        let palette: std::collections::HashMap<String, String> =
            std::env::var_os("STEAM_TUI_SCREENSHOT_THEME")
                .and_then(|path| std::fs::read_to_string(path).ok())
                .map(|text| {
                    let mut section = String::new();
                    let mut colors = std::collections::HashMap::new();
                    for line in text.lines().map(str::trim) {
                        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']'))
                        {
                            section = name.trim_start_matches("colors.").to_owned();
                        } else if let Some((key, value)) = line.split_once('=') {
                            let value = value.trim().trim_matches('"');
                            if value.starts_with('#') {
                                colors
                                    .insert(format!("{section}.{}", key.trim()), value.to_owned());
                            }
                        }
                    }
                    colors
                })
                .unwrap_or_default();
        let themed = |key: &str, fallback: &str| -> String {
            palette
                .get(key)
                .cloned()
                .unwrap_or_else(|| fallback.to_owned())
        };
        let (foreground, background) = (
            themed("primary.foreground", "#cdd6f4"),
            themed("primary.background", "#1e1e2e"),
        );
        let rgb = |color: Color, default: &str| -> String {
            match color {
                Color::Reset => default.into(),
                Color::Black => themed("normal.black", "#45475a"),
                Color::Red => themed("normal.red", "#f38ba8"),
                Color::Green => themed("normal.green", "#a6e3a1"),
                Color::Yellow => themed("normal.yellow", "#f9e2af"),
                Color::Blue => themed("normal.blue", "#89b4fa"),
                Color::Magenta => themed("normal.magenta", "#f5c2e7"),
                Color::Cyan => themed("normal.cyan", "#94e2d5"),
                Color::Gray => themed("normal.white", "#bac2de"),
                Color::DarkGray => themed("bright.black", "#6c7086"),
                Color::LightRed => themed("bright.red", "#f38ba8"),
                Color::LightGreen => themed("bright.green", "#a6e3a1"),
                Color::LightYellow => themed("bright.yellow", "#f9e2af"),
                Color::LightBlue => themed("bright.blue", "#89b4fa"),
                Color::LightMagenta => themed("bright.magenta", "#f5c2e7"),
                Color::LightCyan => themed("bright.cyan", "#94e2d5"),
                Color::White => themed("bright.white", "#cdd6f4"),
                Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                Color::Indexed(_) => default.into(),
            }
        };
        let (cw, ch, pad) = (8.4f32, 18.0f32, 14.0f32);
        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" font-family=\"JetBrainsMono Nerd Font, Noto Sans Symbols, monospace\" font-size=\"14\">\n<rect width=\"100%\" height=\"100%\" rx=\"10\" fill=\"{background}\"/>\n",
            w = width as f32 * cw + pad * 2.0,
            h = height as f32 * ch + pad * 2.0
        );
        for y in 0..height {
            for x in 0..width {
                let cell = &buffer[(x, y)];
                let style = cell.style();
                let reversed = style.add_modifier.contains(Modifier::REVERSED);
                let (fg, bg) = (rgb(cell.fg, &foreground), rgb(cell.bg, &background));
                let (fg, bg) = if reversed { (bg, fg) } else { (fg, bg) };
                let (px, py) = (pad + x as f32 * cw, pad + y as f32 * ch);
                if bg != background {
                    svg.push_str(&format!("<rect x=\"{px:.1}\" y=\"{py:.1}\" width=\"{cw:.1}\" height=\"{ch:.1}\" fill=\"{bg}\"/>\n"));
                }
                let symbol = cell.symbol();
                if symbol.trim().is_empty() {
                    continue;
                }
                let bold = if style.add_modifier.contains(Modifier::BOLD) {
                    " font-weight=\"bold\""
                } else {
                    ""
                };
                let dim = if style.add_modifier.contains(Modifier::DIM) {
                    " opacity=\"0.6\""
                } else {
                    ""
                };
                let text = symbol
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                svg.push_str(&format!(
                    "<text x=\"{px:.1}\" y=\"{:.1}\" fill=\"{fg}\"{bold}{dim}>{text}</text>\n",
                    py + 13.5
                ));
            }
        }
        svg.push_str("</svg>\n");
        std::fs::write(path, svg).unwrap();
    }
}

fn chat_modal(frame: &mut Frame<'_>, model: &mut Model) {
    let Some(friend) = model.chat.active else {
        return;
    };
    let name = model
        .friends
        .iter()
        .find(|f| f.steam_id == friend)
        .map(|f| f.name.clone())
        .unwrap_or_else(|| friend.to_string());
    let area = modal_area(frame.area(), 100, 32);
    open_modal(frame, area);
    let block = Block::bordered()
        .title(format!(" Chat · {} ", safe(&name)))
        .title_bottom(" Enter send · Ctrl+V paste · PgUp/PgDn history · Esc close ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let attached = model
        .chat
        .conversations
        .get(&friend)
        .is_some_and(|c| c.attachment.is_some());
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if attached { 6 } else { 0 }),
        Constraint::Length(2),
        Constraint::Length(4),
    ])
    .split(inner);
    model.chat.rows = chunks[0].height as usize;
    let c = model.chat.conversations.entry(friend).or_default();
    let mut lines = Vec::new();
    let mut previews = Vec::new();
    let width = usize::from(chunks[0].width).max(1);
    let preview_size = (chunks[0].width.clamp(1, 80), chunks[0].height.clamp(1, 12));
    if c.more {
        lines.push(Line::styled("PgUp loads older messages", dim()));
    }
    for message in &c.messages {
        let time = DateTime::from_timestamp(i64::from(message.timestamp), 0)
            .map(|t| t.with_timezone(&Local).format("%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let sender = if message.outgoing { "You" } else { &name };
        lines.extend(wrap_segments(
            &[seg(
                format!("{time} {sender}"),
                if message.outgoing { accent() } else { bold() },
            )],
            width,
        ));
        for line in message.text.split('\n') {
            lines.extend(wrap_segments(&[seg(safe(line), Style::default())], width));
        }
        for url in super::chat_images::links(&message.text) {
            previews.push((url, lines.len()));
            lines.extend((0..preview_size.1).map(|_| Line::raw("")));
        }
        lines.push(Line::raw(""));
    }
    if c.loading {
        lines.push(Line::styled("Loading messages…", dim()));
    }
    if lines.is_empty() {
        lines.push(Line::styled("No messages yet", dim()));
    }
    let total = lines.len();
    let paragraph = Paragraph::new(lines);
    let max_offset = total.saturating_sub(chunks[0].height as usize);
    model.chat.offset = model.chat.offset.min(max_offset);
    let start = max_offset
        .saturating_sub(model.chat.offset)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(paragraph.scroll((start, 0)), chunks[0]);
    for (url, row) in previews {
        let top = row.max(usize::from(start));
        let bottom = (row + usize::from(preview_size.1))
            .min(usize::from(start) + usize::from(chunks[0].height));
        if top >= bottom {
            continue;
        }
        model.chat.images.prepare(&url, preview_size);
        let area = Rect::new(
            chunks[0].x,
            chunks[0].y + (top - usize::from(start)) as u16,
            preview_size.0.min(chunks[0].width),
            (bottom - top) as u16,
        );
        if !model.chat.images.render(
            &url,
            area,
            (top - row) as u16,
            preview_size.1,
            frame.buffer_mut(),
        ) {
            frame.render_widget(
                Paragraph::new(model.chat.images.status(&url)).style(dim()),
                area,
            );
        }
    }

    if let Some(attachment) = &c.attachment {
        let key = format!("clipboard:{friend}:{}", attachment.sha);
        let size = (chunks[1].width.clamp(1, 50), chunks[1].height.max(1));
        model
            .chat
            .images
            .prepare_local(&key, attachment.image.clone(), size);
        if !model
            .chat
            .images
            .render(&key, chunks[1], 0, chunks[1].height, frame.buffer_mut())
        {
            frame.render_widget(
                Paragraph::new("Preparing attachment preview…").style(dim()),
                chunks[1],
            );
        }
    }
    let status = if !c.error.is_empty() {
        c.error.clone()
    } else if !model.chat.status.is_empty() {
        model.chat.status.clone()
    } else if c.clipboard_pending {
        "Reading clipboard…".into()
    } else if c.sending {
        if c.attachment.is_some() {
            "Uploading image…".into()
        } else {
            "Sending…".into()
        }
    } else if !c.notice.is_empty() {
        c.notice.clone()
    } else if super::chat::typing(c) {
        format!("{name} is typing…")
    } else {
        format!(
            "{} / {} bytes",
            c.draft.len(),
            steam_core::chat::MAX_MESSAGE_BYTES
        )
    };
    frame.render_widget(
        Paragraph::new(safe(&status)).wrap(Wrap { trim: false }),
        chunks[2],
    );
    let composer = format!("{}▏{}", &c.draft[..c.cursor], &c.draft[c.cursor..]);
    let input_lines: Vec<_> = composer
        .split('\n')
        .flat_map(|line| {
            wrap_segments(
                &[seg(safe(line), Style::default())],
                usize::from(chunks[3].width.saturating_sub(2)).max(1),
            )
        })
        .collect();
    let before_cursor = format!("{}▏", &c.draft[..c.cursor]);
    let cursor_lines: usize = before_cursor
        .split('\n')
        .map(|line| {
            wrap_segments(
                &[seg(safe(line), Style::default())],
                usize::from(chunks[3].width.saturating_sub(2)).max(1),
            )
            .len()
        })
        .sum();
    let scroll = cursor_lines
        .saturating_sub(usize::from(chunks[3].height.saturating_sub(2)))
        .min(u16::MAX as usize) as u16;
    let input = Paragraph::new(input_lines)
        .scroll((scroll, 0))
        .block(Block::bordered().title(" Message "));
    frame.render_widget(input, chunks[3]);
}

#[cfg(test)]
mod chat_render_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use steam_core::{
        chat::{Event, Message},
        friends::{Friend, Presence},
    };

    fn screen(model: &mut Model, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, model)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn unread_selection_and_chat_modal_render_without_a_real_terminal() {
        let mut m = Model::new(false, None);
        m.set_friends(vec![Friend {
            steam_id: 10,
            name: "Alice".into(),
            state: Presence::Online,
            game: None,
        }]);
        m.focus = Focus::Friends;
        m.chat_event(Event::Message {
            friend: 10,
            message: Message {
                timestamp: 100,
                ordinal: 0,
                outgoing: false,
                text: "hello".into(),
            },
        });
        let text = screen(&mut m, 150, 40);
        assert!(text.contains("[1]"));
        assert!(text.contains("› ● Alice"));
        m.open_chat();
        m.chat_paste("some draft");
        let text = screen(&mut m, 120, 40);
        assert!(text.contains("Chat · Alice"));
        assert!(text.contains("hello"));
        assert!(text.contains("some draft▏"));
        for (width, height) in [(48, 14), (12, 5), (1, 1)] {
            screen(&mut m, width, height);
        }
    }

    #[test]
    fn long_messages_wrap_and_the_latest_message_remains_visible() {
        let mut m = Model::new(false, None);
        m.set_friends(vec![Friend {
            steam_id: 10,
            name: "Alice".into(),
            state: Presence::Online,
            game: None,
        }]);
        m.open_chat();
        let messages = (0..100)
            .map(|i| Message {
                timestamp: 100 + i,
                ordinal: 0,
                outgoing: false,
                text: if i == 99 {
                    "LATEST MESSAGE".into()
                } else {
                    "long wrapped text ".repeat(50)
                },
            })
            .collect();
        m.chat_event(Event::History {
            friend: 10,
            messages,
            more: false,
            older: false,
        });
        let text = screen(&mut m, 70, 24);
        assert!(text.contains("LATEST MESSAGE"));
    }
}
