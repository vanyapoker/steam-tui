//! Cheap session liveness check. steam-vent exposes no connection-state API, and
//! a socket that died silently only shows up as requests that never answer.
//! A probe is one small service call (a single CDN directory entry) raced
//! against a timeout; any answer, even an API error code from Steam, proves the
//! session carries traffic, while a transport error or silence does not.

use std::time::Duration;

use steam_vent::{Connection, ConnectionTrait, NetworkError};
use steam_vent_proto_steam::steammessages_contentsystem_steamclient::CContentServerDirectory_GetServersForSteamPipe_Request;

/// Default wait for a probe answer.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Whether a failed probe call still shows an answering session: Steam replied
/// (an error result, or a message this request did not expect).
fn answered(error: &NetworkError) -> bool {
    matches!(
        error,
        NetworkError::ApiError(_)
            | NetworkError::DifferentMessage(..)
            | NetworkError::DifferentServiceMethod(..)
            | NetworkError::MalformedBody(_)
    )
}

/// True when the session answered within `timeout`. Never logs or returns data.
pub async fn probe(connection: &Connection, timeout: Duration) -> bool {
    let request =
        connection.service_method(CContentServerDirectory_GetServersForSteamPipe_Request {
            cell_id: Some(connection.cell_id()),
            max_servers: Some(1),
            ..Default::default()
        });
    match tokio::time::timeout(timeout, request).await {
        Ok(Ok(_)) => true,
        Ok(Err(error)) => answered(&error),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use steam_vent::EResult;

    #[test]
    fn answers_prove_liveness_but_transport_failures_do_not() {
        assert!(answered(&NetworkError::ApiError(EResult::Fail)));
        assert!(!answered(&NetworkError::EOF));
        assert!(!answered(&NetworkError::Timeout));
        assert!(!answered(&NetworkError::IO(std::io::Error::other("reset"))));
    }
}
