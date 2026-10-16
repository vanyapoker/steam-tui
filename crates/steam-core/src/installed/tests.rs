use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "steam-tui-inventory-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("steamapps/common")).unwrap();
        Self(path)
    }
    fn put(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn manifest(&self, id: u32, dir: &str, flags: u32) {
        fs::create_dir_all(self.0.join("steamapps/common").join(dir)).unwrap();
        self.put(&format!("steamapps/appmanifest_{id}.acf"), &format!(r#""AppState" {{ "appid" "{id}" "name" "Game {id}" "installdir" "{dir}" "StateFlags" "{flags}" "SizeOnDisk" "1234" }}"#));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn scalar(out: &mut Vec<u8>, key: &str, value: &str) {
    out.push(1);
    out.extend_from_slice(key.as_bytes());
    out.push(0);
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}
fn cache_apps(apps: &[(u32, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0x07564427u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    for (id, kind) in apps {
        let mut kv = b"\0appinfo\0".to_vec();
        scalar(&mut kv, "appid", &id.to_string());
        kv.extend_from_slice(b"\0common\0");
        scalar(&mut kv, "type", kind);
        kv.extend_from_slice(&[8, 8, 8]);
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&((40 + kv.len()) as u32).to_le_bytes());
        out.extend_from_slice(&[0; 40]);
        out.extend(kv);
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}
#[test]
fn reconciles_libraries_types_and_matching_account_only() {
    let main = Fixture::new();
    let second = Fixture::new();
    main.put("steamapps/libraryfolders.vdf", &format!(r#""libraryfolders" {{ "0" {{ "path" "{}" }} "1" {{ "path" "{}" }} "2" {{ "path" "/missing-steam-tui-library" }} }}"#, main.0.display(), second.0.display()));
    main.manifest(10, "native", 4);
    second.manifest(20, "other", 6);
    second.manifest(10, "duplicate", 4);
    main.manifest(30, "runtime", 4);
    main.manifest(40, "partial", 2);
    main.put("config/loginusers.vdf", r#""users" { "76561197960265729" { "AccountName" "alice" } "76561197960265730" { "AccountName" "bob" } }"#);
    main.put("userdata/1/config/localconfig.vdf", r#""UserLocalConfigStore" { "Software" { "Valve" { "Steam" { "apps" { "10" { "LastPlayed" "1700000000" "Playtime" "99" "LaunchOptions" "-safe" } "20" { "LastPlayed" "86400" } } } } } }"#);
    main.put("userdata/2/config/localconfig.vdf", r#""UserLocalConfigStore" { "Software" { "Valve" { "Steam" { "apps" { "10" { "LastPlayed" "1800000000" "Playtime" "999" } } } } } }"#);
    fs::create_dir_all(main.0.join("appcache")).unwrap();
    fs::write(
        main.0.join("appcache/appinfo.vdf"),
        cache_apps(&[(10, "game"), (20, "game"), (30, "tool")]),
    )
    .unwrap();
    let inventory = scan(Some(&main.0), Some("ALICE")).unwrap();
    assert_eq!(
        inventory.games.iter().map(|g| g.app_id).collect::<Vec<_>>(),
        [10, 20]
    );
    assert_eq!(inventory.games[0].playtime_minutes, Some(99));
    assert_eq!(inventory.games[0].last_played, Some(1700000000));
    assert_eq!(inventory.games[0].size_bytes, Some(1234));
    assert_eq!(inventory.games[0].launch_options.as_deref(), Some("-safe"));
    assert_eq!(
        inventory.games[0].library_root,
        main.0.canonicalize().unwrap()
    );
    assert_eq!(inventory.games[1].last_played, None);
    assert!(!inventory.warnings.is_empty());
    let unknown = scan(Some(&main.0), Some("nobody")).unwrap();
    assert!(unknown.games.iter().all(|g| g.playtime_minutes.is_none()
        && g.last_played.is_none()
        && g.launch_options.is_none()));
    let anonymous = scan(Some(&main.0), None).unwrap();
    assert!(anonymous.games.iter().all(|g| g.playtime_minutes.is_none()));
}
#[test]
fn rejects_escaping_install_and_missing_install() {
    let fixture = Fixture::new();
    fixture.manifest(1, "safe", 4);
    let bad = fixture.0.join("steamapps/appmanifest_1.acf");
    fs::write(
        &bad,
        r#""AppState" { "appid" "1" "name" "bad" "installdir" "../outside" "StateFlags" "4" }"#,
    )
    .unwrap();
    assert!(manifest_game(&bad, &fixture.0, &fixture.0, &BTreeMap::new()).is_err());
    fs::write(
        &bad,
        r#""AppState" { "appid" "1" "name" "bad" "installdir" "missing" "StateFlags" "4" }"#,
    )
    .unwrap();
    assert!(manifest_game(&bad, &fixture.0, &fixture.0, &BTreeMap::new()).is_err());
}
#[test]
fn parses_indexed_appinfo_and_rejects_truncation() {
    let strings = ["appinfo", "appid", "common", "type"];
    let mut kv = vec![0];
    kv.extend_from_slice(&0u32.to_le_bytes());
    kv.push(1);
    kv.extend_from_slice(&1u32.to_le_bytes());
    kv.extend_from_slice(b"12\0");
    kv.push(0);
    kv.extend_from_slice(&2u32.to_le_bytes());
    kv.push(1);
    kv.extend_from_slice(&3u32.to_le_bytes());
    kv.extend_from_slice(b"game\0");
    kv.extend_from_slice(&[8, 8, 8]);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0x07564429u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    let offset = 16 + 8 + 60 + kv.len() + 4;
    bytes.extend_from_slice(&(offset as u64).to_le_bytes());
    bytes.extend_from_slice(&12u32.to_le_bytes());
    bytes.extend_from_slice(&((60 + kv.len()) as u32).to_le_bytes());
    bytes.extend_from_slice(&[0; 60]);
    bytes.extend(kv);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    for s in strings {
        bytes.extend_from_slice(s.as_bytes());
        bytes.push(0);
    }
    let parsed = metadata::cache(&bytes, &BTreeSet::from([12])).unwrap();
    assert_eq!(
        parsed[&12].at(&["common"]).unwrap().text("type"),
        Some("game")
    );
    for end in [0, 4, 15, 30, bytes.len() - 1] {
        assert!(metadata::cache(&bytes[..end], &BTreeSet::from([12])).is_err());
    }
}
#[test]
fn text_vdf_rejects_duplicate_keys_and_unclosed_groups() {
    assert!(metadata::text(r#""root" { "key" "a" "KEY" "b" }"#).is_err());
    assert!(metadata::text(r#""root" { "key" "a" "#).is_err());
}

#[test]
fn keeps_disk_games_offline_but_hides_manifest_identified_tools_without_cache() {
    let fixture = Fixture::new();
    fixture.manifest(1, "game", 4);
    fixture.manifest(2, "proton", 4);
    fixture.manifest(3, "uninstalled", 5);
    fixture.put("steamapps/common/proton/toolmanifest.vdf", r#""manifest" { "version" "2" "compatmanager_layer_name" "proton" "commandline" "/proton %verb%" }"#);
    fixture.put(
        "config/libraryfolders.vdf",
        &format!(r#""libraryfolders" {{ "0" "{}" }}"#, fixture.0.display()),
    );
    let inventory = scan(Some(&fixture.0), None).unwrap();
    assert_eq!(
        inventory.games.iter().map(|g| g.app_id).collect::<Vec<_>>(),
        [1]
    );
    assert!(
        inventory
            .warnings
            .iter()
            .any(|w| w.contains("App type metadata unavailable"))
    );
}

#[test]
fn standard_keyvalues_pairs_are_independent_of_line_breaks() {
    let inline = metadata::text(r#""root" { "one" "1" "two" "quoted \"text\"" "nested" { "three" "3" "four" "C:\\games" } "five" "5" }"#).unwrap();
    let multiline = metadata::text(
        r#"
        // Comments and line breaks do not change token pairing.
        "root"
        {
            "one" "1"
            "two" "quoted \"text\""
            "nested"
            {
                "three" "3"
                "four" "C:\\games"
            }
            "five" "5"
        }
    "#,
    )
    .unwrap();
    assert_eq!(inline, multiline);
    let root = inline.child("root").unwrap();
    assert_eq!(root.text("two"), Some("quoted \"text\""));
    assert_eq!(
        root.at(&["nested"]).unwrap().text("four"),
        Some("C:\\games")
    );
    assert_eq!(root.text("five"), Some("5"));
    let bare = metadata::text("root { one 1 nested { two 2 } three 3 }").unwrap();
    assert_eq!(bare.at(&["root", "nested"]).unwrap().text("two"), Some("2"));
}

#[test]
fn text_vdf_rejects_dangling_tokens_directives_and_conditionals() {
    for input in [
        "\"root\" { \"key\" \"value\" \"dangling\" }",
        "\"root\" { \"key\" \"value\" \"next\"",
        "#include \"other.vdf\"",
        "\"root\" { \"key\" \"value\" [$LINUX] }",
        "\"root\" { \"key\" \"value\" [$LINUX] \"next\" \"value\" }",
    ] {
        assert!(metadata::text(input).is_err(), "{input}");
    }
}

#[test]
fn parses_large_escaped_localconfig_value_on_default_test_thread() {
    // Actual Steam localconfig contains large JSON strings. Regex lexers can
    // recurse once per escape; exercise hundreds of thousands without a
    // custom thread/stack size or an optimized-build requirement.
    let encoded_unit = r#"\"key\":\"value\\\\path\""#;
    let decoded_unit = "\"key\":\"value\\\\path\"";
    let repetitions = 100_000;
    let input = format!(
        "\"UserLocalConfigStore\" {{ \"json\" \"{}\" \"following\" \"intact\" }}",
        encoded_unit.repeat(repetitions)
    );
    let document = metadata::text(&input).unwrap();
    let root = document.child("UserLocalConfigStore").unwrap();
    assert_eq!(root.text("json").unwrap(), decoded_unit.repeat(repetitions));
    assert_eq!(root.text("following"), Some("intact"));
}

#[test]
fn parses_large_unescaped_string_on_default_test_thread() {
    let value = "long-plain-🦀-value".repeat(100_000);
    let input = format!("root {{ value \"{value}\" next done }}");
    let document = metadata::text(&input).unwrap();
    assert_eq!(
        document.child("root").unwrap().text("value"),
        Some(value.as_str())
    );
    assert_eq!(document.child("root").unwrap().text("next"), Some("done"));
}

#[test]
fn iterative_text_parser_handles_comments_escapes_and_bounds() {
    let document = metadata::text(
        "\u{feff} /* ignore { \"quoted\" } */ root { // ignore }\n key \"\\\"\\\\\\n\\r\\t\\q\" /* } */ next ok } // end"
    ).unwrap();
    let root = document.child("root").unwrap();
    assert_eq!(root.text("key"), Some("\"\\\n\r\t\\q"));
    assert_eq!(root.text("next"), Some("ok"));
    for input in [
        "root { key \"unfinished\\",
        "root { key \"unfinished",
        "/* unfinished",
        "root { key value } }",
        "{ key value }",
    ] {
        assert!(metadata::text(input).is_err(), "{input}");
    }
    let allowed = format!("{}{}", "group { ".repeat(64), "}".repeat(64));
    assert!(metadata::text(&allowed).is_ok());
    let excessive = format!("{}{}", "group { ".repeat(65), "}".repeat(65));
    assert!(
        metadata::text(&excessive)
            .unwrap_err()
            .to_string()
            .contains("nesting")
    );
}

#[test]
fn reads_installed_build_branch_language_platform_and_exact_manifest_ids() {
    let fixture = Fixture::new();
    fixture.manifest(10, "game", 6);
    fixture.put(
        "steamapps/appmanifest_10.acf",
        r#""AppState" {
        "appid" "10" "name" "Game" "installdir" "game" "StateFlags" "6"
        "buildid" "123"
        "UserConfig" { "BetaKey" "preview" "language" "german" "os_override" "windows" }
        "InstalledDepots" { "101" { "manifest" "18446744073709550000" } "102" { "manifest" "42" } }
    }"#,
    );
    let game = manifest_game(
        &fixture.0.join("steamapps/appmanifest_10.acf"),
        &fixture.0,
        &fixture.0,
        &BTreeMap::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(game.build_id, Some(123));
    assert_eq!(game.branch, "preview");
    assert_eq!(game.language, "german");
    assert_eq!(
        game.platform,
        Some(crate::catalog::InstallPlatform::Windows)
    );
    assert_eq!(
        game.installed_depots,
        BTreeMap::from([(101, 18_446_744_073_709_550_000), (102, 42)])
    );
}

#[test]
fn absent_installed_version_remains_unknown_not_zero() {
    let fixture = Fixture::new();
    fixture.manifest(10, "game", 4);
    let game = manifest_game(
        &fixture.0.join("steamapps/appmanifest_10.acf"),
        &fixture.0,
        &fixture.0,
        &BTreeMap::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(game.build_id, None);
    assert_eq!(game.branch, "public");
    assert_eq!(game.language, "english");
    assert_eq!(game.platform, None);
    assert!(game.installed_depots.is_empty());
}
