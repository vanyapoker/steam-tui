use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use secret_service::{EncryptionType, SecretService};
use zeroize::Zeroizing;

use super::{AccountError, Result};

const SERVICE: &str = "steam-tui";
const STORE_TIMEOUT: Duration = Duration::from_secs(15);
const CONTENT_TYPE: &str = "application/x-steam-refresh-token";

pub(crate) struct TokenStore {
    connection: SecretService<'static>,
    service: String,
    kind: String,
}

impl TokenStore {
    pub(crate) async fn open() -> Result<Self> {
        let store = Self::open_for_logout().await?;
        // Refuse to silently treat a locked or absent keyring as empty. Only an
        // unavailable provider/default collection offers explicit ephemeral mode.
        let collection = operation(store.connection.get_default_collection()).await?;
        operation(collection.ensure_unlocked()).await?;
        drop(collection);
        Ok(store)
    }

    pub(super) async fn open_for_logout() -> Result<Self> {
        let connection =
            tokio::time::timeout(STORE_TIMEOUT, SecretService::connect(EncryptionType::Dh))
                .await
                .map_err(|_| AccountError::Unavailable)?
                .map_err(|_| AccountError::Unavailable)?;
        Ok(Self {
            connection,
            service: SERVICE.to_owned(),
            kind: "refresh-token".into(),
        })
    }

    pub(crate) async fn branch_keys() -> Result<Self> {
        let mut store = Self::open().await?;
        store.kind = "branch-access-key".into();
        Ok(store)
    }

    fn attributes<'a>(&'a self, account: &'a str) -> HashMap<&'a str, &'a str> {
        HashMap::from([
            ("service", self.service.as_str()),
            ("account", account),
            ("kind", self.kind.as_str()),
        ])
    }

    pub(crate) async fn load(&self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let items = operation(self.connection.search_items(self.attributes(account))).await?;
        if !items.locked.is_empty() {
            return Err(AccountError::Locked);
        }
        if items.unlocked.len() > 1 {
            return Err(AccountError::Message(
                "Multiple saved refresh tokens match this account; use logout to remove those entries before logging in",
            ));
        }
        match items.unlocked.first() {
            Some(item) => Ok(Some(Zeroizing::new(operation(item.get_secret()).await?))),
            None => Ok(None),
        }
    }

    pub(crate) async fn save(&self, account: &str, token: &[u8]) -> Result<()> {
        let items = operation(self.connection.search_items(self.attributes(account))).await?;
        if !items.locked.is_empty() {
            return Err(AccountError::Locked);
        }
        match items.unlocked.as_slice() {
            [item] => {
                operation(item.set_secret(
                    token,
                    if self.kind == "refresh-token" {
                        CONTENT_TYPE
                    } else {
                        "application/x-steam-branch-access-key"
                    },
                ))
                .await
            }
            [] => {
                let collection = operation(self.connection.get_default_collection()).await?;
                operation(collection.ensure_unlocked()).await?;
                operation(collection.create_item(
                    if self.kind == "refresh-token" {
                        "steam-tui Steam refresh token"
                    } else {
                        "steam-tui branch access key"
                    },
                    self.attributes(account),
                    token,
                    true,
                    if self.kind == "refresh-token" {
                        CONTENT_TYPE
                    } else {
                        "application/x-steam-branch-access-key"
                    },
                ))
                .await?;
                Ok(())
            }
            _ => Err(AccountError::Message(
                "Multiple saved refresh tokens match this account; the newly returned token could not be saved. Use logout before logging in again",
            )),
        }
    }

    pub(super) async fn delete(&self, account: &str) -> Result<()> {
        let items = operation(self.connection.search_items(self.attributes(account))).await?;
        if !items.locked.is_empty() {
            return Err(AccountError::Locked);
        }
        for item in items.unlocked {
            operation(item.delete()).await?;
        }
        Ok(())
    }
}

async fn operation<T>(
    future: impl Future<Output = std::result::Result<T, secret_service::Error>>,
) -> Result<T> {
    tokio::time::timeout(STORE_TIMEOUT, future)
        .await
        .map_err(|_| AccountError::Message("Secret Service timed out; credential storage outcome may be unknown. Check your keyring before retrying"))?
        .map_err(|error| match error {
            secret_service::Error::Locked => AccountError::Locked,
            secret_service::Error::Unavailable | secret_service::Error::NoResult => AccountError::Unavailable,
            secret_service::Error::Prompt => AccountError::Message("Secret Service authorization was cancelled; no plaintext fallback was used"),
            _ => AccountError::Message("Secret Service operation failed; check your keyring. No plaintext fallback was used"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    // Uses a unique application namespace and only synthetic credentials. Run
    // explicitly in an unlocked real Secret Service desktop session.
    #[tokio::test]
    #[ignore = "requires an unlocked real Secret Service default collection"]
    async fn secret_service_token_lifecycle_isolated() {
        let mut store = TokenStore::open()
            .await
            .expect("Secret Service must be available and unlocked");
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        store.service = format!("steam-tui-test-{}-{suffix}", std::process::id());
        let mut other_service = TokenStore::open()
            .await
            .expect("Secret Service must remain available");
        other_service.service = format!("{}-other", store.service);
        let account = "synthetic-account";
        let unrelated = "other-synthetic-account";

        let result: Result<()> = async {
            ensure(store.load(account).await?.is_none(), "new namespace was not empty")?;
            store.save(account, b"synthetic-refresh-initial").await?;
            ensure(matches!(store.load(account).await?, Some(value) if value.as_slice() == b"synthetic-refresh-initial"), "saved synthetic credential was not loaded")?;
            store.save(account, b"synthetic-refresh-rotated").await?;
            ensure(matches!(store.load(account).await?, Some(value) if value.as_slice() == b"synthetic-refresh-rotated"), "rotated synthetic credential was not loaded")?;
            store.save(unrelated, b"synthetic-unrelated-account").await?;
            other_service.save(account, b"synthetic-unrelated-service").await?;
            store.delete(account).await?;
            ensure(store.load(account).await?.is_none(), "deleted synthetic credential remained")?;
            ensure(matches!(store.load(unrelated).await?, Some(value) if value.as_slice() == b"synthetic-unrelated-account"), "logout removed an unrelated account")?;
            ensure(matches!(other_service.load(account).await?, Some(value) if value.as_slice() == b"synthetic-unrelated-service"), "logout removed an unrelated service")?;
            store.delete(account).await?;
            Ok(())
        }.await;

        // Always attempt every cleanup before reporting any assertion failure.
        let cleanup_account = store.delete(account).await;
        let cleanup_unrelated = store.delete(unrelated).await;
        let cleanup_service = other_service.delete(account).await;
        assert!(
            cleanup_account.is_ok() && cleanup_unrelated.is_ok() && cleanup_service.is_ok(),
            "synthetic Secret Service test cleanup failed"
        );
        assert!(
            result.is_ok(),
            "synthetic Secret Service credential lifecycle failed"
        );
    }

    fn ensure(condition: bool, message: &'static str) -> Result<()> {
        if condition {
            Ok(())
        } else {
            Err(AccountError::Message(message))
        }
    }
}
