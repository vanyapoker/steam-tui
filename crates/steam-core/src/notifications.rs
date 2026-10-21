//! Recent and live Steam notifications for the signed-in account.
//!
//! Sources (all pushed by Steam over the existing CM session, no polling):
//! - `CMsgClientUserNotifications`: per-type counts. Steam omits a type to mean
//!   zero, so each message replaces every count it can carry.
//! - `CMsgClientCommentNotifications` / `CMsgClientItemAnnouncements`: counts
//!   for comments and new items; requested once at startup.
//! - `SteamNotificationClient.NotificationsReceived#1` service notification:
//!   individual notifications plus pending gift/friend/family-invite counts.
//!
//! History is requested once per connection through the notification service,
//! including read entries. Subsequent pushes update the same feed.
//!
//! Everything here is server-controlled text headed for a terminal, so it is
//! sanitized and bounded before leaving this module. Nothing is logged.
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::error::Error;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use steam_vent::{Connection, ConnectionTrait, ReceivableMessage};
use steam_vent_proto_steam::steammessages_clientserver_2::{
    CMsgClientCommentNotifications, CMsgClientItemAnnouncements,
    CMsgClientRequestCommentNotifications, CMsgClientRequestItemAnnouncements,
    CMsgClientUserNotifications,
};
use steam_vent_proto_steam::steammessages_notifications_steamclient::{
    CSteamNotification_NotificationsReceived_Notification, SteamNotificationData,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

mod history;

const MAX_RAW_BYTES: usize = 8 * 1024 * 1024;
const TITLE_CHARS: usize = 80;
const BODY_CHARS: usize = 300;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Decode failures are reported a few times, then ignored, so a bad stream
/// cannot flood the UI.
const MAX_REPORTED_ERRORS: u32 = 3;

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

/// What a notification count is about.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum CountKind {
    TradeOffers,
    CommunityMessages,
    Comments,
    Items,
    Gifts,
    FriendInvites,
    FamilyInvites,
    /// A `CMsgClientUserNotifications` type this program has no name for.
    Other(u32),
}

impl CountKind {
    /// `user_notification_type` values with a known meaning: 1 trade offers and
    /// 3 community messages (as handled by node-steam-user); others stay anonymous.
    fn from_user_type(value: u32) -> Self {
        match value {
            1 => Self::TradeOffers,
            3 => Self::CommunityMessages,
            other => Self::Other(other),
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::TradeOffers => "Trade offers".into(),
            Self::CommunityMessages => "Community messages".into(),
            Self::Comments => "Comments".into(),
            Self::Items => "Items".into(),
            Self::Gifts => "Gifts".into(),
            Self::FriendInvites => "Friend invites".into(),
            Self::FamilyInvites => "Family invites".into(),
            Self::Other(value) => format!("Notifications (type {value})"),
        }
    }
}

/// One pushed notification, already safe to print.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Notification {
    pub id: u64,
    /// Unix seconds.
    pub timestamp: u64,
    pub label: String,
    pub title: String,
    pub body: String,
    pub read: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum NotificationEvent {
    /// Complete non-zero count summary; replaces the previous one.
    Counts(Vec<(CountKind, u32)>),
    Received(Notification),
    /// Existing account history; does not create live/unseen badges.
    History(Vec<Notification>),
    Removed(u64),
    Status(String),
}

/// Human label for `ESteamNotificationType` values.
pub fn type_label(value: i32) -> String {
    match value {
        1 => "Test",
        2 => "Gift",
        3 => "Comment",
        4 => "Item",
        5 => "Friend invite",
        6 => "Major sale",
        7 => "Preload available",
        8 => "Wishlist",
        9 => "Trade offer",
        10 => "General",
        11 => "Help request",
        12 => "Async game",
        13 => "Chat message",
        14 => "Moderator message",
        15 => "Parental access request",
        16 => "Family invite",
        17 => "Family purchase request",
        18 => "Parental playtime request",
        19 => "Family purchase response",
        20 => "Parental access response",
        21 => "Parental playtime response",
        22 => "Requested game added",
        23 => "Clip downloaded",
        24 => "Send to phone",
        25 => "2FA prompt",
        26 => "Mobile confirmation",
        27 => "Partner event",
        28 => "Playtest invite",
        29 => "Trade reversal",
        30 => "Content report",
        other => return format!("Notification #{other}"),
    }
    .to_owned()
}

