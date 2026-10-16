//! Interactive Steam authentication with refresh tokens kept only in Secret Service.
//!
//! Passwords and Guard machine data are never persisted. `ephemeral` bypasses
//! Secret Service entirely. Logout deletes local credentials, not server sessions.

mod store;
mod terminal;

use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::future::{Future, pending};
use std::time::Duration;

use steam_vent::auth::{
    AuthConfirmationHandler, ClientInfo, ConfirmationAction, ConfirmationMethod,
    ConfirmationMethodClass, GuardDataStore, Os, RefreshToken, SteamGuardToken,
};
use steam_vent::{Connection, ConnectionError, EResult, LoginError, NetworkError, ServerList};
use zeroize::Zeroize;

use store::TokenStore;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(45);
const AUTH_TIMEOUT: Duration = Duration::from_secs(180);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_GUARD_ATTEMPTS: usize = 3;

#[derive(Debug)]
enum AccountError {
    Message(&'static str),
    Unavailable,
    Locked,
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Message(message) => message,
            Self::Unavailable => "Secret Service is unavailable; start an unlocked Secret Service provider or use --ephemeral",
            Self::Locked => "Secret Service credentials are locked; unlock your keyring and retry, or explicitly use --ephemeral",
        })
    }
}

impl Error for AccountError {}

type Result<T, E = AccountError> = std::result::Result<T, E>;

/// Log in using a saved refresh token, or an interactive password/Guard flow.
///
/// Network operations and Guard submissions are bounded. Invalid credentials
/// trigger at most one password login; transient failures leave saved tokens
/// untouched. Every successful persistent login stores the token returned by
/// the connection, including any rotation supported by the upstream library.
///
/// The upstream reader task has no public shutdown API. Drop all connection
/// clones and shut down the owning Tokio runtime when this CLI operation ends.
pub async fn login(
    account: &str,
    ephemeral: bool,
) -> std::result::Result<Connection, Box<dyn Error>> {
    Ok(login_inner(account, ephemeral).await?)
}

/// Resume a saved session without terminal prompts or status output.
///
/// Suitable for TUI startup. Missing, expired, or rejected credentials require
/// the explicit `login` command; the saved entry is preserved on failure.
pub async fn resume(account: &str) -> std::result::Result<Connection, Box<dyn Error>> {
    let account = normalize_account(account)?;
    let store = TokenStore::open().await?;
    let bytes = store.load(&account).await?.ok_or(AccountError::Message(
        "No saved refresh token for this account; run steam-tui login first",
    ))?;
    let raw = std::str::from_utf8(&bytes).map_err(|_| {
        AccountError::Message(
            "Saved refresh token is not valid text; run steam-tui login to replace it",
        )
    })?;
    let token = RefreshToken::new(raw.to_owned()).map_err(|_| {
        AccountError::Message("Saved refresh token is malformed; run steam-tui login to replace it")
    })?;
    if token.expired() {
        return Err(AccountError::Message(
            "Saved refresh token has expired; run steam-tui login again",
        )
        .into());
    }
    let servers = bounded(DISCOVERY_TIMEOUT, ServerList::discover())
        .await?
        .map_err(|_| AccountError::Message("Steam server discovery failed; check connectivity"))?;
    let result = bounded(
        TOKEN_TIMEOUT,
        Connection::login_with_refresh_token(&servers, &token),
    )
    .await?;
    let connection = result.map_err(|error| {
        if refresh_rejected(&error) {
            AccountError::Message(
                "Steam rejected the saved refresh token; run steam-tui login again",
            )
        } else {
            sanitize_connection_error(&error)
        }
    })?;
    Ok(persist_connection(Some(&store), &account, connection, false).await?)
}

