//! Direct messages over the account's CM connection.
pub mod upload;
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::io::Read;
use std::{pin::Pin, time::Duration};
use steam_vent::{Connection, ConnectionTrait, ServiceNotification};
use steam_vent_proto_steam::steammessages_friendmessages_steamclient::*;
use tokio::{sync::mpsc, time::timeout};

pub const MAX_MESSAGE_BYTES: usize = 6_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub timestamp: u32,
    pub ordinal: u32,
    pub outgoing: bool,
    pub text: String,
}

#[derive(Debug)]
pub enum Command {
    SendImage {
        friend: u64,
        attachment: upload::Attachment,
    },
    History {
        friend: u64,
        before: Option<(u32, u32)>,
    },
    Send {
        friend: u64,
        text: String,
    },
    Read {
        friend: u64,
        timestamp: u32,
    },
}

#[derive(Debug)]
pub enum Event {
    ImageSent {
        friend: u64,
        result: Result<(), String>,
    },
    Sessions(Vec<(u64, u32, u32)>),
    Message {
        friend: u64,
        message: Message,
    },
    Typing(u64),
    Read {
        friend: u64,
        timestamp: u32,
    },
    History {
        friend: u64,
        messages: Vec<Message>,
        more: bool,
        older: bool,
    },
    Sent {
        friend: u64,
        result: Result<Message, String>,
    },
    HistoryFailed {
        friend: u64,
        error: String,
    },
    ReadFailed {
        friend: u64,
        timestamp: u32,
    },
    Status(String),
    Disconnected(String),
}

#[derive(Debug)]
struct AckEcho(CFriendMessages_AckMessage_Notification);
impl ServiceNotification for AckEcho {
    const NOTIFICATION_NAME: &'static str = "FriendMessagesClient.NotifyAckMessageEcho#1";
    fn parse(reader: &mut dyn std::io::Read) -> std::io::Result<Self> {
        use protobuf::Message;
        let mut bytes = Vec::new();
        reader.take(1024).read_to_end(&mut bytes)?;
        CFriendMessages_AckMessage_Notification::parse_from_bytes(&bytes)
            .map(Self)
            .map_err(std::io::Error::other)
    }
}

fn incoming_text(raw: &str, plain: Option<&str>) -> String {
    let mut text = plain
        .filter(|text| !text.is_empty())
        .unwrap_or(raw)
        .to_owned();
    // Rich image messages can carry the image URL only in their formatted
    // body. Keep it available to the terminal renderer as a normal link.
    for (offset, _) in raw.match_indices("https://") {
        let tail = &raw[offset..];
        let end = tail
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '\'' | '"' | '<' | '>' | '[' | ']'))
            .unwrap_or(tail.len());
        let url = &tail[..end];
        if !text.contains(url) {
            text.push('\n');
            text.push_str(url);
        }
    }
    text
}

type Work = Pin<Box<dyn Future<Output = Event>>>;

/// Keep notification ingestion running while history/send requests await replies.
/// Requests and output are bounded; a timed-out send is never automatically retried.
pub async fn run(
    connection: Connection,
    updates: mpsc::Sender<Event>,
    mut commands: mpsc::Receiver<Command>,
) {
    let incoming = connection.on_notification::<CFriendMessages_IncomingMessage_Notification>();
    let acks = connection.on_notification::<AckEcho>();
    futures_util::pin_mut!(incoming, acks);
    let own_id = connection.steam_id().steam64();
    let mut work: FuturesUnordered<Work> = FuturesUnordered::new();
    let initial = connection.clone();
    work.push(Box::pin(async move {
        match timeout(
            REQUEST_TIMEOUT,
            initial.service_method(CFriendsMessages_GetActiveMessageSessions_Request {
                only_sessions_with_messages: Some(true),
                ..Default::default()
            }),
        )
        .await
        {
            Ok(Ok(response)) => Event::Sessions(
                response
                    .message_sessions
                    .iter()
                    .map(|s| {
                        (
                            (own_id & 0xffffffff00000000) | u64::from(s.accountid_friend()),
                            s.unread_message_count(),
                            s.last_message(),
                        )
                    })
                    .collect(),
            ),
            _ => Event::Status("Could not load unread chat counts; reconnect to retry".into()),
        }
    }));
    loop {
        let event = tokio::select! {
            _ = updates.closed() => return,
            message = incoming.next() => match message {
                Some(Ok(m)) if m.steamid_friend() != 0 => {
                    let friend = m.steamid_friend();
                    match m.chat_entry_type() {
                        1 => {
                            Event::Message { friend, message: Message {
                                timestamp: m.rtime32_server_timestamp(), ordinal: m.ordinal(), outgoing: m.local_echo(),
                                text: incoming_text(m.message(), m.message_no_bbcode.as_deref()),
                            }}
                        }
                        2 => Event::Typing(friend),
                        _ => continue,
                    }
                }
                Some(Ok(_)) => continue,
                _ => { let _ = updates.send(Event::Disconnected("Chat stream interrupted; reconnect to recover messages".into())).await; return; }
            },
            ack = acks.next() => match ack {
                Some(Ok(AckEcho(ack))) => Event::Read { friend: ack.steamid_partner(), timestamp: ack.timestamp() },
                _ => { let _ = updates.send(Event::Disconnected("Chat read receipts interrupted; reconnect to recover".into())).await; return; }
            },
            Some(event) = work.next(), if !work.is_empty() => event,
            command = commands.recv(), if work.len() < 8 => {
                let Some(command) = command else { return; };
                let connection = connection.clone();
                work.push(Box::pin(async move { execute(&connection, own_id, command).await }));
                continue;
            }
        };
        if updates.send(event).await.is_err() {
            return;
        }
    }
}