fn unsafe_character(ch: char) -> bool {
    ch.is_control()
        || matches!(ch,
            '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' |
            '\u{2060}'..='\u{206f}' | '\u{feff}'
        )
}

/// Removes control characters (including ESC and C1), bidi/zero-width controls,
/// collapses whitespace runs and bounds the length in characters with `…`.
pub fn sanitize(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    let mut count = 0;
    let mut pending_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() || matches!(ch, '\n' | '\r' | '\t') {
            pending_space = !output.is_empty();
            continue;
        }
        if unsafe_character(ch) {
            continue;
        }
        if count + usize::from(pending_space) >= max_chars {
            output.push('…');
            return output;
        }
        if pending_space {
            output.push(' ');
            count += 1;
            pending_space = false;
        }
        output.push(ch);
        count += 1;
    }
    output
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// All visible, unexpired history retained by Steam, including read entries.
pub async fn latest(connection: &Connection) -> Result<Vec<Notification>> {
    let response = history::fetch(connection).await?;
    let names = resolve_names(connection, &response.0.notifications).await;
    let time = now();
    let mut notes: Vec<_> = response
        .0
        .notifications
        .iter()
        .filter_map(|data| decode_named(data, time, &names))
        .collect();
    notes.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.id.cmp(&a.id)));
    let mut ids = std::collections::HashSet::new();
    notes.retain(|note| note.id == 0 || ids.insert(note.id));
    Ok(notes)
}

fn body_app_id(data: &SteamNotificationData) -> Option<u32> {
    let object: serde_json::Value = serde_json::from_str(data.body_data()).ok()?;
    let value = object
        .get("app_id")
        .or_else(|| object.get("appid"))
        .or_else(|| object.get("json_data")?.get("app_id"))?;
    let id = value.as_u64().or_else(|| value.as_str()?.parse().ok())?;
    u32::try_from(id).ok().filter(|id| *id != 0)
}

async fn resolve_names(
    connection: &Connection,
    data: &[SteamNotificationData],
) -> BTreeMap<u32, String> {
    let ids: std::collections::BTreeSet<u32> = data.iter().filter_map(body_app_id).collect();
    if ids.is_empty() {
        return BTreeMap::new();
    }
    if let Ok(Ok((apps, _))) = timeout(
        Duration::from_secs(5),
        crate::library::inspect_apps(connection, &ids),
    )
    .await
    {
        apps.into_iter()
            .filter_map(|(id, app)| app.name.map(|name| (id, sanitize(&name, TITLE_CHARS))))
            .collect()
    } else {
        BTreeMap::new()
    }
}

fn decode_named(
    data: &SteamNotificationData,
    time: u64,
    names: &BTreeMap<u32, String>,
) -> Option<Notification> {
    let mut note = decode(data, time)?;
    if note.title == note.label
        && let Some(name) = body_app_id(data).and_then(|id| names.get(&id))
    {
        note.title = name.clone();
    }
    Some(note)
}

fn text_field(
    object: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| {
        let text = match object.get(*key)? {
            serde_json::Value::String(text) => sanitize(text, BODY_CHARS),
            serde_json::Value::Number(number) => number.to_string(),
            _ => return None,
        };
        (!text.is_empty()).then_some(text)
    })
}

