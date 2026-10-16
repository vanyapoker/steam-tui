//! Prints a game's achievements for the account. Usage: achievements-probe ACCOUNT APP [LANGUAGE]
use std::error::Error;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut args = std::env::args().skip(1);
            let usage = "Usage: achievements-probe ACCOUNT APP [LANGUAGE]";
            let account = args.next().ok_or(usage)?;
            let app: u32 = args.next().ok_or(usage)?.parse()?;
            let language = args.next().unwrap_or_else(|| "english".into());
            let connection = steam_core::account::resume(&account).await?;
            let all = steam_core::achievements::fetch(&connection, app, &language).await?;
            println!(
                "{}/{} unlocked, rarity {}",
                all.unlocked(),
                all.items.len(),
                all.has_percentages
            );
            for a in all.items.iter().take(12) {
                println!(
                    "{} {:<28} {:>6} {:?} {:?}",
                    if a.unlocked { "✓" } else { "·" },
                    a.display_name,
                    a.percent.map_or("?".into(), |p| format!("{p:.1}%")),
                    a.unlock_time,
                    a.description
                );
            }
            Ok(())
        })
        .await
}
