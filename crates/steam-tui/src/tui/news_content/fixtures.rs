//! Offline announcement corpus shared by parser and terminal rendering tests.
use steam_core::news::NewsItem;

pub(in crate::tui) fn feeds() -> impl Iterator<Item = (&'static str, u32, Vec<NewsItem>)> {
    [
        (
            "Mycopunk",
            3247750,
            include_str!("../../../tests/fixtures/news/mycopunk.json"),
        ),
        (
            "Deep Rock Galactic: Survivor",
            2321470,
            include_str!("../../../tests/fixtures/news/drg-survivor.json"),
        ),
        (
            "Deadlock",
            1422450,
            include_str!("../../../tests/fixtures/news/deadlock.json"),
        ),
        (
            "Backpack Battles",
            2427700,
            include_str!("../../../tests/fixtures/news/backpack-battles.json"),
        ),
        (
            "Far Far West",
            3124540,
            include_str!("../../../tests/fixtures/news/far-far-west.json"),
        ),
        (
            "Team Fortress 2",
            440,
            include_str!("../../../tests/fixtures/news/tf2.json"),
        ),
    ]
    .into_iter()
    .map(|(name, app_id, json)| {
        let response: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(response["appnews"]["appid"], app_id);
        let items = serde_json::from_value(response["appnews"]["newsitems"].clone()).unwrap();
        (name, app_id, items)
    })
}

pub(in crate::tui) fn assert_no_markup(text: &str, context: &str) {
    let text = text.to_ascii_lowercase();
    for name in [
        "p",
        "b",
        "i",
        "u",
        "h1",
        "h2",
        "h3",
        "list",
        "*",
        "img",
        "url",
        "hr",
        "dynamiclink",
        "previewyoutube",
        "carousel",
    ] {
        for marker in [
            format!("[{name}]"),
            format!("[/{name}]"),
            format!("[{name} "),
            format!("[{name}="),
        ] {
            assert!(!text.contains(&marker), "{context}: leaked {marker}");
        }
    }
    assert!(
        !text.contains("{steam_clan_image}"),
        "{context}: leaked image placeholder"
    );
    assert!(
        !text.contains("{steam_clan_loc_image}"),
        "{context}: leaked localized image placeholder"
    );
}