/// `body_data` is type-specific JSON whose schema is not part of the protocol
/// definitions. Prefer conventional title/body keys; otherwise show the data
/// itself (sanitized and bounded) rather than guessing at fields.
fn describe(kind: i32, label: &str, body_data: &str) -> (String, String) {
    let raw = || sanitize(body_data, BODY_CHARS);
    let Ok(serde_json::Value::Object(object)) = serde_json::from_str(body_data) else {
        return (label.to_owned(), raw());
    };
    let field = |keys: &[&str]| text_field(&object, keys);
    // These fields are specific to each notification type. Unknown types retain
    // the original data so new server formats remain inspectable.
    let custom = match kind {
        2 => field(&["gifter_account"]).map(|id| format!("Gift from account {id}")),
        4 => field(&["app_id"]).map(|id| {
            let asset = field(&["asset_id"]).map_or(String::new(), |id| format!(" · Item {id}"));
            format!("New inventory item · App {id}{asset}")
        }),
        5 => field(&["requestor_id"]).map(|id| format!("Friend request from {id}")),
        8 => field(&["appid"]).map(|id| {
            let count = field(&["count"]).unwrap_or_else(|| "1".into());
            format!("Wishlist sale · App {id} · {count} game(s)")
        }),
        9 => field(&["sender"]).map(|id| format!("Trade offer from account {id}")),
        12 => field(&["appid"]).map(|id| format!("Game update · App {id}")),
        15 | 18 => field(&["steamid"]).map(|id| format!("Request from {id}")),
        16 => field(&["inviter"]).map(|id| format!("Family invitation from {id}")),
        17 => field(&["requestor_steamid"]).map(|id| format!("Purchase request from {id}")),
        19..=22 => field(&["responder_steamid", "steamid"]).map(|id| format!("Response from {id}")),
        23 => field(&["content_app_name"]).map(|name| {
            let machine =
                field(&["machine_name"]).map_or(String::new(), |name| format!(" · {name}"));
            format!("Clip from {name}{machine}")
        }),
        28 => field(&["appid"]).map(|id| format!("Playtest invitation · App {id}")),
        30 => field(&["report_id"]).map(|id| format!("Content report {id} updated")),
        _ => None,
    };
    let heading = text_field(
        &object,
        &["title", "heading", "app_name", "name", "persona_name"],
    );
    let message =
        text_field(&object, &["body", "text", "message", "msg", "description"]).or(custom);
    if heading.is_none() && message.is_none() {
        return (label.to_owned(), raw());
    }
    (
        heading.map_or_else(
            || label.to_owned(),
            |heading| sanitize(&heading, TITLE_CHARS),
        ),
        sanitize(&message.unwrap_or_default(), BODY_CHARS),
    )
}

/// Decodes one pushed notification; hidden ones are not shown. `fallback_time`
/// stands in for a missing timestamp.
pub fn decode(data: &SteamNotificationData, fallback_time: u64) -> Option<Notification> {
    if data.hidden() || (data.expiry() != 0 && u64::from(data.expiry()) <= fallback_time) {
        return None;
    }
    let kind = data.notification_type.map_or(0, |value| value.value());
    let label = type_label(kind);
    let (title, body) = describe(kind, &label, data.body_data());
    Some(Notification {
        id: data.notification_id(),
        timestamp: data.timestamp.map_or(fallback_time, u64::from),
        label,
        title,
        body,
        read: data.read(),
    })
}

/// Pushes can contain just an ID and changed fields (for example `read`).
/// Preserve absent fields, including when a patch arrives before history.
fn merge_data(base: &mut SteamNotificationData, patch: &SteamNotificationData) {
    macro_rules! fields {
        ($($field:ident),* $(,)?) => { $(if patch.$field.is_some() {
            base.$field = patch.$field;
        })* };
    }
    fields!(
        notification_id,
        notification_targets,
        notification_type,
        read,
        timestamp,
        hidden,
        expiry,
        viewed
    );
    if patch.body_data.is_some() {
        base.body_data.clone_from(&patch.body_data);
    }
}

#[derive(Default)]
struct Feed {
    data: BTreeMap<u64, SteamNotificationData>,
    names: BTreeMap<u32, String>,
}