async fn login_inner(account: &str, ephemeral: bool) -> Result<Connection> {
    let account = normalize_account(account)?;
    let store = if ephemeral {
        eprintln!("Ephemeral login: refresh tokens will not be loaded or saved.");
        None
    } else {
        match TokenStore::open().await {
            Ok(store) => Some(store),
            Err(AccountError::Unavailable) => {
                eprintln!(
                    "Secret Service is unavailable. No plaintext credential fallback is used."
                );
                let answer =
                    terminal::prompt("Continue without saving credentials? [y/N]: ").await?;
                if !answer.eq_ignore_ascii_case("y") && !answer.eq_ignore_ascii_case("yes") {
                    return Err(AccountError::Unavailable);
                }
                eprintln!("Ephemeral login selected: refresh tokens will not be loaded or saved.");
                None
            }
            Err(error) => return Err(error),
        }
    };

    let saved = match &store {
        Some(store) => store.load(&account).await?,
        None => None,
    };
    let saved_token = match saved {
        Some(bytes) => match std::str::from_utf8(&bytes) {
            Ok(raw) => match RefreshToken::new(raw.to_owned()) {
                Ok(token) if !token.expired() => Some(token),
                Ok(_) => {
                    eprintln!(
                        "Saved refresh token has expired; password login is required. The saved entry is unchanged until login succeeds."
                    );
                    None
                }
                Err(_) => {
                    eprintln!(
                        "Saved refresh token is malformed; password login is required. The saved entry is unchanged until login succeeds."
                    );
                    None
                }
            },
            Err(_) => {
                return Err(AccountError::Message(
                    "Saved refresh token is not valid text; remove it with logout or explicitly use --ephemeral",
                ));
            }
        },
        None => None,
    };

    let servers = bounded(DISCOVERY_TIMEOUT, ServerList::discover())
        .await?
        .map_err(|_| {
            AccountError::Message("Steam server discovery failed; check your network and retry")
        })?;

    if let Some(token) = saved_token {
        eprintln!("Trying saved refresh token.");
        let result = bounded(
            TOKEN_TIMEOUT,
            Connection::login_with_refresh_token(&servers, &token),
        )
        .await?;
        match result {
            Ok(connection) => {
                return persist_connection(store.as_ref(), &account, connection, true).await;
            }
            Err(error) if refresh_rejected(&error) => {
                eprintln!(
                    "Steam rejected the saved refresh token; password login is required. The saved entry is unchanged until login succeeds."
                );
            }
            Err(error) => return Err(sanitize_connection_error(&error)),
        }
    }

    let password = terminal::prompt("Steam password (hidden; Ctrl-C cancels): ").await?;
    if password.is_empty() {
        return Err(AccountError::Message(
            "Login cancelled: no password entered",
        ));
    }
    let mut handler = SafeConfirmation::default();
    let mut client = ClientInfo::new("steam-tui".to_owned());
    client.os = Os::Linux;
    let result = bounded(
        AUTH_TIMEOUT,
        Connection::login(
            &servers,
            &account,
            &password,
            DiscardGuardData,
            &mut handler,
            &client,
        ),
    )
    .await?;
    drop(password);
    let connection = result.map_err(|error| {
        handler
            .error
            .take()
            .unwrap_or_else(|| sanitize_connection_error(&error))
    })?;
    persist_connection(store.as_ref(), &account, connection, true).await
}

async fn persist_connection(
    store: Option<&TokenStore>,
    account: &str,
    connection: Connection,
    announce: bool,
) -> Result<Connection> {
    let token = connection.refresh_token().ok_or(AccountError::Message(
        "Steam authenticated the session without a refresh token; no credential was saved",
    ))?;
    if let Some(store) = store {
        store.save(account, token.token().as_bytes()).await?;
        if announce {
            eprintln!(
                "Refresh token saved in Secret Service. Password and Guard machine data were not saved."
            );
        }
    }
    Ok(connection)
}

/// Remove only this application's local refresh-token entries for `account`.
///
/// This does not contact Steam, revoke tokens, or log out other sessions.
pub async fn logout(account: &str) -> std::result::Result<(), Box<dyn Error>> {
    let account = normalize_account(account)?;
    TokenStore::open_for_logout()
        .await?
        .delete(&account)
        .await?;
    eprintln!("Local refresh token removed (if present). No server-side revocation was requested.");
    Ok(())
}

fn normalize_account(account: &str) -> Result<String> {
    let account = account.trim();
    if account.is_empty() || !account.is_ascii() || account.chars().any(char::is_control) {
        return Err(AccountError::Message(
            "Enter a nonempty Steam account login name without control characters",
        ));
    }
    Ok(account.to_ascii_lowercase())
}

async fn bounded<T>(duration: Duration, future: impl Future<Output = T>) -> Result<T> {
    tokio::select! {
        result = tokio::time::timeout(duration, future) => result.map_err(|_| AccountError::Message("Steam authentication timed out; no saved credential was removed. Retry when ready")),
        _ = tokio::signal::ctrl_c() => Err(AccountError::Message("Login cancelled; no saved credential was removed")),
    }
}

fn refresh_rejected(error: &ConnectionError) -> bool {
    matches!(
        error,
        ConnectionError::LoginError(LoginError::InvalidCredentials)
            | ConnectionError::LoginError(LoginError::AccessToken(_))
            | ConnectionError::LoginError(LoginError::Unknown(
                EResult::Expired
                    | EResult::Revoked
                    | EResult::InvalidPassword
                    | EResult::RequirePasswordReEntry
            ))
            | ConnectionError::Network(NetworkError::ApiError(
                EResult::Expired
                    | EResult::Revoked
                    | EResult::InvalidPassword
                    | EResult::RequirePasswordReEntry
            ))
    )
}

