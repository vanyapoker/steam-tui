//! Private branch access stays account-scoped. Passwords are submitted once;
//! only returned access keys may enter SecretService. Private depot sections are
//! merged into an in-memory planning document, never the public PICS cache.
use crate::installed::{Node, Result, failure};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver_2::{
    CMsgClientCheckAppBetaPassword, CMsgClientCheckAppBetaPasswordResponse,
};
use steam_vent_proto_steam::steammessages_clientserver_appinfo::{
    CMsgClientPICSAccessTokenRequest, CMsgClientPICSAccessTokenResponse,
    CMsgClientPICSPrivateBetaRequest, CMsgClientPICSPrivateBetaResponse,
};
use zeroize::{Zeroize, Zeroizing};

// The installed proto crate has these messages but predates their EMsg entries.
// Message kinds are confirmed by SteamKit's current generated SteamLanguage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PrivateKind {
    #[default]
    Request,
    Response,
}
impl protobuf::Enum for PrivateKind {
    const NAME: &'static str = "PrivateBetaMessage";
    const VALUES: &'static [Self] = &[Self::Request, Self::Response];
    fn value(&self) -> i32 {
        match self {
            Self::Request => 8907,
            Self::Response => 8908,
        }
    }
    fn from_i32(v: i32) -> Option<Self> {
        match v {
            8907 => Some(Self::Request),
            8908 => Some(Self::Response),
            _ => None,
        }
    }
    fn from_str(v: &str) -> Option<Self> {
        match v {
            "Request" => Some(Self::Request),
            "Response" => Some(Self::Response),
            _ => None,
        }
    }
}
impl steam_vent_proto_common::MsgKindEnum for PrivateKind {}
struct PrivateRequest(CMsgClientPICSPrivateBetaRequest);
struct PrivateResponse(CMsgClientPICSPrivateBetaResponse);
struct BetaRequest(CMsgClientCheckAppBetaPassword);
struct BetaResponse(CMsgClientCheckAppBetaPasswordResponse);
macro_rules! wire {
    ($name:ident, $inner:ty, $enum:ty, $kind:expr) => {
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }
        impl steam_vent_proto_common::RpcMessage for $name {
            fn parse(reader: &mut dyn std::io::Read) -> protobuf::Result<Self> {
                Ok(Self(<$inner as protobuf::Message>::parse_from_reader(
                    reader,
                )?))
            }
            fn write(&self, writer: &mut dyn std::io::Write) -> protobuf::Result<()> {
                protobuf::Message::write_to_writer(&self.0, writer)
            }
            fn encode_size(&self) -> usize {
                protobuf::Message::compute_size(&self.0) as usize
            }
        }
        impl steam_vent_proto_common::RpcMessageWithKind for $name {
            type KindEnum = $enum;
            const KIND: Self::KindEnum = $kind;
        }
    };
}
wire!(
    PrivateRequest,
    CMsgClientPICSPrivateBetaRequest,
    PrivateKind,
    PrivateKind::Request
);
wire!(
    PrivateResponse,
    CMsgClientPICSPrivateBetaResponse,
    PrivateKind,
    PrivateKind::Response
);
wire!(
    BetaRequest,
    CMsgClientCheckAppBetaPassword,
    steam_vent_proto_steam::enums_clientserver::EMsg,
    steam_vent_proto_steam::enums_clientserver::EMsg::k_EMsgClientCheckAppBetaPassword
);
wire!(
    BetaResponse,
    CMsgClientCheckAppBetaPasswordResponse,
    steam_vent_proto_steam::enums_clientserver::EMsg,
    steam_vent_proto_steam::enums_clientserver::EMsg::k_EMsgClientCheckAppBetaPasswordResponse
);
impl Drop for BetaRequest {
    fn drop(&mut self) {
        if let Some(password) = &mut self.0.betapassword {
            password.zeroize();
        }
    }
}
impl Drop for BetaResponse {
    fn drop(&mut self) {
        for returned in &mut self.0.betapasswords {
            if let Some(key) = &mut returned.betapassword {
                key.zeroize();
            }
        }
    }
}
impl Drop for PrivateRequest {
    fn drop(&mut self) {
        if let Some(key) = &mut self.0.password_hash {
            key.zeroize();
        }
    }
}