impl Feed {
    fn push(&mut self, data: &SteamNotificationData, time: u64) -> Option<NotificationEvent> {
        let merged = if data.notification_id() == 0 {
            data
        } else {
            let entry = self.data.entry(data.notification_id()).or_default();
            merge_data(entry, data);
            entry
        };
        if merged.hidden() || (merged.expiry() != 0 && u64::from(merged.expiry()) <= time) {
            return Some(NotificationEvent::Removed(merged.notification_id()));
        }
        // A state-only patch for an unseen ID needs its history body first.
        merged.notification_type?;
        decode_named(merged, time, &self.names).map(NotificationEvent::Received)
    }

    fn history(&mut self, data: Vec<SteamNotificationData>, time: u64) -> Vec<Notification> {
        let mut notes = Vec::new();
        for mut entry in data {
            if entry.notification_id() != 0 {
                if let Some(patch) = self.data.get(&entry.notification_id()) {
                    merge_data(&mut entry, patch);
                }
                self.data.insert(entry.notification_id(), entry.clone());
            }
            if let Some(note) = decode_named(&entry, time, &self.names) {
                notes.push(note);
            }
        }
        notes
    }
}

/// Current non-zero counts. Each `apply_*` returns whether anything changed.
#[derive(Debug, Default)]
pub struct Counts(BTreeMap<CountKind, u32>);

impl Counts {
    fn set(&mut self, kind: CountKind, count: u32) {
        if count == 0 {
            self.0.remove(&kind);
        } else {
            self.0.insert(kind, count);
        }
    }

    pub fn snapshot(&self) -> Vec<(CountKind, u32)> {
        self.0.iter().map(|(kind, count)| (*kind, *count)).collect()
    }

    fn changed(&mut self, update: impl FnOnce(&mut Self)) -> bool {
        let before = self.0.clone();
        update(self);
        before != self.0
    }

    /// Replaces every type this message can carry: absent means zero.
    pub fn apply_user(&mut self, message: &CMsgClientUserNotifications) -> bool {
        self.changed(|counts| {
            counts.0.retain(|kind, _| {
                !matches!(
                    kind,
                    CountKind::TradeOffers | CountKind::CommunityMessages | CountKind::Other(_)
                )
            });
            for entry in &message.notifications {
                let kind = CountKind::from_user_type(entry.user_notification_type());
                let total = counts.0.get(&kind).copied().unwrap_or(0);
                counts.set(kind, total.saturating_add(entry.count()));
            }
        })
    }

    pub fn apply_comments(&mut self, message: &CMsgClientCommentNotifications) -> bool {
        self.changed(|counts| counts.set(CountKind::Comments, message.count_new_comments()))
    }

    pub fn apply_items(&mut self, message: &CMsgClientItemAnnouncements) -> bool {
        self.changed(|counts| counts.set(CountKind::Items, message.count_new_items()))
    }

    /// Pending counts are present only when Steam sends them.
    pub fn apply_pending(
        &mut self,
        message: &CSteamNotification_NotificationsReceived_Notification,
    ) -> bool {
        self.changed(|counts| {
            for (kind, value) in [
                (CountKind::Gifts, message.pending_gift_count),
                (CountKind::FriendInvites, message.pending_friend_count),
                (
                    CountKind::FamilyInvites,
                    message.pending_family_invite_count,
                ),
            ] {
                if let Some(value) = value {
                    counts.set(kind, value);
                }
            }
        })
    }
}

/// Maintain live notification counts and the incoming feed until the consumer
/// drops. Requires a LocalSet like the other connection workers. Start it right
/// after login: only this worker's message kinds are taken from the startup
/// buffer, so it coexists with the friends and library consumers.
pub async fn run(connection: Connection, updates: mpsc::Sender<NotificationEvent>) -> Result<()> {
    let result = run_inner(connection, &updates).await;
    if let Err(error) = &result {
        let _ = updates
            .send(NotificationEvent::Status(error.to_string()))
            .await;
    }
    result
}

fn startup<T: ReceivableMessage>(connection: &Connection) -> Vec<T> {
    connection
        .take_unprocessed_kind::<T>()
        .into_iter()
        .filter(|raw| raw.data.len() <= MAX_RAW_BYTES)
        .filter_map(|raw| raw.into_message::<T>().ok())
        .collect()
}

