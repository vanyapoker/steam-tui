use super::model::Model;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};
use steam_core::chat::{Command, Event, MAX_MESSAGE_BYTES, Message};

#[derive(Default)]
pub(super) struct Conversation {
    pub messages: Vec<Message>,
    pub unread: u32,
    pub latest: u32,
    pub read_through: u32,
    pub read_pending: Option<u32>,
    pub read_failed: u32,
    pub draft: String,
    pub attachment: Option<steam_core::chat::upload::Attachment>,
    pub clipboard_pending: bool,
    pub notice: String,
    pub cursor: usize,
    pub sending: bool,
    pub loading: bool,
    pub loaded: bool,
    pub history_loaded: bool,
    pub more: bool,
    pub error: String,
    pub typing: Option<Instant>,
}

#[derive(Default)]
pub(super) struct Chat {
    pub images: super::chat_images::Images,
    pub available: bool,
    pub conversations: BTreeMap<u64, Conversation>,
    pub active: Option<u64>,
    pub offset: usize,
    pub rows: usize,
    pub pending: VecDeque<Command>,
    pub status: String,
}

impl Conversation {
    fn merge(&mut self, mut message: Message) -> bool {
        if self.messages.iter().any(|m| {
            (m.timestamp, m.ordinal, m.outgoing)
                == (message.timestamp, message.ordinal, message.outgoing)
        }) {
            return false;
        }
        message.text = clean(&message.text);
        self.latest = self.latest.max(message.timestamp);
        self.messages.push(message);
        self.messages.sort_by_key(|m| (m.timestamp, m.ordinal));
        true
    }
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .collect()
}

impl Model {
    pub fn selected_friend(&self) -> Option<&steam_core::friends::Friend> {
        self.friend_order
            .get(self.friend_selected)
            .map(|&i| &self.friends[i])
    }

    pub fn open_chat(&mut self) {
        let Some(friend) = self.selected_friend().map(|f| f.steam_id) else {
            return;
        };
        self.chat.active = Some(friend);
        self.chat.offset = 0;
        if !self.chat.available {
            return;
        }
        let conversation = self.chat.conversations.entry(friend).or_default();
        conversation.read_failed = 0;
        if !conversation.loading {
            conversation.loading = true;
            conversation.loaded = false;
            conversation.error.clear();
            self.chat.pending.push_back(Command::History {
                friend,
                before: None,
            });
        }
        self.read_chat();
    }

    fn read_chat(&mut self) {
        if self.chat.offset != 0 || !self.chat.available {
            return;
        }
        let Some(friend) = self.chat.active else {
            return;
        };
        let c = self.chat.conversations.entry(friend).or_default();
        if c.loaded
            && !c.loading
            && (c.latest > c.read_through || c.unread > 0)
            && (c.read_failed == 0 || c.latest > c.read_failed)
            && c.read_pending.is_none()
        {
            c.read_pending = Some(c.latest);
            self.chat.pending.push_back(Command::Read {
                friend,
                timestamp: c.latest,
            });
        }
    }