static KEYS: LazyLock<Mutex<BTreeMap<String, Zeroizing<Vec<u8>>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
fn identity(connection: &Connection, app: u32, branch: &str) -> String {
    format!("{}:{app}:{branch}", connection.steam_id().steam64())
}
fn validate(app: u32, branch: &str) -> Result<()> {
    if app == 0 {
        return Err(failure("App ID must be positive"));
    }
    crate::catalog::ContentSelection {
        branch: Some(branch.into()),
        language: None,
    }
    .validate()
    .map_err(|e| failure(e.to_string()))
}
fn decode_key(value: &str) -> Result<Zeroizing<Vec<u8>>> {
    if value.is_empty() || value.len() > 256 || !value.len().is_multiple_of(2) {
        return Err(failure("Invalid branch access key"));
    }
    let mut key = Zeroizing::new(Vec::with_capacity(value.len() / 2));
    for bytes in value.as_bytes().as_chunks::<2>().0 {
        let digit = |b: u8| {
            (b as char)
                .to_digit(16)
                .ok_or_else(|| failure("Invalid branch access key"))
        };
        key.push((digit(bytes[0])? * 16 + digit(bytes[1])?) as u8);
    }
    Ok(key)
}
fn remember(identity: String, key: Zeroizing<Vec<u8>>) {
    let mut keys = KEYS.lock();
    if keys.len() >= 256
        && !keys.contains_key(&identity)
        && let Some(old) = keys.keys().next().cloned()
    {
        keys.remove(&old);
    }
    keys.insert(identity, key);
}
async fn access_key(connection: &Connection, app: u32, branch: &str) -> Result<Zeroizing<Vec<u8>>> {
    let id = identity(connection, app, branch);
    if let Some(key) = KEYS.lock().get(&id) {
        return Ok(Zeroizing::new(key.to_vec()));
    }
    let store = crate::account::store::TokenStore::branch_keys()
        .await
        .map_err(|_| {
            failure("Unlock this private branch first; its keyring is unavailable or locked")
        })?;
    let key = store
        .load(&id)
        .await
        .map_err(|_| failure("Cannot read private branch access key"))?
        .ok_or_else(|| failure("Private branch is locked; use content unlock APPID BRANCH"))?;
    if key.is_empty() || key.len() > 128 {
        return Err(failure("Invalid stored branch access key"));
    }
    remember(id, Zeroizing::new(key.to_vec()));
    Ok(key)
}

/// Unlocking alone does not alter the installed or selected branch.
pub async fn unlock(
    connection: &Connection,
    app: u32,
    branch: &str,
    password: Zeroizing<String>,
    persist: bool,
) -> Result<()> {
    validate(app, branch)?;
    let library = crate::library::load(connection)
        .await
        .map_err(|_| failure("Cannot refresh branch ownership"))?;
    if !library.apps.iter().any(|a| a.app_id == app) {
        return Err(failure("No current license for this app"));
    }
    if password.is_empty() || password.len() > 4096 {
        return Err(failure("Invalid branch password length"));
    }
    let mut response: BetaResponse = tokio::time::timeout(
        Duration::from_secs(30),
        connection.job(BetaRequest(CMsgClientCheckAppBetaPassword {
            app_id: Some(app),
            betapassword: Some(password.to_string()),
            ..Default::default()
        })),
    )
    .await
    .map_err(|_| failure("Branch unlock timed out"))?
    .map_err(|_| failure("Branch unlock request failed"))?;
    drop(password);
    let response = &mut response.0;
    let mut found = None;
    if response.eresult() == 1 && response.betapasswords.len() <= 256 {
        for returned in &mut response.betapasswords {
            if returned.betaname() == branch {
                found = Some(decode_key(returned.betapassword()));
            }
            if let Some(value) = &mut returned.betapassword {
                value.zeroize();
            }
        }
    } else {
        for returned in &mut response.betapasswords {
            if let Some(value) = &mut returned.betapassword {
                value.zeroize();
            }
        }
    }
    let key = found.ok_or_else(|| failure("Password did not unlock the requested branch"))??;
    // Confirm the key with the private PICS endpoint before saving it.
    private_section(connection, app, branch, &key).await?;
    let id = identity(connection, app, branch);
    if persist {
        let store = crate::account::store::TokenStore::branch_keys().await.map_err(|_| failure("Cannot save branch key: unlock SecretService, or choose session-only mode with F2 in content properties"))?;
        store
            .save(&id, &key)
            .await
            .map_err(|_| failure("Cannot save branch key; no plaintext fallback was used"))?;
    }
    remember(id, key);
    Ok(())
}