fn sanitize_connection_error(error: &ConnectionError) -> AccountError {
    let message = match error {
        ConnectionError::Aborted => "Login cancelled; no saved credential was removed",
        ConnectionError::LoginError(LoginError::InvalidCredentials)
        | ConnectionError::Network(NetworkError::ApiError(EResult::InvalidPassword)) => {
            "Steam rejected the credentials; check the account name and password"
        }
        ConnectionError::LoginError(LoginError::RateLimited)
        | ConnectionError::Network(NetworkError::ApiError(EResult::RateLimitExceeded)) => {
            "Steam rate-limited authentication; wait before trying again"
        }
        ConnectionError::LoginError(LoginError::SteamGuardRequired)
        | ConnectionError::UnsupportedConfirmationAction(_) => {
            "Steam requires a confirmation method this login cannot provide"
        }
        ConnectionError::LoginError(LoginError::UnavailableAccount) => {
            "This Steam account is unavailable for login"
        }
        ConnectionError::Network(NetworkError::Timeout) => {
            "Steam did not respond in time; retry later. The saved credential was not removed"
        }
        ConnectionError::Network(_) => {
            "Steam network authentication failed; check connectivity and retry. The saved credential was not removed"
        }
        _ => {
            "Steam authentication failed; retry later or check your account in the official Steam client. The saved credential was not removed"
        }
    };
    AccountError::Message(message)
}

struct DiscardGuardData;

impl GuardDataStore for DiscardGuardData {
    type Err = Infallible;

    async fn store(
        &mut self,
        _account: &str,
        mut machine_token: String,
    ) -> std::result::Result<(), Self::Err> {
        machine_token.zeroize();
        Ok(())
    }

    async fn load(&mut self, _account: &str) -> std::result::Result<Option<String>, Self::Err> {
        Ok(None)
    }
}

#[derive(Default)]
struct SafeConfirmation {
    attempts: usize,
    error: Option<AccountError>,
}

impl AuthConfirmationHandler for SafeConfirmation {
    fn handle_confirmation<'this>(
        &'this mut self,
        allowed: &[ConfirmationMethod],
    ) -> Box<dyn Future<Output = Option<ConfirmationAction>> + 'this> {
        // Do not print Steam-provided confirmation details: they are untrusted
        // terminal text and can contain identifying account information.
        if allowed
            .iter()
            .any(|method| method.class() == ConfirmationMethodClass::Confirmation)
        {
            eprintln!(
                "Approve this login in your Steam mobile app or confirmation email (3-minute login limit; Ctrl-C cancels)."
            );
            return Box::new(pending());
        }
        let token_type = allowed.iter().find_map(ConfirmationMethod::token_type);
        Box::new(async move {
            let token_type = token_type?;
            if self.attempts >= MAX_GUARD_ATTEMPTS {
                self.error = Some(AccountError::Message(
                    "Steam Guard confirmation failed after three submissions; retry login with a fresh code",
                ));
                return Some(ConfirmationAction::Abort);
            }
            self.attempts += 1;
            if self.attempts > 1 {
                eprintln!("Steam did not accept the previous Guard code; enter a fresh code.");
            }
            match terminal::prompt("Steam Guard code (hidden; Ctrl-C cancels): ").await {
                Ok(code) if !code.is_empty() => {
                    // steam-vent takes ownership of a String; that upstream copy
                    // cannot be zeroized here. Our prompt buffer is zeroized.
                    Some(ConfirmationAction::GuardToken(
                        SteamGuardToken::new(code.to_string()),
                        token_type,
                    ))
                }
                Ok(_) => Some(ConfirmationAction::Abort),
                Err(error) => {
                    self.error = Some(error);
                    Some(ConfirmationAction::Abort)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_refresh_failures_do_not_request_password_recovery() {
        for error in [
            ConnectionError::Network(NetworkError::Timeout),
            ConnectionError::Network(NetworkError::EOF),
            ConnectionError::LoginError(LoginError::RateLimited),
            ConnectionError::LoginError(LoginError::Unknown(EResult::AccessDenied)),
        ] {
            assert!(!refresh_rejected(&error));
        }
    }

    #[test]
    fn rejected_or_expired_refresh_tokens_request_one_interactive_login() {
        for error in [
            ConnectionError::LoginError(LoginError::InvalidCredentials),
            ConnectionError::LoginError(LoginError::AccessToken(
                steam_vent::auth::RefreshTokenError::Expired,
            )),
            ConnectionError::LoginError(LoginError::Unknown(EResult::Revoked)),
            ConnectionError::Network(NetworkError::ApiError(EResult::Expired)),
        ] {
            assert!(refresh_rejected(&error));
        }
    }

    #[test]
    fn secret_service_account_identity_is_case_insensitive() {
        assert_eq!(normalize_account("  SteamUser  ").unwrap(), "steamuser");
        assert!(normalize_account("\u{1b}account").is_err());
        assert!(normalize_account(" ").is_err());
    }
}
