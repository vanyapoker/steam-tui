//! A game's achievements for the signed-in account: the schema (names,
//! descriptions, hidden flags), which are unlocked and when, and how rare each is
//! among all players. Read-only; works for any game, running or not.
use std::collections::HashMap;
use std::time::Duration;

use steam_ipc::schema::Schema;
use steam_ipc::{Response, StatsOp, hex_decode};
use steam_vent::Connection;
use tokio::time::timeout;

use crate::backend::stats::Stats;

const DEADLINE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq)]
pub struct Achievement {
    /// API name, e.g. `ACH_WIN_ONE_GAME`.
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub hidden: bool,
    /// Image file names on Valve's CDN: unlocked and locked (gray) icon.
    pub icon: String,
    pub icon_gray: String,
    pub unlocked: bool,
    /// Unix seconds, when unlocked and known.
    pub unlock_time: Option<u32>,
    /// Percent of all players who unlocked it, when Steam reports it.
    pub percent: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Achievements {
    pub items: Vec<Achievement>,
    /// False when the global percentages could not be fetched.
    pub has_percentages: bool,
}

impl Achievement {
    /// The icon matching the unlock state; locked ones fall back to the colour
    /// icon when the schema has no gray one.
    pub fn icon_url(&self, app_id: u32) -> Option<String> {
        let file = if self.unlocked || self.icon_gray.is_empty() {
            &self.icon
        } else {
            &self.icon_gray
        };
        let hash = file.strip_suffix(".jpg").unwrap_or(file);
        let safe = !hash.is_empty()
            && hash.len() <= 64
            && app_id != 0
            && hash.bytes().all(|b| b.is_ascii_alphanumeric());
        safe.then(|| {
            format!(
                "https://cdn.fastly.steamstatic.com/steamcommunity/public/images/apps/{app_id}/{hash}.jpg"
            )
        })
    }
}

impl Achievements {
    pub fn unlocked(&self) -> usize {
        self.items.iter().filter(|a| a.unlocked).count()
    }
}

/// Joins the schema with the account's stat values and unlock times.
pub fn assemble(
    schema: &Schema,
    values: &[(u32, u32)],
    unlocks: &[(u32, Vec<u32>)],
    percentages: &[(String, f32)],
) -> Achievements {
    let values: HashMap<u32, u32> = values.iter().copied().collect();
    let unlocks: HashMap<u32, &Vec<u32>> = unlocks.iter().map(|(id, t)| (*id, t)).collect();
    let percent: HashMap<&str, f32> = percentages.iter().map(|(n, p)| (n.as_str(), *p)).collect();
    let items = schema
        .achievements()
        .map(|(stat, ach)| {
            let unlocked = values.get(&stat).copied().unwrap_or(0) >> ach.bit & 1 == 1;
            let time = unlocks
                .get(&stat)
                .and_then(|t| t.get(ach.bit as usize))
                .copied()
                .filter(|t| unlocked && *t != 0);
            Achievement {
                name: ach.name.clone(),
                display_name: if ach.display_name.is_empty() {
                    ach.name.clone()
                } else {
                    ach.display_name.clone()
                },
                description: ach.description.clone(),
                hidden: ach.hidden,
                icon: ach.icon.clone(),
                icon_gray: ach.icon_gray.clone(),
                unlocked,
                unlock_time: time,
                percent: percent.get(ach.name.as_str()).copied(),
            }
        })
        .collect();
    Achievements {
        items,
        has_percentages: !percentages.is_empty(),
    }
}

/// Fetches the account's achievements for `app_id`; `language` is a Steam
/// language name (`english`, `german`, ...) for the display text.
pub async fn fetch(
    connection: &Connection,
    app_id: u32,
    language: &str,
) -> Result<Achievements, String> {
    let mut stats = Stats::new(app_id, None);
    let loaded = timeout(
        DEADLINE,
        stats.handle(connection, StatsOp::GetStats { user: 0 }),
    )
    .await
    .map_err(|_| "Steam did not answer in time".to_owned())?;
    let (schema, values, unlocks) = match loaded {
        Response::StatsData {
            result: 1,
            schema,
            stats,
            unlocks,
            ..
        } => (schema, stats, unlocks),
        Response::StatsData { result, .. } => {
            return Err(format!("Steam refused the request (eresult {result})"));
        }
        Response::Error { message } => return Err(message),
        _ => return Err("Unexpected reply from Steam".into()),
    };
    let bytes = hex_decode(&schema).unwrap_or_default();
    if bytes.is_empty() {
        return Ok(Achievements::default());
    }
    let schema = Schema::parse(&bytes, language)
        .ok_or_else(|| "The achievement schema is unreadable".to_owned())?;
    let percentages = match timeout(
        DEADLINE,
        stats.handle(connection, StatsOp::GlobalAchievementPercentages),
    )
    .await
    {
        Ok(Response::Percentages {
            result: 1, entries, ..
        }) => entries,
        _ => Vec::new(),
    };
    Ok(assemble(&schema, &values, &unlocks, &percentages))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_urls_follow_unlock_state_and_reject_unsafe_names() {
        let mut item = Achievement {
            name: "A".into(),
            display_name: "A".into(),
            description: String::new(),
            hidden: false,
            icon: "aa11.jpg".into(),
            icon_gray: "bb22".into(),
            unlocked: true,
            unlock_time: None,
            percent: None,
        };
        let base = "https://cdn.fastly.steamstatic.com/steamcommunity/public/images/apps/7/";
        assert_eq!(item.icon_url(7), Some(format!("{base}aa11.jpg")));
        item.unlocked = false;
        assert_eq!(item.icon_url(7), Some(format!("{base}bb22.jpg")));
        item.icon_gray.clear();
        assert_eq!(item.icon_url(7), Some(format!("{base}aa11.jpg")));
        item.icon = "../x/y".into();
        assert_eq!(item.icon_url(7), None);
        item.icon.clear();
        assert_eq!(item.icon_url(7), None);
    }
    use steam_ipc::schema::{Achievement as Entry, Stat, StatKind};

    fn schema() -> Schema {
        let entry = |name: &str, bit: u32, hidden: bool| Entry {
            name: name.into(),
            bit,
            display_name: if name == "NONAME" {
                String::new()
            } else {
                name.to_lowercase()
            },
            description: "desc".into(),
            hidden,
            icon: String::new(),
            icon_gray: String::new(),
            progress: None,
        };
        Schema {
            stats: vec![Stat {
                id: 7,
                name: "ach".into(),
                kind: StatKind::Bits,
                default: 0,
                min: None,
                max: None,
                max_change: None,
                increment_only: false,
                achievements: vec![
                    entry("WIN", 0, false),
                    entry("SECRET", 1, true),
                    entry("NONAME", 2, false),
                ],
            }],
        }
    }

    #[test]
    fn joins_unlock_bits_times_and_rarity() {
        let result = assemble(
            &schema(),
            &[(7, 0b101)],
            &[(7, vec![1000, 0, 3000])],
            &[("WIN".into(), 12.5), ("SECRET".into(), 0.3)],
        );
        assert_eq!(result.items.len(), 3);
        assert_eq!(result.unlocked(), 2);
        assert!(result.has_percentages);
        let win = &result.items[0];
        assert!(win.unlocked && win.unlock_time == Some(1000) && win.percent == Some(12.5));
        let secret = &result.items[1];
        assert!(!secret.unlocked && secret.hidden && secret.unlock_time.is_none());
        assert_eq!(secret.percent, Some(0.3));
        // Missing display text falls back to the API name; no rarity data stays unknown.
        let plain = &result.items[2];
        assert_eq!(plain.display_name, "NONAME");
        assert_eq!(plain.percent, None);
        assert_eq!(plain.unlock_time, Some(3000));
    }

    #[test]
    fn an_unlocked_bit_without_a_time_has_no_date() {
        let result = assemble(&schema(), &[(7, 1)], &[], &[]);
        assert!(result.items[0].unlocked);
        assert_eq!(result.items[0].unlock_time, None);
        assert!(!result.has_percentages);
    }
}