async fn private_section(
    connection: &Connection,
    app: u32,
    branch: &str,
    key: &[u8],
) -> Result<Node> {
    let tokens: CMsgClientPICSAccessTokenResponse = tokio::time::timeout(
        Duration::from_secs(30),
        connection.job(CMsgClientPICSAccessTokenRequest {
            appids: vec![app],
            ..Default::default()
        }),
    )
    .await
    .map_err(|_| failure("Private branch token request timed out"))?
    .map_err(|_| failure("Private branch token request failed"))?;
    if tokens.app_denied_tokens.contains(&app) {
        return Err(failure("Private branch app access denied"));
    }
    let token = tokens
        .app_access_tokens
        .iter()
        .find(|t| t.appid() == app)
        .map(|t| t.access_token())
        .ok_or_else(|| failure("No app access token for private branch"))?;
    let response: PrivateResponse = tokio::time::timeout(
        Duration::from_secs(30),
        connection.job(PrivateRequest(CMsgClientPICSPrivateBetaRequest {
            appid: Some(app),
            access_token: Some(token),
            beta_name: Some(branch.into()),
            password_hash: Some(key.to_vec()),
            ..Default::default()
        })),
    )
    .await
    .map_err(|_| failure("Private branch metadata timed out"))?
    .map_err(|_| failure("Private branch metadata request failed"))?;
    let mut response = response.0;
    if response.eresult() != 1 {
        return Err(failure("Private branch access denied; unlock it again"));
    }
    let data = Zeroizing::new(response.depot_section.take().unwrap_or_default());
    if data.is_empty() || data.len() > 16 * 1024 * 1024 {
        return Err(failure("Private branch metadata is empty or too large"));
    }
    let text = std::str::from_utf8(&data)
        .map_err(|_| failure("Invalid private branch metadata text"))?
        .trim_end_matches('\0');
    crate::installed::text(text)
}

pub(crate) async fn apply(
    connection: &Connection,
    app: u32,
    branch: &str,
    document: &mut Node,
) -> Result<()> {
    validate(app, branch)?;
    let required = document
        .at(&["depots", "branches", branch])
        .and_then(|n| n.text("pwdrequired"))
        .is_some_and(|s| s != "0" && !s.eq_ignore_ascii_case("false"))
        || document
            .child("depots")
            .into_iter()
            .flat_map(|n| n.children.values())
            .any(|d| d.at(&["encryptedmanifests", branch]).is_some());
    if !required {
        return Ok(());
    }
    let key = access_key(connection, app, branch).await?;
    let private = private_section(connection, app, branch, &key).await?;
    merge(document, private, branch)
}
fn merge(document: &mut Node, mut private: Node, branch: &str) -> Result<()> {
    let depots = document
        .children
        .get_mut("depots")
        .ok_or_else(|| failure("Missing public depot metadata"))?;
    // The response is a depot section. Some producers wrap it in "depots".
    if let Some(wrapped) = private.children.remove("depots") {
        private = wrapped;
    }
    fn overlay(target: &mut Node, incoming: Node) {
        if incoming.value.is_some() {
            target.value = incoming.value;
        }
        for (key, value) in incoming.children {
            overlay(target.children.entry(key).or_default(), value);
        }
    }
    overlay(depots, private);
    let selected = depots
        .children
        .get_mut("branches")
        .and_then(|n| n.children.get_mut(branch))
        .ok_or_else(|| failure("Unlocked branch has no build metadata"))?;
    selected.children.remove("pwdrequired");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensitive_rpc_debug_is_redacted() {
        let request = BetaRequest(CMsgClientCheckAppBetaPassword {
            betapassword: Some("password-must-stay-private".into()),
            ..Default::default()
        });
        let private = PrivateRequest(CMsgClientPICSPrivateBetaRequest {
            password_hash: Some(b"key-must-stay-private".to_vec()),
            ..Default::default()
        });
        let text = format!("{request:?} {private:?}");
        assert!(text.contains("redacted"));
        assert!(!text.contains("password-must-stay-private"));
        assert!(!text.contains("key-must-stay-private"));
        let response = BetaResponse(Default::default());
        assert_eq!(format!("{response:?}"), "BetaResponse(<redacted>)");
    }
    #[test]
    fn private_overlay_retains_public_platform_and_only_unlocks_selected_branch() {
        let mut doc = crate::installed::text(r#""depots" { "branches" { "secret" { "buildid" "1" "pwdrequired" "1" } "other" { "pwdrequired" "1" } } "10" { "config" { "oslist" "linux" } "manifests" { "public" { "gid" "2" } } } }"#).unwrap();
        let private =
            crate::installed::text(r#""10" { "manifests" { "secret" { "gid" "3" } } }"#).unwrap();
        merge(&mut doc, private, "secret").unwrap();
        assert_eq!(
            doc.at(&["depots", "10", "config"]).unwrap().text("oslist"),
            Some("linux")
        );
        assert_eq!(
            doc.at(&["depots", "10", "manifests", "secret"])
                .unwrap()
                .text("gid"),
            Some("3")
        );
        assert!(
            doc.at(&["depots", "branches", "secret"])
                .unwrap()
                .text("pwdrequired")
                .is_none()
        );
        assert_eq!(
            doc.at(&["depots", "branches", "other"])
                .unwrap()
                .text("pwdrequired"),
            Some("1")
        );
        assert_eq!(&*decode_key("0123aB").unwrap(), &[1, 35, 171]);
        for bad in ["", "0", "xz"] {
            assert!(decode_key(bad).is_err());
        }
    }
}
