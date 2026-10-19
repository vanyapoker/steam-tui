#[cfg(not(target_os = "linux"))]
compile_error!("steam-core supports Linux only");

pub mod account;
pub mod achievements;
pub mod backend;
pub mod branches;
pub mod catalog;
pub mod cloud;
pub mod content;
pub mod content_options;
mod dlc;
pub mod friends;
pub mod install;
pub mod installed;
pub mod launch;
pub mod library;
pub mod liveness;
pub mod news;
pub mod notifications;
pub mod presence;
pub mod shaders;
pub mod updates;

use std::error::Error;
use std::net::SocketAddr;
use std::time::Duration;

use steam_vent::ServerList;

/// Endpoints selected by the backend's default server discovery policy.
pub struct DiscoveredEndpoints {
    pub tcp: SocketAddr,
    pub websocket: String,
}

/// Probe Steam's directory without signing in or reading account credentials.
pub async fn discover_servers() -> Result<DiscoveredEndpoints, Box<dyn Error>> {
    let servers = tokio::time::timeout(Duration::from_secs(20), ServerList::discover()).await??;
    Ok(DiscoveredEndpoints {
        tcp: servers.pick(),
        websocket: servers.pick_ws(),
    })
}

/// Exercise the same metadata path as the licensed library without an account.
pub async fn inspect_public_app(
    app_id: u32,
) -> Result<Option<library::LibraryApp>, Box<dyn Error>> {
    let servers = tokio::time::timeout(Duration::from_secs(20), ServerList::discover()).await??;
    let connection = tokio::time::timeout(
        Duration::from_secs(30),
        steam_vent::Connection::anonymous(&servers),
    )
    .await??;
    library::inspect_app(&connection, app_id).await
}