    pub fn chat_event(&mut self, event: Event) {
        match event {
            Event::ImageSent { friend, result } => {
                let c = self.chat.conversations.entry(friend).or_default();
                c.sending = false;
                match result {
                    Ok(()) => {
                        c.attachment = None;
                        c.error.clear();
                        c.notice = if c.draft.is_empty() {
                            "Image sent".into()
                        } else {
                            "Image sent; text draft kept — Enter sends it".into()
                        };
                        c.loading = true;
                        self.chat.pending.push_back(Command::History {
                            friend,
                            before: None,
                        });
                    }
                    Err(error) => c.error = error,
                }
            }
            Event::Sessions(sessions) => {
                for (friend, count, latest) in sessions {
                    let c = self.chat.conversations.entry(friend).or_default();
                    c.latest = c.latest.max(latest);
                    if latest > c.read_through {
                        let newer = c
                            .messages
                            .iter()
                            .filter(|m| {
                                !m.outgoing && m.timestamp > latest && m.timestamp > c.read_through
                            })
                            .count() as u32;
                        c.unread = count.saturating_add(newer);
                    }
                }
            }
            Event::Message { friend, message } => {
                let c = self.chat.conversations.entry(friend).or_default();
                // A new ordinal can arrive in the same second as our last
                // acknowledgement. It still counts as a new arrival.
                let unread = !message.outgoing && message.timestamp >= c.read_through;
                if c.merge(message) && unread {
                    c.unread = c.unread.saturating_add(1);
                }
                c.typing = None;
            }
            Event::Typing(friend) => {
                self.chat.conversations.entry(friend).or_default().typing = Some(Instant::now())
            }
            Event::Read { friend, timestamp } => {
                let c = self.chat.conversations.entry(friend).or_default();
                c.read_through = c.read_through.max(timestamp);
                if c.read_pending.is_some_and(|pending| pending <= timestamp) {
                    c.read_pending = None;
                }
                if timestamp >= c.latest {
                    c.unread = 0;
                } else {
                    c.unread = c
                        .messages
                        .iter()
                        .filter(|m| !m.outgoing && m.timestamp > c.read_through)
                        .count()
                        .try_into()
                        .unwrap_or(u32::MAX);
                }
            }
            Event::History {
                friend,
                messages,
                more,
                older,
            } => {
                let c = self.chat.conversations.entry(friend).or_default();
                let previous_oldest = c.messages.first().map(|m| (m.timestamp, m.ordinal));
                for message in messages {
                    c.merge(message);
                }
                c.loading = false;
                c.loaded = true;
                if older {
                    c.more = more
                        && previous_oldest != c.messages.first().map(|m| (m.timestamp, m.ordinal));
                } else if !c.history_loaded {
                    c.more = more;
                }
                c.history_loaded = true;
            }
            Event::ReadFailed { friend, timestamp } => {
                let c = self.chat.conversations.entry(friend).or_default();
                if c.read_pending == Some(timestamp) {
                    c.read_pending = None;
                }
                c.read_failed = c.read_failed.max(timestamp);
                c.error = "Could not mark messages as read; reopen chat to retry".into();
            }
            Event::HistoryFailed { friend, error } => {
                let c = self.chat.conversations.entry(friend).or_default();
                c.loading = false;
                c.error = error;
            }
            Event::Sent { friend, result } => {
                let c = self.chat.conversations.entry(friend).or_default();
                c.sending = false;
                match result {
                    Ok(message) => {
                        c.merge(message);
                        c.draft.clear();
                        c.cursor = 0;
                        c.error.clear();
                        c.notice.clear();
                    }
                    Err(error) => c.error = error,
                }
            }
            Event::Status(status) => self.chat.status = clean(&status),
            Event::Disconnected(status) => {
                self.chat_disconnected();
                self.chat.status = clean(&status);
            }
        }
        self.filter_chat_friends();
        self.read_chat();
    }

    pub fn clipboard_result(
        &mut self,
        friend: u64,
        result: Result<super::clipboard::Content, String>,
    ) {
        let c = self.chat.conversations.entry(friend).or_default();
        c.clipboard_pending = false;
        match result {
            Ok(super::clipboard::Content::Image(image)) => {
                c.attachment = Some(image);
                c.error.clear();
                c.notice = "Image attached — Enter sends · Ctrl+Delete removes".into();
            }
            Ok(super::clipboard::Content::Text(text)) => {
                // Apply to the conversation which requested the clipboard, even
                // if its modal was closed while the compositor supplied data.
                for ch in clean(&text).chars() {
                    if c.draft.len() + ch.len_utf8() > MAX_MESSAGE_BYTES {
                        break;
                    }
                    c.draft.insert(c.cursor, ch);
                    c.cursor += ch.len_utf8();
                }
            }
            Err(error) => c.error = error,
        }
    }

    pub fn chat_disconnected(&mut self) {
        self.chat.available = false;
        self.chat.pending.clear();
        self.chat.status = "Chat disconnected — press r after closing chat to reconnect".into();
        for c in self.chat.conversations.values_mut() {
            c.loading = false;
            c.read_pending = None;
            if c.sending {
                c.error =
                    "Connection lost; delivery unknown. Check history before retrying.".into();
            }
            c.sending = false;
        }
    }

