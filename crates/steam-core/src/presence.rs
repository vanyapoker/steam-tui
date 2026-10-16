//! Report directly launched games to Steam and monitor their process lifetime.
use std::collections::BTreeSet;
use std::time::Duration;
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver::{
    CMsgClientGamesPlayed, cmsg_client_games_played::GamePlayed,
};
use tokio::sync::mpsc;

pub enum PresenceEvent {
    Running(BTreeSet<u32>),
    Status(String),
}

fn games_played(apps: &BTreeSet<u32>) -> CMsgClientGamesPlayed {
    CMsgClientGamesPlayed {
        games_played: apps
            .iter()
            .map(|&id| GamePlayed {
                game_id: Some(u64::from(id)),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// Rescans on startup/reconnect, publishes changes, and clears presence on exit.
/// An empty list clears only this CM session's games. Offline mode still tracks
/// local games for display. Failed writes are retried on the next poll.
pub async fn run(connection: Option<Connection>, updates: mpsc::Sender<PresenceEvent>) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut observed = None;
    let mut published = None;
    let mut warned = false;
    loop {
        tokio::select! {
            _ = updates.closed() => return,
            _ = tick.tick() => {},
        }
        let account = connection.as_ref().map(|c| c.steam_id().steam64());
        let Ok((apps, report)) = tokio::task::spawn_blocking(move || {
            (
                crate::launch::running_app_ids(),
                account.map(crate::launch::presence_app_ids),
            )
        })
        .await
        else {
            continue;
        };
        if observed.as_ref() != Some(&apps) {
            if updates
                .send(PresenceEvent::Running(apps.clone()))
                .await
                .is_err()
            {
                return;
            }
            observed = Some(apps.clone());
        }
        if let (Some(connection), Some(apps)) = (&connection, report)
            && published.as_ref() != Some(&apps)
        {
            match tokio::time::timeout(Duration::from_secs(5), connection.send(games_played(&apps)))
                .await
            {
                Ok(Ok(())) => {
                    published = Some(apps);
                    warned = false;
                }
                _ if !warned => {
                    if updates
                        .send(PresenceEvent::Status(
                            "Steam game presence update failed; retrying…".into(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    warned = true;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_all_running_apps_and_clears_after_last_exit() {
        let active = games_played(&BTreeSet::from([3124540, 440]));
        assert_eq!(
            active
                .games_played
                .iter()
                .map(|game| game.game_id.unwrap())
                .collect::<Vec<_>>(),
            [440, 3124540]
        );
        let remaining = games_played(&BTreeSet::from([440]));
        assert_eq!(remaining.games_played.len(), 1);
        assert_eq!(remaining.games_played[0].game_id, Some(440));
        assert!(games_played(&BTreeSet::new()).games_played.is_empty());
    }
}
