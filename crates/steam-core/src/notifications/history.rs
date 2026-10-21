//! Account notification history over the existing authenticated CM transport.
use super::{MAX_RAW_BYTES, Result, failure};
use protobuf::{CodedInputStream, CodedOutputStream, Message, UnknownFields};
use std::io::{Read, Write};
use steam_vent::{Connection, ConnectionTrait, ServiceMethodRequest};
use steam_vent_proto_common::RpcMessage;
use steam_vent_proto_steam::steammessages_notifications_steamclient::CSteamNotification_NotificationsReceived_Notification;

/// The protocol crate contains the push message but omits this service method.
#[derive(Debug)]
pub(super) struct HistoryRequest;

impl ServiceMethodRequest for HistoryRequest {
    const REQ_NAME: &'static str = "SteamNotification.GetSteamNotifications#1";
    type Response = HistoryResponse;

    fn write(&self, writer: &mut dyn Write) -> std::io::Result<()> {
        // hidden=false, language=English, confirmations=false, pinned counts=true,
        // include read=true, count only=false. There is no pagination parameter.
        writer.write_all(&[8, 0, 16, 0, 24, 0, 32, 1, 40, 1, 48, 0])
    }

    fn encode_size(&self) -> usize {
        12
    }
}

#[derive(Default)]
pub(super) struct HistoryResponse(pub CSteamNotification_NotificationsReceived_Notification);

impl std::fmt::Debug for HistoryResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HistoryResponse(<private>)")
    }
}

impl RpcMessage for HistoryResponse {
    fn parse(reader: &mut dyn Read) -> protobuf::Result<Self> {
        let mut bytes = Vec::new();
        reader
            .take(MAX_RAW_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_RAW_BYTES {
            return Err(std::io::Error::other("notification history is too large").into());
        }
        let mut input = CodedInputStream::from_bytes(&bytes);
        let mut message = CSteamNotification_NotificationsReceived_Notification::default();
        let mut unknown = UnknownFields::new();
        while let Some(tag) = input.read_raw_tag_or_eof()? {
            match tag {
                10 => message.notifications.push(input.read_message()?),
                // History and push responses use different field numbers.
                24 => message.pending_gift_count = Some(input.read_uint32()?),
                40 => message.pending_friend_count = Some(input.read_uint32()?),
                56 => message.pending_family_invite_count = Some(input.read_uint32()?),
                _ => {
                    protobuf::rt::read_unknown_or_skip_group(tag, &mut input, &mut unknown)?;
                }
            }
        }
        Ok(Self(message))
    }

    fn write(&self, writer: &mut dyn Write) -> protobuf::Result<()> {
        let mut output = CodedOutputStream::new(writer);
        for note in &self.0.notifications {
            output.write_bytes(1, &note.write_to_bytes()?)?;
        }
        for (field, value) in [
            (3, self.0.pending_gift_count),
            (5, self.0.pending_friend_count),
            (7, self.0.pending_family_invite_count),
        ] {
            if let Some(value) = value {
                output.write_uint32(field, value)?;
            }
        }
        output.flush()
    }

    fn encode_size(&self) -> usize {
        self.0
            .notifications
            .iter()
            .map(|note| {
                let size = note.compute_size();
                1 + protobuf::rt::compute_raw_varint64_size(size) as usize + size as usize
            })
            .sum::<usize>()
            + [
                self.0.pending_gift_count,
                self.0.pending_friend_count,
                self.0.pending_family_invite_count,
            ]
            .into_iter()
            .flatten()
            .map(|value| 1 + protobuf::rt::compute_raw_varint64_size(u64::from(value)) as usize)
            .sum::<usize>()
    }
}

pub(super) async fn fetch(connection: &Connection) -> Result<HistoryResponse> {
    tokio::time::timeout(
        super::REQUEST_TIMEOUT,
        connection.service_method(HistoryRequest),
    )
    .await
    .map_err(|_| failure("Steam notification history request timed out"))?
    .map_err(|_| failure("Steam notification history request failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use steam_vent_proto_steam::steammessages_notifications_steamclient::SteamNotificationData;

    #[test]
    fn request_includes_read_history_and_pinned_counts() {
        let mut bytes = Vec::new();
        ServiceMethodRequest::write(&HistoryRequest, &mut bytes).unwrap();
        assert_eq!(bytes, [8, 0, 16, 0, 24, 0, 32, 1, 40, 1, 48, 0]);
        assert_eq!(bytes.len(), HistoryRequest.encode_size());
    }

    #[test]
    fn response_maps_history_fields_without_confusing_unread_and_gift_counts() {
        let mut bytes = Vec::new();
        let mut output = CodedOutputStream::vec(&mut bytes);
        for id in 1..=150 {
            output
                .write_bytes(
                    1,
                    &SteamNotificationData {
                        notification_id: Some(id),
                        read: Some(true),
                        ..Default::default()
                    }
                    .write_to_bytes()
                    .unwrap(),
                )
                .unwrap();
        }
        for (field, value) in [(2, 999), (3, 2), (5, 3), (6, 555), (7, 4), (100, 42)] {
            output.write_uint32(field, value).unwrap();
        }
        output.flush().unwrap();
        drop(output);
        let response = HistoryResponse::parse(&mut bytes.as_slice()).unwrap();
        assert_eq!(response.0.notifications.len(), 150);
        assert_eq!(response.0.pending_gift_count, Some(2));
        assert_eq!(response.0.pending_friend_count, Some(3));
        assert_eq!(response.0.pending_family_invite_count, Some(4));
        let mut roundtrip = Vec::new();
        response.write(&mut roundtrip).unwrap();
        assert_eq!(response.encode_size(), roundtrip.len());
        assert_eq!(
            HistoryResponse::parse(&mut roundtrip.as_slice()).unwrap().0,
            response.0
        );
        assert!(HistoryResponse::parse(&mut &[10, 100, 1][..]).is_err());
    }
}
