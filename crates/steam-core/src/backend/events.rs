//! What the per-launch backend sends and receives, for display in the TUI.
//! Payload contents are summarised (ids, sizes, a hex head and the protobuf
//! wire fields); auth ticket bytes and credentials never appear here.
use std::time::SystemTime;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// game -> backend -> Steam
    ToSteam,
    /// Steam -> backend -> game
    FromSteam,
    /// answered by the backend itself
    Local,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub at: SystemTime,
    pub app_id: u32,
    pub direction: Direction,
    /// "GC", "CM" or "IPC".
    pub channel: &'static str,
    pub title: String,
    pub detail: String,
}
pub type EventSender = UnboundedSender<Event>;

pub(super) fn emit(
    events: &Option<EventSender>,
    app_id: u32,
    direction: Direction,
    channel: &'static str,
    title: impl Into<String>,
    detail: impl Into<String>,
) {
    if let Some(events) = events {
        let _ = events.send(Event {
            at: SystemTime::now(),
            app_id,
            direction,
            channel,
            title: title.into(),
            detail: detail.into(),
        });
    }
}

const PROTOBUF_FLAG: u32 = 0x8000_0000;

/// Names for the Game Coordinator messages shared by all games.
pub fn gc_message_name(id: u32) -> Option<&'static str> {
    Some(match id {
        21 => "SOCreate",
        22 => "SOUpdate",
        23 => "SODestroy",
        24 => "SOCacheSubscribed",
        25 => "SOCacheUnsubscribed",
        26 => "SOUpdateMultiple",
        27 => "SOCacheSubscriptionCheck",
        28 => "SOCacheSubscriptionRefresh",
        4004 => "ClientWelcome",
        4005 => "ServerWelcome",
        4006 => "ClientHello",
        4007 => "ServerHello",
        _ => return None,
    })
}

/// "ClientHello (4006, protobuf)" style title.
pub fn gc_title(msg_type: u32) -> String {
    let id = msg_type & !PROTOBUF_FLAG;
    let kind = if msg_type & PROTOBUF_FLAG != 0 {
        "protobuf"
    } else {
        "raw"
    };
    match gc_message_name(id) {
        Some(name) => format!("{name} ({id}, {kind})"),
        None => format!("message {id} ({kind})"),
    }
}

/// Hex head, then the protobuf fields of the body when the message is a
/// protobuf one: `[u32 type|flag][u32 header len][header][body]`.
pub fn gc_detail(msg_type: u32, payload: &[u8]) -> String {
    let mut out = format!("{} bytes", payload.len());
    out.push_str(" · hex ");
    out.push_str(&hex_head(payload, 24));
    if msg_type & PROTOBUF_FLAG != 0
        && let Some(len) = payload
            .get(4..8)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    {
        let header_end = 8usize.saturating_add(len as usize);
        if let Some(body) = payload.get(header_end..) {
            if len > 0
                && let Some(header) = payload.get(8..header_end)
            {
                out.push_str(&format!(" · header {}", wire_summary(header)));
            }
            out.push_str(&format!(" · body {}", wire_summary(body)));
        }
    }
    out
}

fn hex_head(bytes: &[u8], limit: usize) -> String {
    let mut out = String::new();
    for byte in bytes.iter().take(limit) {
        out.push_str(&format!("{byte:02x}"));
    }
    if bytes.len() > limit {
        out.push('…');
    }
    if out.is_empty() {
        out.push('-');
    }
    out
}

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Top-level protobuf fields as `f1=7 f2=bytes(12) f3=…`, bounded in size.
/// Nested messages are not decoded; malformed data ends the summary with `?`.
pub fn wire_summary(bytes: &[u8]) -> String {
    let mut at = 0;
    let mut parts: Vec<String> = Vec::new();
    while at < bytes.len() {
        if parts.len() >= 12 {
            parts.push("…".into());
            break;
        }
        let Some(tag) = varint(bytes, &mut at) else {
            parts.push("?".into());
            break;
        };
        let field = tag >> 3;
        let part = match tag & 7 {
            0 => varint(bytes, &mut at).map(|v| format!("f{field}={v}")),
            1 => bytes.get(at..at + 8).map(|b| {
                at += 8;
                format!(
                    "f{field}=fixed64({})",
                    u64::from_le_bytes(b.try_into().unwrap())
                )
            }),
            2 => varint(bytes, &mut at).and_then(|len| {
                let end = at.checked_add(len as usize)?;
                let data = bytes.get(at..end)?;
                at = end;
                Some(match std::str::from_utf8(data) {
                    Ok(text)
                        if !text.is_empty()
                            && text.len() <= 40
                            && text.chars().all(|c| !c.is_control()) =>
                    {
                        format!("f{field}=\"{text}\"")
                    }
                    _ => format!("f{field}=bytes({len})"),
                })
            }),
            5 => bytes.get(at..at + 4).map(|b| {
                at += 4;
                format!(
                    "f{field}=fixed32({})",
                    u32::from_le_bytes(b.try_into().unwrap())
                )
            }),
            _ => None,
        };
        match part {
            Some(part) => parts.push(part),
            None => {
                parts.push("?".into());
                break;
            }
        }
    }
    if parts.is_empty() {
        "empty".into()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_titles_and_wire_fields() {
        assert_eq!(gc_title(0x8000_0fa6), "ClientHello (4006, protobuf)");
        assert_eq!(gc_title(2500), "message 2500 (raw)");
        // field 1 varint 1, field 2 string "ab", field 3 bytes(3)
        let body = [
            0x08, 0x01, 0x12, 0x02, b'a', b'b', 0x1a, 0x03, 0xff, 0xff, 0xff,
        ];
        assert_eq!(wire_summary(&body), "f1=1 f2=\"ab\" f3=bytes(3)");
        assert_eq!(wire_summary(&[]), "empty");
        assert_eq!(wire_summary(&[0x12, 0x09, 1]), "?");
        let mut payload = 0x8000_0fa6u32.to_le_bytes().to_vec();
        payload.extend(0u32.to_le_bytes());
        payload.extend([0x08, 0x01]);
        let detail = gc_detail(0x8000_0fa6, &payload);
        assert!(
            detail.contains("10 bytes") && detail.contains("body f1=1"),
            "{detail}"
        );
    }
}
