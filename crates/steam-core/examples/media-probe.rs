//! Fetches an installed app's transcoded media databases exactly as a Proton
//! launch does, without starting Steam or any game. Uses the remembered login.
use std::collections::BTreeMap;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app: u32 = std::env::args()
        .nth(1)
        .ok_or("usage: media-probe APPID")?
        .parse()?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".config")
        });
    let name = std::fs::read_to_string(config.join("steam-tui/account"))?;
    let connection = steam_core::account::resume(name.trim())
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let game = steam_core::installed::scan(None, Some(name.trim()))?
        .games
        .into_iter()
        .find(|g| g.app_id == app)
        .ok_or("app must be installed")?;
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut plan = steam_core::launch::LaunchPlan {
        executable: "unused".into(),
        arguments: Vec::new(),
        working_dir: ".".into(),
        environment: BTreeMap::new(),
        needs_steam: false,
    };
    steam_core::media::prepare(&connection, &game, &mut plan, Some(&events)).await;
    drop(events);
    while let Some(event) = rx.recv().await {
        println!("{}: {}", event.channel, event.title);
    }
    println!(
        "STEAM_COMPAT_TRANSCODED_MEDIA_PATH={:?}",
        plan.environment.get("STEAM_COMPAT_TRANSCODED_MEDIA_PATH")
    );
    Ok(())
}