    pub fn chat_key(&mut self, key: KeyEvent) {
        let Some(friend) = self.chat.active else {
            return;
        };
        let c = self.chat.conversations.entry(friend).or_default();
        match key.code {
            KeyCode::Esc => self.chat.active = None,
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) && !c.sending => {
                c.attachment = None;
                c.notice.clear();
            }
            KeyCode::PageUp => {
                self.chat.offset = self.chat.offset.saturating_add(self.chat.rows.max(1));
                if self.chat.available
                    && c.more
                    && !c.loading
                    && let Some(first) = c.messages.first()
                {
                    self.chat.pending.push_back(Command::History {
                        friend,
                        before: Some((first.timestamp, first.ordinal)),
                    });
                    c.loading = true;
                }
            }
            KeyCode::PageDown => {
                self.chat.offset = self.chat.offset.saturating_sub(self.chat.rows.max(1));
                self.read_chat();
            }
            KeyCode::Enter if !c.sending && key.modifiers == KeyModifiers::SHIFT => {
                if c.draft.len() < MAX_MESSAGE_BYTES {
                    c.draft.insert(c.cursor, '\n');
                    c.cursor += 1;
                }
            }
            KeyCode::Enter if !c.sending && !c.clipboard_pending && c.attachment.is_some() => {
                if !self.chat.available {
                    c.error = self.chat.status.clone();
                    return;
                }
                c.sending = true;
                c.error.clear();
                c.notice.clear();
                self.chat.pending.push_back(Command::SendImage {
                    friend,
                    attachment: c.attachment.as_ref().expect("attachment exists").clone(),
                });
            }
            KeyCode::Enter if !c.sending && !c.clipboard_pending && !c.draft.trim().is_empty() => {
                if !self.chat.available {
                    c.error = self.chat.status.clone();
                    return;
                }
                c.sending = true;
                c.error.clear();
                self.chat.offset = 0;
                self.chat.pending.push_back(Command::Send {
                    friend,
                    text: c.draft.clone(),
                });
            }
            KeyCode::Left => {
                c.cursor = c.draft[..c.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(i, _)| i);
            }
            KeyCode::Right => {
                c.cursor += c.draft[c.cursor..].chars().next().map_or(0, char::len_utf8);
            }
            KeyCode::Home => c.cursor = 0,
            KeyCode::End => c.cursor = c.draft.len(),
            KeyCode::Backspace if !c.sending && c.cursor > 0 => {
                let previous = c.draft[..c.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(i, _)| i);
                c.draft.drain(previous..c.cursor);
                c.cursor = previous;
            }
            KeyCode::Delete if !c.sending && c.cursor < c.draft.len() => {
                c.draft.remove(c.cursor);
            }
            KeyCode::Char(ch)
                if !c.sending
                    && !ch.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && c.draft.len() + ch.len_utf8() <= MAX_MESSAGE_BYTES =>
            {
                c.draft.insert(c.cursor, ch);
                c.cursor += ch.len_utf8();
            }
            _ => {}
        }
    }

    pub fn chat_paste(&mut self, text: &str) {
        if self.chat.active.is_none() {
            let editing = self.searching
                || self
                    .modal
                    .as_ref()
                    .is_some_and(|modal| modal.custom.is_some() && !modal.checking)
                || self
                    .properties
                    .as_ref()
                    .is_some_and(|props| props.password.is_some());
            if editing {
                for ch in text.chars().filter(|ch| !ch.is_control()) {
                    self.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
                }
            }
            return;
        }
        if let Some(friend) = self.chat.active {
            let c = self.chat.conversations.entry(friend).or_default();
            if !c.sending {
                for ch in clean(text).chars() {
                    if c.draft.len() + ch.len_utf8() > MAX_MESSAGE_BYTES {
                        break;
                    }
                    c.draft.insert(c.cursor, ch);
                    c.cursor += ch.len_utf8();
                }
            }
        }
    }
}

pub(super) fn typing(c: &Conversation) -> bool {
    c.typing
        .is_some_and(|at| at.elapsed() < Duration::from_secs(8))
}

#[cfg(test)]
mod tests {
    use super::super::model::{Action, Focus};
    use super::*;
    use steam_core::friends::{Friend, Presence};

