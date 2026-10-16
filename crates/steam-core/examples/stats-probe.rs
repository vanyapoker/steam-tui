//! Read-only probe of the live UserStats protocol: stats and schema, current
//! players, global achievement percentages and (optionally) a leaderboard.
//! Never stores stats or uploads scores.
//! Usage: stats-probe ACCOUNT APP [LEADERBOARD_NAME ...]
use std::error::Error;
use steam_core::{account, backend::stats::Stats};
use steam_ipc::schema::Schema;
use steam_ipc::{Response, StatsOp};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut args = std::env::args().skip(1);
            let usage = "Usage: stats-probe ACCOUNT APP [LEADERBOARD_NAME ...]";
            let account = args.next().ok_or(usage)?;
            let app: u32 = args.next().ok_or(usage)?.parse()?;
            let boards: Vec<String> = args.collect();
            let connection = account::resume(&account).await?;
            let mut stats = Stats::new(app, None);

            match stats.handle(&connection, StatsOp::GetStats { user: 0 }).await {
                Response::StatsData { result, crc, schema, stats: values, unlocks } => {
                    println!("GetStats: eresult {result}, crc {crc}, {} stat values, {} unlock blocks, schema {} bytes", values.len(), unlocks.len(), schema.len() / 2);
                    let bytes = steam_ipc::hex_decode(&schema).unwrap_or_default();
                    match Schema::parse(&bytes, "english") {
                        Some(schema) => {
                            println!("schema: {} stats, {} achievements", schema.stats.len(), schema.achievements().count());
                            for stat in schema.stats.iter().take(8) {
                                println!("  stat {} {:?} {} (default {})", stat.id, stat.kind, stat.name, stat.default);
                            }
                            for (stat, ach) in schema.achievements().take(5) {
                                println!("  achievement {} bit {} in stat {stat}: {:?}", ach.name, ach.bit, ach.display_name);
                            }
                        }
                        None => println!("schema did not parse"),
                    }
                }
                other => println!("GetStats: {}", describe(&other)),
            }
            println!("CurrentPlayers: {}", describe(&stats.handle(&connection, StatsOp::CurrentPlayers).await));
            println!("Percentages: {}", describe(&stats.handle(&connection, StatsOp::GlobalAchievementPercentages).await));
            for name in boards {
                let found = stats
                    .handle(&connection, StatsOp::FindLeaderboard { name: name.clone(), create: false, sort: 0, display: 0 })
                    .await;
                println!("FindLeaderboard {name}: {}", describe(&found));
                if let Response::Board { found: true, id, .. } = found {
                    let entries = stats
                        .handle(&connection, StatsOp::LeaderboardEntries { board: id, request: 0, start: 1, end: 5, users: Vec::new() })
                        .await;
                    println!("  top entries: {}", describe(&entries));
                }
            }
            Ok(())
        })
        .await
}

fn describe(response: &Response) -> String {
    match response {
        Response::Board {
            found,
            id,
            name,
            entries,
            sort,
            display,
        } => format!(
            "found={found} id={id} name={name:?} entries={entries} sort={sort} display={display}"
        ),
        Response::BoardEntries {
            result,
            total,
            entries,
        } => format!(
            "eresult {result}, total {total}, {} returned, first {:?}",
            entries.len(),
            entries
                .first()
                .map(|e| (e.steam_id, e.rank, e.score, e.details.len()))
        ),
        Response::Players { result, count } => format!("eresult {result}, {count} players"),
        Response::Percentages { result, entries } => format!(
            "eresult {result}, {} achievements, top {:?}",
            entries.len(),
            entries.iter().take(3).collect::<Vec<_>>()
        ),
        Response::Error { message } => format!("error: {message}"),
        _ => "unexpected response".into(),
    }
}