async fn run_inner(
    connection: Connection,
    updates: &mpsc::Sender<NotificationEvent>,
) -> Result<()> {
    // on()/on_notification() register synchronously; register everything before
    // draining the startup buffer so nothing falls into the gap.
    let users = connection.on::<CMsgClientUserNotifications>();
    let comments = connection.on::<CMsgClientCommentNotifications>();
    let items = connection.on::<CMsgClientItemAnnouncements>();
    let pushes =
        connection.on_notification::<CSteamNotification_NotificationsReceived_Notification>();
    futures_util::pin_mut!(users, comments, items, pushes);

    let mut counts = Counts::default();
    for message in startup::<CMsgClientUserNotifications>(&connection) {
        counts.apply_user(&message);
    }
    for message in startup::<CMsgClientCommentNotifications>(&connection) {
        counts.apply_comments(&message);
    }
    for message in startup::<CMsgClientItemAnnouncements>(&connection) {
        counts.apply_items(&message);
    }
    // Item and comment counts are only sent on request; user counts are pushed.
    for request in [
        timeout(
            REQUEST_TIMEOUT,
            connection.send(CMsgClientRequestItemAnnouncements::default()),
        )
        .await,
        timeout(
            REQUEST_TIMEOUT,
            connection.send(CMsgClientRequestCommentNotifications::default()),
        )
        .await,
    ] {
        if !matches!(request, Ok(Ok(()))) {
            send(
                updates,
                NotificationEvent::Status("Steam notification count request failed".into()),
            )
            .await?;
        }
    }
    send(updates, NotificationEvent::Counts(counts.snapshot())).await?;

    let mut reported = 0u32;
    send(
        updates,
        NotificationEvent::Status("Loading recent notifications…".into()),
    )
    .await?;
    let history = async {
        let response = history::fetch(&connection).await?;
        let names = resolve_names(&connection, &response.0.notifications).await;
        Ok::<_, Box<dyn Error>>((response, names))
    };
    futures_util::pin_mut!(history);
    let mut history_pending = true;
    let mut pending_seen = std::collections::HashSet::new();
    let mut feed = Feed::default();
    loop {
        let mut report = |what: &str| {
            reported += 1;
            (reported <= MAX_REPORTED_ERRORS)
                .then(|| format!("Steam {what} notification could not be read"))
        };
        let mut events = Vec::new();
        let changed = tokio::select! {
            _ = updates.closed() => return Ok(()),
            response = &mut history, if history_pending => {
                history_pending = false;
                match response {
                    Ok((response, names)) => {
                        let time = now();
                        feed.names = names;
                        let mut snapshot = response.0;
                        let notes = feed.history(std::mem::take(&mut snapshot.notifications), time);
                        events.push(NotificationEvent::History(notes));
                        events.push(NotificationEvent::Status(String::new()));
                        // Live pending counts are authoritative if already received.
                        for (kind, value) in [
                            (CountKind::Gifts, &mut snapshot.pending_gift_count),
                            (CountKind::FriendInvites, &mut snapshot.pending_friend_count),
                            (CountKind::FamilyInvites, &mut snapshot.pending_family_invite_count),
                        ] {
                            if pending_seen.contains(&kind) { *value = None; }
                        }
                        counts.apply_pending(&snapshot)
                    }
                    Err(error) => {
                        events.push(NotificationEvent::Status(error.to_string()));
                        false
                    }
                }
            },
            message = users.next() => match message {
                Some(Ok(message)) => counts.apply_user(&message),
                Some(Err(_)) => { events.extend(report("count").map(NotificationEvent::Status)); false }
                None => return Err(failure("Steam notification stream ended")),
            },
            message = comments.next() => match message {
                Some(Ok(message)) => counts.apply_comments(&message),
                Some(Err(_)) => { events.extend(report("comment").map(NotificationEvent::Status)); false }
                None => return Err(failure("Steam notification stream ended")),
            },
            message = items.next() => match message {
                Some(Ok(message)) => counts.apply_items(&message),
                Some(Err(_)) => { events.extend(report("item").map(NotificationEvent::Status)); false }
                None => return Err(failure("Steam notification stream ended")),
            },
            message = pushes.next() => match message {
                Some(Ok(message)) => {
                    let time = now();
                    for data in &message.notifications {
                        events.extend(feed.push(data, time));
                    }
                    for (kind, value) in [
                        (CountKind::Gifts, message.pending_gift_count),
                        (CountKind::FriendInvites, message.pending_friend_count),
                        (CountKind::FamilyInvites, message.pending_family_invite_count),
                    ] {
                        if value.is_some() { pending_seen.insert(kind); }
                    }
                    counts.apply_pending(&message)
                }
                Some(Err(_)) => { events.extend(report("push").map(NotificationEvent::Status)); false }
                None => return Err(failure("Steam notification stream ended")),
            },
        };
        for event in events {
            send(updates, event).await?;
        }
        if changed {
            send(updates, NotificationEvent::Counts(counts.snapshot())).await?;
        }
    }
}

