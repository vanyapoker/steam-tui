//! Developer announcements from Steam's public ISteamNews API. No login needed.
use crate::installed::{Result, failure};
use serde::Deserialize;
use std::time::Duration;

const MAX_RESPONSE: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct NewsItem {
    pub title: String,
    pub contents: String,
    pub date: u64,
    pub url: String,
}

#[derive(Deserialize)]
struct Response {
    appnews: AppNews,
}

#[derive(Deserialize)]
struct AppNews {
    appid: u32,
    newsitems: Vec<NewsItem>,
}

fn parse(bytes: &[u8], app_id: u32) -> Result<Vec<NewsItem>> {
    let response: Response = serde_json::from_slice(bytes)?;
    if response.appnews.appid != app_id {
        return Err(failure("Steam returned news for a different game"));
    }
    Ok(response.appnews.newsitems)
}

pub async fn fetch(app_id: u32) -> Result<Vec<NewsItem>> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut response = http
        .get(format!("https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/?appid={app_id}&count=10&maxlength=0&feeds=steam_community_announcements"))
        .send()
        .await
        .map_err(|_| failure("could not fetch game updates"))?
        .error_for_status()
        .map_err(|e| failure(format!("game updates request failed: {}", e.status().map_or(0, |s| s.as_u16()))))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_RESPONSE {
            return Err(failure("game updates response is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    parse(&bytes, app_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_empty_feed_and_checks_game_identity() {
        let data = br#"{"appnews":{"appid":30,"newsitems":[]}}"#;
        assert!(parse(data, 30).unwrap().is_empty());
        assert!(parse(data, 40).is_err());
        assert!(parse(b"{}", 30).is_err());
    }
}