async fn execute(connection: &Connection, own_id: u64, command: Command) -> Event {
    match command {
        Command::SendImage { friend, attachment } => Event::ImageSent {
            friend,
            result: upload::send(connection, friend, attachment).await,
        },
        Command::History { friend, before } => {
            let result = timeout(
                REQUEST_TIMEOUT,
                connection.service_method(CFriendMessages_GetRecentMessages_Request {
                    steamid1: Some(own_id),
                    steamid2: Some(friend),
                    count: Some(100),
                    bbcode_format: Some(false),
                    most_recent_conversation: Some(false),
                    time_last: before.map(|b| b.0),
                    ordinal_last: before.map(|b| b.1),
                    ..Default::default()
                }),
            )
            .await;
            match result {
                Ok(Ok(response)) => Event::History {
                    friend,
                    more: response.more_available(),
                    older: before.is_some(),
                    messages: response
                        .messages
                        .iter()
                        .map(|m| Message {
                            timestamp: m.timestamp(),
                            ordinal: m.ordinal(),
                            outgoing: m.accountid() == own_id as u32,
                            text: m.message().to_owned(),
                        })
                        .collect(),
                },
                _ => Event::HistoryFailed {
                    friend,
                    error: "Chat history unavailable; close and reopen to retry".into(),
                },
            }
        }
        Command::Send { friend, text } => {
            if text.trim().is_empty() || text.len() > MAX_MESSAGE_BYTES {
                return Event::Sent {
                    friend,
                    result: Err("Message must contain text and be at most 6000 bytes".into()),
                };
            }
            let response = timeout(
                REQUEST_TIMEOUT,
                connection.service_method(CFriendMessages_SendMessage_Request {
                    steamid: Some(friend),
                    chat_entry_type: Some(1),
                    message: Some(text.clone()),
                    contains_bbcode: Some(false),
                    echo_to_sender: Some(false),
                    ..Default::default()
                }),
            )
            .await;
            Event::Sent {
                friend,
                result: match response {
                    Ok(Ok(r)) => Ok(Message {
                        timestamp: r.server_timestamp(),
                        ordinal: r.ordinal(),
                        outgoing: true,
                        text: r
                            .message_without_bb_code
                            .or(r.modified_message)
                            .unwrap_or(text),
                    }),
                    Ok(Err(_)) => {
                        Err("Steam rejected the message or disconnected. Draft retained.".into())
                    }
                    Err(_) => Err(
                        "Send timed out; delivery is unknown. Check history before retrying."
                            .into(),
                    ),
                },
            }
        }
        Command::Read { friend, timestamp } => {
            match timeout(
                REQUEST_TIMEOUT,
                connection.send_service_notification(CFriendMessages_AckMessage_Notification {
                    steamid_partner: Some(friend),
                    timestamp: Some(timestamp),
                    ..Default::default()
                }),
            )
            .await
            {
                Ok(Ok(())) => Event::Read { friend, timestamp },
                _ => Event::ReadFailed { friend, timestamp },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use steam_vent::ServiceMethodRequest;

    #[test]
    fn read_acknowledgement_is_a_notification_and_echo_uses_the_client_service_name() {
        let ack = CFriendMessages_AckMessage_Notification {
            steamid_partner: Some(0x011000010000002a),
            timestamp: Some(0x12345678),
            ..Default::default()
        };
        let encoded = ack.write_to_bytes().unwrap();
        assert_eq!(
            encoded,
            [9, 42, 0, 0, 0, 1, 0, 16, 1, 16, 248, 172, 209, 145, 1]
        );
        let echo = AckEcho::parse(&mut encoded.as_slice()).unwrap();
        assert_eq!(echo.0.steamid_partner(), ack.steamid_partner());
        assert_eq!(echo.0.timestamp(), ack.timestamp());
        assert_eq!(
            CFriendMessages_AckMessage_Notification::REQ_NAME,
            "FriendMessages.AckMessage#1"
        );
        assert_eq!(
            AckEcho::NOTIFICATION_NAME,
            "FriendMessagesClient.NotifyAckMessageEcho#1"
        );
    }

    #[test]
    fn malformed_read_echo_is_rejected() {
        assert!(AckEcho::parse(&mut [9_u8, 1].as_slice()).is_err());
    }
    #[test]
    fn rich_messages_keep_image_urls_and_empty_plain_bodies_do_not_hide_content() {
        assert_eq!(incoming_text("hello", Some("")), "hello");
        let raw = "[img src=\"https://images.steamusercontent.com/ugc/123/ABC/\"][/img]";
        assert!(
            incoming_text(raw, Some("Image"))
                .contains("https://images.steamusercontent.com/ugc/123/ABC/")
        );
        assert_eq!(
            incoming_text(
                "https://example.test/image",
                Some("https://example.test/image")
            ),
            "https://example.test/image"
        );
    }
}