async fn send(updates: &mpsc::Sender<NotificationEvent>, event: NotificationEvent) -> Result<()> {
    updates
        .send(event)
        .await
        .map_err(|_| failure("notification consumer closed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use steam_vent_proto_steam::steammessages_clientserver_2::cmsg_client_user_notifications::Notification as UserCount;
    use steam_vent_proto_steam::steammessages_notifications_steamclient::ESteamNotificationType;

    fn user(counts: &[(u32, u32)]) -> CMsgClientUserNotifications {
        CMsgClientUserNotifications {
            notifications: counts
                .iter()
                .map(|&(kind, count)| UserCount {
                    user_notification_type: Some(kind),
                    count: Some(count),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn data(kind: ESteamNotificationType, body: &str) -> SteamNotificationData {
        SteamNotificationData {
            notification_id: Some(7),
            notification_type: Some(kind.into()),
            body_data: Some(body.into()),
            timestamp: Some(1_700_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn labels_cover_known_types_and_name_unknown_ones() {
        assert_eq!(type_label(9), "Trade offer");
        assert_eq!(type_label(13), "Chat message");
        assert_eq!(type_label(27), "Partner event");
        assert_eq!(type_label(0), "Notification #0");
        assert_eq!(type_label(99), "Notification #99");
        assert_eq!(CountKind::from_user_type(1).label(), "Trade offers");
        assert_eq!(CountKind::from_user_type(3).label(), "Community messages");
        assert_eq!(
            CountKind::from_user_type(42).label(),
            "Notifications (type 42)"
        );
    }

    #[test]
    fn sanitize_strips_controls_escapes_and_bounds_length() {
        assert_eq!(
            sanitize("a\u{1b}[31mred\u{7}\u{9b}x\u{202e}y\u{200b}z", 100),
            "a[31mredxyz"
        );
        assert_eq!(sanitize("  one \n\t two\r\n", 100), "one two");
        assert_eq!(sanitize("abcdefghij", 5), "abcde…");
        assert_eq!(sanitize("abcde", 5), "abcde");
        assert_eq!(sanitize("日本語日本語", 3), "日本語…");
        assert_eq!(sanitize("\u{1b}\u{7}", 5), "");
    }

    #[test]
    fn user_counts_replace_and_absent_types_drop_to_zero() {
        let mut counts = Counts::default();
        assert!(counts.apply_user(&user(&[(1, 2), (3, 1), (8, 4)])));
        assert_eq!(
            counts.snapshot(),
            [
                (CountKind::TradeOffers, 2),
                (CountKind::CommunityMessages, 1),
                (CountKind::Other(8), 4)
            ]
        );
        assert!(!counts.apply_user(&user(&[(1, 2), (3, 1), (8, 4)])));
        assert!(counts.apply_user(&user(&[(3, 5)])));
        assert_eq!(counts.snapshot(), [(CountKind::CommunityMessages, 5)]);
        assert!(counts.apply_user(&user(&[])));
        assert!(counts.snapshot().is_empty());
    }

    #[test]
    fn comment_item_and_pending_counts_are_independent() {
        let mut counts = Counts::default();
        counts.apply_user(&user(&[(1, 1)]));
        assert!(counts.apply_comments(&CMsgClientCommentNotifications {
            count_new_comments: Some(3),
            ..Default::default()
        }));
        assert!(counts.apply_items(&CMsgClientItemAnnouncements {
            count_new_items: Some(2),
            ..Default::default()
        }));
        let push = CSteamNotification_NotificationsReceived_Notification {
            pending_gift_count: Some(1),
            pending_friend_count: Some(0),
            ..Default::default()
        };
        assert!(counts.apply_pending(&push));
        assert_eq!(
            counts.snapshot(),
            [
                (CountKind::TradeOffers, 1),
                (CountKind::Comments, 3),
                (CountKind::Items, 2),
                (CountKind::Gifts, 1),
            ]
        );
        // Absent pending fields leave counts alone; a user message keeps others.
        assert!(!counts.apply_pending(&Default::default()));
        counts.apply_user(&user(&[]));
        assert_eq!(counts.snapshot().len(), 3);
        counts.apply_comments(&CMsgClientCommentNotifications::default());
        assert!(
            !counts
                .snapshot()
                .iter()
                .any(|(kind, _)| *kind == CountKind::Comments)
        );
    }

    #[test]
    fn decodes_titled_json_bodies() {
        let note = decode(
            &data(
                ESteamNotificationType::k_ESteamNotificationType_Wishlist,
                r#"{"title":"Sale!\u001b[2J","body":"Half\nprice","app_id":12}"#,
            ),
            5,
        )
        .unwrap();
        assert_eq!(note.id, 7);
        assert_eq!(note.timestamp, 1_700_000_000);
        assert_eq!(note.label, "Wishlist");
        assert_eq!(note.title, "Sale![2J");
        assert_eq!(note.body, "Half price");
        assert!(!note.read);
    }

    #[test]
    fn unstructured_bodies_are_shown_sanitized_and_bounded() {
        let note = decode(
            &data(
                ESteamNotificationType::k_ESteamNotificationType_ChatMsg,
                r#"{"steamid_sender":"123","unit":"x"}"#,
            ),
            9,
        )
        .unwrap();
        assert_eq!(note.title, "Chat message");
        assert!(note.body.contains("steamid_sender"));
        let long = "z".repeat(10_000);
        let note = decode(
            &data(
                ESteamNotificationType::k_ESteamNotificationType_General,
                &long,
            ),
            9,
        )
        .unwrap();
        assert_eq!(note.body.chars().count(), BODY_CHARS + 1);
        let text = decode(
            &data(
                ESteamNotificationType::k_ESteamNotificationType_General,
                "plain \u{1b}]0;evil\u{7} text",
            ),
            9,
        )
        .unwrap();
        assert_eq!(text.body, "plain ]0;evil text");
    }

    #[test]
    fn hidden_notifications_are_skipped_and_missing_time_falls_back() {
        let mut hidden = data(ESteamNotificationType::k_ESteamNotificationType_Gift, "{}");
        hidden.hidden = Some(true);
        assert!(decode(&hidden, 1).is_none());
        let mut note = data(ESteamNotificationType::k_ESteamNotificationType_Gift, "{}");
        note.timestamp = None;
        note.read = Some(true);
        let note = decode(&note, 1234).unwrap();
        assert_eq!(note.timestamp, 1234);
        assert!(note.read);
    }

    #[test]
    fn json_numbers_and_empty_titles_fall_back_gracefully() {
        let (title, body) = describe(2, "Gift", r#"{"name":"","text":42}"#);
        assert_eq!(title, "Gift");
        assert_eq!(body, "42");
        let (title, body) = describe(2, "Gift", "not json");
        assert_eq!((title.as_str(), body.as_str()), ("Gift", "not json"));
    }

    #[test]
    fn type_specific_bodies_are_readable_and_do_not_confuse_field_names() {
        for (kind, json, expected) in [
            (2, r#"{"gifter_account":123}"#, "Gift from account 123"),
            (
                4,
                r#"{"app_id":"440","asset_id":"987"}"#,
                "New inventory item · App 440 · Item 987",
            ),
            (
                5,
                r#"{"requestor_id":"76561198000000001"}"#,
                "Friend request from 76561198000000001",
            ),
            (
                8,
                r#"{"appid":620,"count":3,"appids":[620,440,570]}"#,
                "Wishlist sale · App 620 · 3 game(s)",
            ),
            (9, r#"{"sender":"123"}"#, "Trade offer from account 123"),
            (
                16,
                r#"{"inviter":"76561198000000001","familyid":"9"}"#,
                "Family invitation from 76561198000000001",
            ),
            (
                23,
                r#"{"content_app_name":"Portal 2","machine_name":"Deck"}"#,
                "Clip from Portal 2 · Deck",
            ),
        ] {
            assert_eq!(describe(kind, &type_label(kind), json).1, expected);
        }
        assert_eq!(
            describe(
                3,
                "Comment",
                r#"{"title":"Workshop","text":"A reply","type":"10"}"#
            ),
            ("Workshop".into(), "A reply".into())
        );
        assert!(
            describe(99, "Unknown", r#"{"future_field":42}"#)
                .1
                .contains("future_field")
        );
    }

    #[test]
    fn expired_entries_are_filtered_without_dropping_read_history() {
        let mut note = data(ESteamNotificationType::k_ESteamNotificationType_Gift, "{}");
        note.expiry = Some(100);
        assert!(decode(&note, 100).is_none());
        note.expiry = Some(101);
        note.read = Some(true);
        assert!(decode(&note, 100).unwrap().read);
        note.expiry = Some(0);
        assert!(decode(&note, 999).is_some());
    }

    #[test]
    fn partial_pushes_merge_and_late_history_preserves_read_and_hide_changes() {
        let original = data(
            ESteamNotificationType::k_ESteamNotificationType_Comment,
            r#"{"title":"Workshop","text":"Hello"}"#,
        );
        let patch = SteamNotificationData {
            notification_id: Some(7),
            read: Some(true),
            ..Default::default()
        };
        let mut feed = Feed::default();
        assert!(feed.push(&patch, 1).is_none());
        let history = feed.history(vec![original.clone()], 1);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].title, "Workshop");
        assert!(history[0].read);
        let Some(NotificationEvent::Received(note)) = feed.push(&patch, 2) else {
            panic!("missing merged note")
        };
        assert_eq!(note.body, "Hello");
        assert_eq!(note.timestamp, original.timestamp() as u64);
        let hide = SteamNotificationData {
            notification_id: Some(7),
            hidden: Some(true),
            ..Default::default()
        };
        assert_eq!(feed.push(&hide, 3), Some(NotificationEvent::Removed(7)));
        assert!(feed.history(vec![original], 3).is_empty());
    }

    #[test]
    fn current_types_and_app_names_survive_stale_protocol_enums() {
        assert_eq!(type_label(23), "Clip downloaded");
        assert_eq!(type_label(24), "Send to phone");
        let mut invite = data(
            ESteamNotificationType::k_ESteamNotificationType_Test,
            r#"{"appid":3348390,"is_limited_launch":false,"playtest_invite_id":123}"#,
        );
        invite.notification_type = Some(protobuf::EnumOrUnknown::from_i32(28));
        let names = BTreeMap::from([(3348390, "A playtest".into())]);
        let note = decode_named(&invite, 1, &names).unwrap();
        assert_eq!(note.label, "Playtest invite");
        assert_eq!(note.title, "A playtest");
        assert_eq!(note.body, "Playtest invitation · App 3348390");
        assert_eq!(body_app_id(&invite), Some(3348390));
    }
}