    fn friend(id: u64, name: &str, state: Presence) -> Friend {
        Friend {
            steam_id: id,
            name: name.into(),
            state,
            game: None,
        }
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn message(timestamp: u32, ordinal: u32, outgoing: bool) -> Message {
        Message {
            timestamp,
            ordinal,
            outgoing,
            text: "hello\u{1b}[2J\nworld".into(),
        }
    }
    fn model() -> Model {
        let mut model = Model::new(false, None);
        model.set_friends(vec![
            friend(10, "Alice", Presence::Online),
            friend(20, "Bob", Presence::Online),
            friend(30, "Cat", Presence::Online),
        ]);
        model.focus = Focus::Friends;
        model.friend_rows = 2;
        model
    }

    #[test]
    fn arrows_select_keep_visible_and_enter_opens_the_selected_friend() {
        let mut m = model();
        m.key(key(KeyCode::Down));
        assert_eq!(m.selected_friend().unwrap().steam_id, 20);
        assert_eq!(m.friend_offset, 0);
        m.key(key(KeyCode::Down));
        assert_eq!((m.friend_selected, m.friend_offset), (2, 1));
        m.key(key(KeyCode::Up));
        assert_eq!(m.key(key(KeyCode::Enter)), Action::None);
        assert_eq!(m.chat.active, Some(20));
        assert!(matches!(
            m.chat.pending.pop_front(),
            Some(Command::History {
                friend: 20,
                before: None
            })
        ));
        m.key(key(KeyCode::Char('q')));
        assert_eq!(m.chat.conversations[&20].draft, "q");
        m.key(key(KeyCode::Esc));
        assert_eq!(m.chat.active, None);
        m.open_chat();
        assert_eq!(m.chat.conversations[&20].draft, "q");
    }

    #[test]
    fn selection_follows_identity_through_presence_and_name_reordering() {
        let mut m = model();
        m.key(key(KeyCode::Down));
        m.show_offline = true;
        m.set_friends(vec![
            friend(10, "Zed", Presence::Online),
            friend(20, "Bob", Presence::Offline),
            friend(30, "Cat", Presence::Online),
        ]);
        assert_eq!(m.selected_friend().unwrap().steam_id, 20);
        m.set_friends(vec![friend(30, "Cat", Presence::Online)]);
        assert_eq!(m.selected_friend().unwrap().steam_id, 30);
        m.set_friends(vec![]);
        m.key(key(KeyCode::Enter));
        assert_eq!(m.chat.active, None);
    }

    #[test]
    fn unread_counts_merge_with_live_events_and_keep_offline_senders_visible() {
        let mut m = model();
        m.set_friends(vec![friend(10, "Alice", Presence::Offline)]);
        assert!(m.friend_order.is_empty());
        m.chat_event(Event::Message {
            friend: 10,
            message: message(100, 0, false),
        });
        m.chat_event(Event::Message {
            friend: 10,
            message: message(101, 0, false),
        });
        m.chat_event(Event::Sessions(vec![(10, 4, 100)]));
        assert_eq!(m.chat.conversations[&10].unread, 5);
        assert_eq!(m.friend_order.len(), 1);
        m.chat_event(Event::Message {
            friend: 10,
            message: message(101, 0, false),
        });
        assert_eq!(m.chat.conversations[&10].unread, 5);
        m.open_chat();
        // Opening cannot acknowledge messages before history is available.
        assert!(
            !m.chat
                .pending
                .iter()
                .any(|c| matches!(c, Command::Read { .. }))
        );
        m.chat_event(Event::History {
            friend: 10,
            messages: vec![message(100, 0, false), message(101, 0, false)],
            more: false,
            older: false,
        });
        assert!(m.chat.pending.iter().any(|c| matches!(
            c,
            Command::Read {
                friend: 10,
                timestamp: 101
            }
        )));
        assert_eq!(
            m.chat.conversations[&10].unread, 5,
            "badge waits for successful acknowledgement"
        );
        m.chat_event(Event::Read {
            friend: 10,
            timestamp: 101,
        });
        assert_eq!(m.chat.conversations[&10].unread, 0);
        // A late snapshot cannot resurrect read messages.
        m.chat_event(Event::Sessions(vec![(10, 5, 101)]));
        assert_eq!(m.chat.conversations[&10].unread, 0);
    }

    #[test]
    fn history_merges_by_timestamp_and_ordinal_without_losing_live_messages() {
        let mut m = model();
        m.chat_event(Event::Message {
            friend: 10,
            message: message(110, 0, false),
        });
        m.chat_event(Event::History {
            friend: 10,
            messages: vec![message(100, 2, false), message(100, 1, false)],
            more: true,
            older: false,
        });
        let c = &m.chat.conversations[&10];
        assert_eq!(
            c.messages
                .iter()
                .map(|m| (m.timestamp, m.ordinal))
                .collect::<Vec<_>>(),
            [(100, 1), (100, 2), (110, 0)]
        );
        assert_eq!(c.unread, 1, "history is not a new arrival");
        assert!(!c.messages[0].text.contains('\u{1b}'));
        m.open_chat();
        m.chat.conversations.get_mut(&10).unwrap().loading = false;
        m.chat_key(key(KeyCode::PageUp));
        assert!(m.chat.pending.iter().any(|c| matches!(
            c,
            Command::History {
                friend: 10,
                before: Some((100, 1))
            }
        )));
    }

    #[test]
    fn send_failure_keeps_draft_and_success_deduplicates_sender_echo() {
        let mut m = model();
        m.open_chat();
        m.chat_paste("hello\u{1b}\nworld");
        m.chat_key(key(KeyCode::Enter));
        assert!(m.chat.conversations[&10].sending);
        m.chat_key(key(KeyCode::Char('x')));
        m.chat_event(Event::Sent {
            friend: 10,
            result: Err("timeout".into()),
        });
        assert_eq!(m.chat.conversations[&10].draft, "hello\nworld");
        m.chat_key(key(KeyCode::Enter));
        m.chat_event(Event::Message {
            friend: 10,
            message: message(200, 3, true),
        });
        m.chat_event(Event::Sent {
            friend: 10,
            result: Ok(message(200, 3, true)),
        });
        assert!(m.chat.conversations[&10].draft.is_empty());
        assert_eq!(m.chat.conversations[&10].messages.len(), 1);
        assert_eq!(m.chat.conversations[&10].unread, 0);
    }

    #[test]
    fn scrolled_history_does_not_acknowledge_new_messages_and_stale_ack_keeps_new_unread() {
        let mut m = model();
        m.open_chat();
        m.chat.offset = 10;
        m.chat.pending.clear();
        m.chat_event(Event::History {
            friend: 10,
            messages: vec![message(100, 0, false)],
            more: false,
            older: false,
        });
        m.chat_event(Event::Message {
            friend: 10,
            message: message(101, 0, false),
        });
        assert!(m.chat.pending.is_empty());
        m.chat_event(Event::Read {
            friend: 10,
            timestamp: 100,
        });
        assert_eq!(m.chat.conversations[&10].unread, 1);
        m.chat.rows = 20;
        m.chat_key(key(KeyCode::PageDown));
        assert!(matches!(
            m.chat.pending.pop_front(),
            Some(Command::Read { timestamp: 101, .. })
        ));
    }

    #[test]
    fn paste_bounds_utf8_and_disconnect_retains_drafts_without_automatic_retry() {
        let mut m = model();
        m.open_chat();
        m.chat_paste(&"😀".repeat(MAX_MESSAGE_BYTES));
        assert_eq!(m.chat.conversations[&10].draft.len(), MAX_MESSAGE_BYTES);
        m.chat_key(key(KeyCode::Enter));
        m.chat_disconnected();
        assert!(!m.chat.available);
        assert!(m.chat.pending.is_empty());
        assert!(!m.chat.conversations[&10].sending);
        assert_eq!(m.chat.conversations[&10].draft.len(), MAX_MESSAGE_BYTES);
        m.connection_restored();
        assert!(m.chat.available);
        assert!(
            m.chat
                .pending
                .iter()
                .all(|cmd| matches!(cmd, Command::History { .. }))
        );
    }
    #[test]
    fn new_ordinal_in_the_last_read_second_is_unread_and_can_be_acknowledged() {
        let mut m = model();
        m.open_chat();
        m.chat_event(Event::History {
            friend: 10,
            messages: vec![message(100, 0, false)],
            more: false,
            older: false,
        });
        m.chat.pending.clear();
        m.chat_event(Event::Read {
            friend: 10,
            timestamp: 100,
        });
        m.chat.offset = 5;
        m.chat_event(Event::Message {
            friend: 10,
            message: message(100, 1, false),
        });
        assert_eq!(m.chat.conversations[&10].unread, 1);
        m.chat.rows = 10;
        m.chat_key(key(KeyCode::PageDown));
        assert!(
            m.chat
                .pending
                .iter()
                .any(|cmd| matches!(cmd, Command::Read { timestamp: 100, .. }))
        );
    }

    #[test]
    fn failed_acknowledgement_does_not_retry_in_a_loop() {
        let mut m = model();
        m.open_chat();
        m.chat_event(Event::Message {
            friend: 10,
            message: message(100, 0, false),
        });
        m.chat_event(Event::History {
            friend: 10,
            messages: vec![],
            more: false,
            older: false,
        });
        m.chat.pending.clear();
        m.chat_event(Event::ReadFailed {
            friend: 10,
            timestamp: 100,
        });
        m.chat_event(Event::Typing(10));
        assert!(m.chat.pending.is_empty());
        assert_eq!(m.chat.conversations[&10].unread, 1);
        m.chat_key(key(KeyCode::Esc));
        m.open_chat();
        m.chat_event(Event::History {
            friend: 10,
            messages: vec![],
            more: false,
            older: false,
        });
        assert!(
            m.chat
                .pending
                .iter()
                .any(|cmd| matches!(cmd, Command::Read { .. }))
        );
    }

    #[test]
    fn unicode_editing_and_paste_are_text_without_triggering_application_actions() {
        let mut m = model();
        m.open_chat();
        m.chat_paste("a😀b");
        m.chat_key(key(KeyCode::Left));
        m.chat_key(key(KeyCode::Backspace));
        m.chat_key(key(KeyCode::Char('é')));
        assert_eq!(m.chat.conversations[&10].draft, "aéb");
        m.chat_key(key(KeyCode::Delete));
        m.chat_key(key(KeyCode::Home));
        m.chat_key(key(KeyCode::Char('q')));
        m.chat_key(key(KeyCode::End));
        m.chat_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(m.chat.conversations[&10].draft, "qaé\n");
        m.chat_key(key(KeyCode::Esc));
        m.searching = true;
        m.chat_paste("q\u{1b}c\nv");
        assert_eq!(m.search, "qcv");
        assert!(!m.cancel_requested);
    }
    fn attachment() -> steam_core::chat::upload::Attachment {
        let image = image::DynamicImage::new_rgb8(8, 8);
        let mut encoded = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        steam_core::chat::upload::Attachment::decode(encoded.into_inner()).unwrap()
    }

    #[test]
    fn ctrl_v_attaches_locally_and_enter_is_the_only_action_that_queues_an_upload() {
        let mut m = model();
        m.open_chat();
        m.chat.pending.clear();
        let paste = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert_eq!(m.key(paste), Action::Clipboard(10));
        assert_eq!(m.key(paste), Action::None, "one clipboard read at a time");
        m.chat_paste("caption draft");
        m.key(key(KeyCode::Enter));
        assert!(
            m.chat.pending.is_empty(),
            "wait for clipboard result before Enter can send"
        );
        m.clipboard_result(
            10,
            Ok(super::super::clipboard::Content::Image(attachment())),
        );
        assert!(m.chat.pending.is_empty(), "pasting alone must never upload");
        m.key(key(KeyCode::Enter));
        assert!(matches!(
            m.chat.pending.pop_front(),
            Some(Command::SendImage { friend: 10, .. })
        ));
        m.chat_event(Event::ImageSent {
            friend: 10,
            result: Err("commit interrupted".into()),
        });
        assert!(m.chat.conversations[&10].attachment.is_some());
        assert_eq!(m.chat.conversations[&10].draft, "caption draft");
        m.key(key(KeyCode::Enter));
        m.chat.pending.clear();
        m.chat_event(Event::ImageSent {
            friend: 10,
            result: Ok(()),
        });
        assert!(m.chat.conversations[&10].attachment.is_none());
        assert_eq!(m.chat.conversations[&10].draft, "caption draft");
        assert!(
            !m.chat
                .pending
                .iter()
                .any(|command| matches!(command, Command::Send { .. })),
            "commit already sent the image; no extra URL message"
        );
    }

    #[test]
    fn late_clipboard_results_stay_with_the_requesting_friend_and_attachment_can_be_removed() {
        let mut m = model();
        m.open_chat();
        m.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        m.chat_key(key(KeyCode::Esc));
        m.key(key(KeyCode::Down));
        m.open_chat();
        m.clipboard_result(
            10,
            Ok(super::super::clipboard::Content::Image(attachment())),
        );
        assert_eq!(m.chat.active, Some(20));
        assert!(m.chat.conversations[&10].attachment.is_some());
        assert!(m.chat.conversations[&20].attachment.is_none());
        m.chat_key(key(KeyCode::Esc));
        m.key(key(KeyCode::Up));
        m.open_chat();
        m.chat_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL));
        assert!(m.chat.conversations[&10].attachment.is_none());
    }
}
