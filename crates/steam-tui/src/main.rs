#[cfg(not(target_os = "linux"))]
compile_error!("steam-tui supports Linux only");

mod content_jobs;
mod preferences;
mod tui;

use std::borrow::Cow;
use std::env;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "Usage: steam-tui [--account NAME] [command]\n\nWith no command, open the TUI using the latest successful account.\nCommands:\n  tui [--offline]          Friends and installed games (default)\n  notifications            Show all recent account notifications\n  discover                 Query Steam's server directory (no account)\n  inspect-app <appid>       Fetch public app metadata (anonymous session)\n  login [--ephemeral]       Sign in; reuse/store a Secret Service refresh token\n  library [FILTER] [--ephemeral]  List licensed games as APPID: name; filter names case insensitively\n  install <appid> [--library PATH]  Install or update a game without the TUI\n  uninstall <appid> [--yes]  Remove an installed game (no login; keeps saves and workshop items)\n  backend <appid> [--ephemeral]  Experimental client backend (explicit --account required)\n  launch <appid> [--valve-host] [--installed]  Launch built-in, or explicitly use Valve host (explicit --account required)\n  content show|branch|language|dlc|launch-option|unlock  Inspect or select game content\n  jobs list|add|run|pause|resume|cancel  Persistent update jobs\n  updates show|policy|priority|window|during-games  Update settings\n  shaders show|add|process|prune|clear|budget|enabled  Vulkan pre-caching\n  logout                   Delete this account's local refresh token\n\n--login is an alias for the login command.\n--account NAME selects an account for account commands; login or TUI resume remembers it.\nCredentials stay in Secret Service. --ephemeral bypasses token persistence.\nlogout does not revoke tokens on Steam. Enter/double-click automatically selects Valve host for VAC games and built-in for others; Ctrl+Enter forces Valve host.";

#[derive(Debug, PartialEq)]
enum Command {
    Help,
    Jobs(Vec<String>),
    Updates(Vec<String>),
    Content(Vec<String>),
    Shaders(Vec<String>),
    Tui {
        offline: bool,
    },
    Discover,
    Notifications,
    InspectApp(u32),
    Backend {
        app_id: u32,
        ephemeral: bool,
    },
    Launch {
        app_id: u32,
        valve_host: bool,
        installed: bool,
    },
    Install {
        app_id: u32,
        library: Option<PathBuf>,
    },
    Uninstall {
        app_id: u32,
        yes: bool,
    },
    Login {
        ephemeral: bool,
    },
    Library {
        ephemeral: bool,
        filter: Option<String>,
    },
    Logout,
}

#[derive(Debug, PartialEq)]
struct Options {
    command: Command,
    account: Option<String>,
}

fn parse(args: &[String]) -> Result<Options, &'static str> {
    let mut command = None;
    let mut account = None;
    let mut offline = false;
    let mut ephemeral = false;
    let mut valve_host = false;
    let mut installed = false;
    let mut yes = false;
    let mut library_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--account" if account.is_none() => {
                index += 1;
                let name = args
                    .get(index)
                    .ok_or("--account requires a Steam login name")?;
                if !preferences::valid_account(name) {
                    return Err("--account requires a nonempty ASCII Steam login name");
                }
                account = Some(name.clone());
            }
            "--offline" if !offline => offline = true,
            "--ephemeral" if !ephemeral => ephemeral = true,
            "--valve-host" if !valve_host => valve_host = true,
            "--installed" if !installed => installed = true,
            "--yes" if !yes => yes = true,
            "--library" if library_path.is_none() => {
                index += 1;
                let path = args.get(index).ok_or("--library requires a path")?;
                if path.is_empty() {
                    return Err("--library requires a nonempty path");
                }
                library_path = Some(PathBuf::from(path));
            }
            "--help" | "-h" if command.is_none() => command = Some(Command::Help),
            "tui" if command.is_none() => command = Some(Command::Tui { offline: false }),
            "login" | "--login" if command.is_none() => {
                command = Some(Command::Login { ephemeral: false });
            }
            "library" if command.is_none() => {
                command = Some(Command::Library {
                    ephemeral: false,
                    filter: None,
                })
            }
            "shaders" if command.is_none() => command = Some(Command::Shaders(Vec::new())),
            "content" if command.is_none() => command = Some(Command::Content(Vec::new())),
            "jobs" if command.is_none() => command = Some(Command::Jobs(Vec::new())),
            "updates" if command.is_none() => command = Some(Command::Updates(Vec::new())),
            "logout" if command.is_none() => command = Some(Command::Logout),
            "discover" if command.is_none() => command = Some(Command::Discover),
            "notifications" if command.is_none() => command = Some(Command::Notifications),
            "inspect-app" | "backend" | "launch" | "install" | "uninstall"
                if command.is_none() =>
            {
                let backend = args[index] == "backend";
                let launch = args[index] == "launch";
                let install = args[index] == "install";
                let uninstall = args[index] == "uninstall";
                index += 1;
                let id = args
                    .get(index)
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|id| *id != 0)
                    .ok_or("appid must be a positive 32-bit integer")?;
                command = Some(if uninstall {
                    Command::Uninstall {
                        app_id: id,
                        yes: false,
                    }
                } else if install {
                    Command::Install {
                        app_id: id,
                        library: None,
                    }
                } else if launch {
                    Command::Launch {
                        app_id: id,
                        valve_host: false,
                        installed: false,
                    }
                } else if backend {
                    Command::Backend {
                        app_id: id,
                        ephemeral: false,
                    }
                } else {
                    Command::InspectApp(id)
                });
            }
            value if matches!(command, Some(Command::Library { filter: None, .. })) => {
                if value.is_empty() {
                    return Err("library filter cannot be empty");
                }
                if let Some(Command::Library { filter, .. }) = command.as_mut() {
                    *filter = Some(value.to_owned());
                }
            }
            value
                if matches!(
                    command,
                    Some(
                        Command::Jobs(_)
                            | Command::Updates(_)
                            | Command::Content(_)
                            | Command::Shaders(_)
                    )
                ) =>
            {
                match command.as_mut().unwrap() {
                    Command::Jobs(args)
                    | Command::Updates(args)
                    | Command::Content(args)
                    | Command::Shaders(args) => args.push(value.to_owned()),
                    _ => unreachable!(),
                }
            }
            _ => {
                return Err(
                    "unknown or duplicate command/option; credentials are never accepted as arguments",
                );
            }
        }
        index += 1;
    }
    let command = command.unwrap_or(Command::Tui { offline: false });
    if offline && !matches!(command, Command::Tui { .. }) {
        return Err("--offline is only supported in TUI mode");
    }
    if installed && !matches!(command, Command::Launch { .. }) {
        return Err("--installed is only supported by launch");
    }
    if valve_host && !matches!(command, Command::Launch { .. }) {
        return Err("--valve-host is only supported by launch");
    }
    if yes && !matches!(command, Command::Uninstall { .. }) {
        return Err("--yes is only supported by uninstall");
    }
    if library_path.is_some() && !matches!(command, Command::Install { .. }) {
        return Err("--library is only supported by install");
    }
    if ephemeral
        && !matches!(
            command,
            Command::Login { .. } | Command::Library { .. } | Command::Backend { .. }
        )
    {
        return Err("--ephemeral is only supported by login, library, and backend");
    }
    if matches!(command, Command::Backend { .. } | Command::Launch { .. }) && account.is_none() {
        return Err("backend and launch require an explicit --account NAME");
    }
    if account.is_some()
        && matches!(
            command,
            Command::Help | Command::Discover | Command::InspectApp(_)
        )
    {
        return Err(
            "--account is only supported by tui, notifications, login, library, install, uninstall, backend, launch, and logout",
        );
    }
    let command = match command {
        Command::Tui { .. } => Command::Tui { offline },
        Command::Login { .. } => Command::Login { ephemeral },
        Command::Library { filter, .. } => Command::Library { ephemeral, filter },
        Command::Install { app_id, .. } => Command::Install {
            app_id,
            library: library_path,
        },
        Command::Backend { app_id, .. } => Command::Backend { app_id, ephemeral },
        Command::Launch { app_id, .. } => Command::Launch {
            app_id,
            valve_host,
            installed,
        },
        Command::Uninstall { app_id, .. } => Command::Uninstall { app_id, yes },
        other => other,
    };
    Ok(Options { command, account })
}

fn selected_account(
    explicit: Option<String>,
    prompt: bool,
) -> Result<Option<String>, Box<dyn Error>> {
    let account = match explicit {
        Some(account) => Some(account),
        None => preferences::latest()?,
    };
    if account.is_none() && prompt {
        Ok(Some(prompt_account()?))
    } else {
        Ok(account)
    }
}

// Steam metadata is untrusted terminal output: exclude control and formatting
// characters rather than allowing escape sequences or bidi overrides.
fn terminal_text(value: &str) -> Cow<'_, str> {
    let unsafe_character = |ch: char| {
        ch.is_control()
            || matches!(ch,
                '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' |
                '\u{2060}'..='\u{206f}' | '\u{feff}'
            )
    };
    if value.chars().any(unsafe_character) {
        Cow::Owned(value.chars().filter(|ch| !unsafe_character(*ch)).collect())
    } else {
        Cow::Borrowed(value)
    }
}

fn prompt_account() -> Result<String, Box<dyn Error>> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| "account commands require an interactive terminal (/dev/tty)")?;
    write!(tty, "Steam account name: ")?;
    tty.flush()?;
    let mut account = String::new();
    BufReader::new(tty).read_line(&mut account)?;
    let account = account.trim();
    if !preferences::valid_account(account) {
        return Err("enter a nonempty ASCII Steam account name without whitespace".into());
    }
    Ok(account.to_owned())
}

fn print_app(app: &steam_core::library::LibraryApp) {
    println!(
        "{}\t{}\t{}",
        app.app_id,
        terminal_text(app.name.as_deref().unwrap_or("(name unavailable)")),
        terminal_text(app.app_type.as_deref().unwrap_or("(type unavailable)"))
    );
}

fn library_matches(kind: Option<&str>, name: Option<&str>, filter: Option<&str>) -> bool {
    kind.is_none_or(|kind| kind.eq_ignore_ascii_case("game"))
        && name.is_some_and(|name| {
            filter.is_none_or(|needle| name.to_lowercase().contains(&needle.to_lowercase()))
        })
}

async fn run(options: Options) -> Result<(), Box<dyn Error>> {
    let Options { command, account } = options;
    match command {
        Command::Uninstall { app_id, yes } => {
            let account = selected_account(account, false)?;
            content_jobs::uninstall(account.as_deref(), app_id, yes).await?;
        }
        Command::Help => println!("{USAGE}"),
        Command::Notifications => {
            let account = selected_account(account, false)?
                .ok_or("notifications requires --account or a remembered account")?;
            let connection = steam_core::account::resume(&account).await?;
            let notes = steam_core::notifications::latest(&connection).await?;
            println!("{} recent notifications (newest first)", notes.len());
            for note in notes {
                let time = chrono::DateTime::from_timestamp(note.timestamp as i64, 0)
                    .map(|time| {
                        time.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_else(|| note.timestamp.to_string());
                println!(
                    "{time} [{}] {}{}",
                    if note.read { "read" } else { "unread" },
                    note.label,
                    if note.title != note.label {
                        format!(" · {}", note.title)
                    } else {
                        String::new()
                    }
                );
                if !note.body.is_empty() {
                    println!("  {}", note.body);
                }
            }
        }
        Command::Shaders(args) => {
            let account = selected_account(account, false)?
                .ok_or("shaders requires --account or a remembered account")?;
            content_jobs::shaders(&account, &args).await?;
        }
        Command::Content(args) => {
            let account = selected_account(account, false)?
                .ok_or("content requires --account or a remembered account")?;
            content_jobs::content(&account, &args).await?;
        }
        Command::Jobs(args) => {
            let account = selected_account(account, false)?
                .ok_or("jobs requires --account or a remembered account")?;
            content_jobs::run(&account, false, &args).await?;
        }
        Command::Updates(args) => {
            let account = selected_account(account, false)?
                .ok_or("updates requires --account or a remembered account")?;
            content_jobs::run(&account, true, &args).await?;
        }
        Command::Tui { offline } => {
            let account = selected_account(account, !offline)?;
            tui::run(account, offline).await?;
        }
        Command::Discover => {
            let endpoints = steam_core::discover_servers().await?;
            println!("Steam directory discovery succeeded (no account login).");
            println!("TCP candidate: {}", endpoints.tcp);
            println!(
                "WebSocket candidate: {}",
                terminal_text(&endpoints.websocket)
            );
        }
        Command::Install { app_id, library } => {
            let account = selected_account(account, true)?.ok_or("no account selected")?;
            content_jobs::install(&account, app_id, library).await?;
        }
        Command::Backend { app_id, ephemeral } => {
            let account = account.ok_or("backend requires an explicit account")?;
            let connection = steam_core::account::login(&account, ephemeral).await?;
            let endpoint = steam_core::backend::Endpoint::create()?;
            println!("{}", endpoint.path().display());
            std::io::stdout().flush()?;
            eprintln!(
                "Experimental backend for app {app_id}; Ctrl-C stops it. No game or Steam host is launched."
            );
            tokio::select! {
                result = endpoint.run(&connection, app_id) => result?,
                result = tokio::signal::ctrl_c() => result?,
            }
        }
        Command::Launch {
            app_id,
            valve_host,
            installed,
        } => {
            let backend = if valve_host {
                steam_core::launch::LaunchBackend::ValveHost
            } else {
                steam_core::launch::LaunchBackend::BuiltIn
            };
            steam_core::launch::validate_backend(app_id, backend)
                .map_err(|error| error.to_string())?;
            let account = account.ok_or("launch requires an explicit account")?;
            let connection = steam_core::account::login(&account, false).await?;
            let inventory =
                steam_core::installed::scan(None, Some(&account)).map_err(|e| e.to_string())?;
            let mut game = inventory
                .games
                .into_iter()
                .find(|game| game.app_id == app_id)
                .ok_or("that app is not installed")?;
            let local = tokio::task::LocalSet::new();
            local
                .run_until(async {
                    if !installed {
                        let selection = steam_core::updates::Queue::open(&account)?.settings()?.selections.get(&game.app_id).cloned().unwrap_or_default();
                        match steam_core::updates::check_selected(&connection, &game, &selection).await {
                            Ok(steam_core::catalog::UpdateStatus::UpToDate) => {},
                            Ok(steam_core::catalog::UpdateStatus::Available { .. }) => {
                                let queue = steam_core::updates::Queue::open(&account)?;
                                let id = queue.enqueue(steam_core::updates::Intent::installed(&game), true)?;
                                let (progress, mut reports) = tokio::sync::mpsc::channel(64);
                                let cancel = tokio_util::sync::CancellationToken::new();
                                let update = steam_core::updates::run_selected(queue, Some(id), connection.clone(), progress, cancel.clone(),
                                    steam_core::install::PauseGate::default(), content_jobs::minute(), steam_core::launch::games_running());
                                tokio::pin!(update);
                                let result = loop {
                                    tokio::select! {
                                        result = &mut update => break result?,
                                        Some(report) = reports.recv() => println!("{}", terminal_text(&report.message)),
                                        _ = tokio::signal::ctrl_c(), if !cancel.is_cancelled() => cancel.cancel(),
                                    }
                                };
                                if cancel.is_cancelled() { return Err(std::io::Error::other("Update cancelled; launch was not started").into()); }
                                game = result.ok_or_else(|| std::io::Error::other("Update job is paused or unavailable; inspect jobs before retrying"))?.1;
                            }
                            result => {
                                let reason = match result { Err(e) => e.to_string(), _ => "freshness is unknown".into() };
                                return Err(std::io::Error::other(format!("Update check failed: {reason}; use launch --installed to explicitly launch the installed version")).into());
                            }
                        }
                    }
                    let mut dependencies = std::collections::BTreeSet::new();
                    let result = loop {
                        let (events, mut reports) = tokio::sync::mpsc::unbounded_channel();
                        let launch = steam_core::launch::run_with_backend(&game, Some(&connection), Some(&account), backend, Some(events));
                        tokio::pin!(launch);
                        let result = loop { tokio::select! {
                            result = &mut launch => break result,
                            Some(event) = reports.recv() => if matches!(event.channel, "Setup" | "Shaders") { println!("{}", terminal_text(&event.title)); },
                            _ = tokio::signal::ctrl_c() => { if steam_core::shaders::skip(game.app_id) { println!("Skipping shader processing; continuing launch"); } },
                        } };
                        match result {
                            Ok(result) => break result,
                            Err(error) => {
                                let Some(missing) = steam_core::launch::Dependency::from_error(error.as_ref()) else {
                                    return Err(error);
                                };
                                if !dependencies.insert(missing.app_id()) || dependencies.len() > 64 {
                                    return Err(std::io::Error::other("Dependency installation made no progress or exceeded 64 dependencies").into());
                                }
                                println!("{}; installing…", terminal_text(&missing.to_string()));
                                let request = missing.install_request(&connection, &game, &account).await?;
                                let (progress, mut reports) = tokio::sync::mpsc::channel(64);
                                let cancel = tokio_util::sync::CancellationToken::new();
                                let queue = steam_core::updates::Queue::open(&account)?;
                                let intent = steam_core::updates::Intent { app_id: request.plan.app_id, steam_root: request.steam_root, destination: request.destination,
                                    shader: false, dependency: Some(steam_core::updates::DependencyIntent { parent_app_id: game.app_id, requirement: missing }), revision: None };
                                let id = queue.enqueue(intent, true)?;
                                let install = steam_core::updates::run_selected(queue, Some(id), connection.clone(), progress, cancel.clone(),
                                    steam_core::install::PauseGate::default(), content_jobs::minute(), steam_core::launch::games_running());
                                tokio::pin!(install);
                                loop {
                                    tokio::select! {
                                        result = &mut install => { result?; break; }
                                        Some(report) = reports.recv() => println!("{}", terminal_text(&report.message)),
                                        _ = tokio::signal::ctrl_c(), if !cancel.is_cancelled() => {
                                            cancel.cancel();
                                        }
                                    }
                                }
                                if cancel.is_cancelled() {
                                    return Err(std::io::Error::other("Dependency installation cancelled").into());
                                }
                            }
                        }
                    };
                    println!("{}", result.description);
                    // The per-launch backend lives until the game exits.
                    while steam_core::launch::is_running(app_id) {
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                            _ = tokio::signal::ctrl_c() => break,
                        }
                    }
                    Ok::<(), Box<dyn Error + Send + Sync>>(())
                })
                .await
                .map_err(|e| e.to_string())?;
        }
        Command::InspectApp(app_id) => {
            let start = Instant::now();
            match steam_core::inspect_public_app(app_id).await? {
                Some(app) => print_app(&app),
                None => {
                    return Err(
                        "app metadata is unknown or inaccessible to the anonymous session".into(),
                    );
                }
            }
            eprintln!(
                "Anonymous metadata query: {:.3}s",
                start.elapsed().as_secs_f64()
            );
        }
        Command::Login { ephemeral } | Command::Library { ephemeral, .. } => {
            let account = selected_account(account, true)?.ok_or("no account selected")?;
            let start = Instant::now();
            let connection = steam_core::account::login(&account, ephemeral).await?;
            preferences::remember(&account)?;
            eprintln!(
                "Authentication: {:.3}s (includes interactive confirmation)",
                start.elapsed().as_secs_f64()
            );
            if let Command::Library { filter, .. } = command {
                let start = Instant::now();
                let library = steam_core::library::load(&connection).await?;
                for app in library.apps.iter().filter(|app| {
                    library_matches(
                        app.app_type.as_deref(),
                        app.name.as_deref(),
                        filter.as_deref(),
                    )
                }) {
                    println!(
                        "{}: {}",
                        app.app_id,
                        terminal_text(app.name.as_deref().unwrap_or_default())
                    );
                }
                if !library.unresolved_packages.is_empty() {
                    eprintln!("Unresolved package IDs: {:?}", library.unresolved_packages);
                }
                if !library.unresolved_apps.is_empty() {
                    eprintln!("Unresolved app IDs: {:?}", library.unresolved_apps);
                }
                eprintln!("Library resolution: {:.3}s", start.elapsed().as_secs_f64());
            } else {
                println!("Signed in: {}", connection.steam_id().steam64());
            }
        }
        Command::Logout => {
            let account = selected_account(account, true)?.ok_or("no account selected")?;
            steam_core::account::logout(&account).await?;
            println!("Local refresh token removed. Steam-side tokens were not revoked.");
        }
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    if let Some(result) = steam_core::launch::internal_host_command(&args)
        .or_else(|| steam_core::launch::internal_desktop_command(&args))
    {
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{}", terminal_text(&error.to_string()));
                ExitCode::FAILURE
            }
        };
    }
    let command = match parse(&args) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", terminal_text(&error.to_string()));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_defaults_builtin_and_host_requires_explicit_flag() {
        for host in [false, true] {
            let mut args = vec!["launch", "440", "--account", "testacct"];
            if host {
                args.push("--valve-host");
            }
            assert_eq!(
                parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>())
                    .unwrap()
                    .command,
                Command::Launch {
                    app_id: 440,
                    valve_host: host,
                    installed: false,
                }
            );
        }
        for args in [
            vec!["tui", "--valve-host"],
            vec!["--valve-host"],
            vec!["launch", "440", "--valve-host"],
        ] {
            assert!(parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn rejects_credentials_and_invalid_app_ids() {
        for args in [
            vec!["login", "account", "password"],
            vec!["library", "--token", "secret"],
            vec!["inspect-app", "0"],
            vec!["inspect-app", "4294967296"],
        ] {
            assert!(parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
        assert_eq!(
            parse(&["inspect-app".into(), "440".into()]),
            Ok(Options {
                command: Command::InspectApp(440),
                account: None
            })
        );
        assert_eq!(
            parse(&["library".into(), "--ephemeral".into()]),
            Ok(Options {
                command: Command::Library {
                    ephemeral: true,
                    filter: None
                },
                account: None
            })
        );
    }

    #[test]
    fn library_filter_and_install_arguments() {
        assert_eq!(
            parse(&["library", "Rogue core", "--ephemeral"].map(str::to_owned)),
            Ok(Options {
                command: Command::Library {
                    ephemeral: true,
                    filter: Some("Rogue core".into())
                },
                account: None,
            })
        );
        assert_eq!(
            parse(
                &[
                    "install",
                    "440",
                    "--library",
                    "/games",
                    "--account",
                    "alice"
                ]
                .map(str::to_owned)
            ),
            Ok(Options {
                command: Command::Install {
                    app_id: 440,
                    library: Some(PathBuf::from("/games"))
                },
                account: Some("alice".into()),
            })
        );
        for args in [
            vec!["library", "rogue", "core"],
            vec!["install", "0"],
            vec!["install", "440", "--library"],
            vec!["library", "rogue", "--library", "/games"],
        ] {
            assert!(parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
        assert!(library_matches(
            Some("Game"),
            Some("Rogue Core"),
            Some("rOgUe")
        ));
        assert!(library_matches(
            Some("game"),
            Some("Rogue Core"),
            Some("ROGUE CORE")
        ));
        assert!(!library_matches(
            Some("Game"),
            Some("Rogue Core"),
            Some("other")
        ));
        assert!(!library_matches(Some("Tool"), Some("Rogue Core"), None));
    }

    #[test]
    fn login_accepts_command_and_flag_with_explicit_account() {
        for login in ["login", "--login"] {
            for args in [
                vec![login, "--account", "testacct"],
                vec!["--account", "testacct", login],
            ] {
                assert_eq!(
                    parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
                    Ok(Options {
                        command: Command::Login { ephemeral: false },
                        account: Some("testacct".into()),
                    })
                );
            }
        }
        assert_eq!(
            parse(&["--login", "--account", "testacct", "--ephemeral"].map(str::to_owned)),
            Ok(Options {
                command: Command::Login { ephemeral: true },
                account: Some("testacct".into()),
            })
        );
        for args in [
            ["login", "--login"],
            ["--login", "tui"],
            ["--login", "--offline"],
        ] {
            assert!(parse(&args.map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn experimental_backend_requires_app_and_explicit_account() {
        for args in [
            vec!["backend", "440", "--account", "test_account"],
            vec!["--account", "test_account", "backend", "440"],
        ] {
            assert_eq!(
                parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
                Ok(Options {
                    command: Command::Backend {
                        app_id: 440,
                        ephemeral: false
                    },
                    account: Some("test_account".into())
                })
            );
        }
        assert_eq!(
            parse(
                &["backend", "440", "--account", "test_account", "--ephemeral"].map(str::to_owned)
            ),
            Ok(Options {
                command: Command::Backend {
                    app_id: 440,
                    ephemeral: true
                },
                account: Some("test_account".into())
            })
        );
        for args in [
            vec!["backend"],
            vec!["backend", "0"],
            vec!["backend", "440"],
            vec!["backend", "440", "--account", "test_account", "--offline"],
        ] {
            assert!(parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn account_switching_accepts_global_option_order_and_rejects_conflicts() {
        for args in [
            vec!["--account", "second"],
            vec!["--account", "second", "tui"],
            vec!["tui", "--account", "second"],
        ] {
            assert_eq!(
                parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
                Ok(Options {
                    command: Command::Tui { offline: false },
                    account: Some("second".into())
                })
            );
        }
        for args in [
            vec!["--account"],
            vec!["--account", "first", "--account", "second"],
            vec!["login", "--offline"],
            vec!["--ephemeral"],
            vec!["discover", "--account", "first"],
            vec!["tui", "--account", "--offline"],
        ] {
            assert!(parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn uninstall_parses_yes_and_rejects_misuse() {
        let p = |args: &[&str]| parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            p(&["uninstall", "440"]).unwrap().command,
            Command::Uninstall {
                app_id: 440,
                yes: false
            }
        );
        assert_eq!(
            p(&["uninstall", "440", "--yes"]).unwrap().command,
            Command::Uninstall {
                app_id: 440,
                yes: true
            }
        );
        assert!(p(&["install", "440", "--yes"]).is_err());
        assert!(p(&["uninstall", "0"]).is_err());
    }

    #[test]
    fn removes_terminal_control_and_bidi_characters() {
        assert_eq!(
            terminal_text("name\x1b[2J\n\t\u{202e}evil\u{2066}"),
            "name[2Jevil"
        );
        assert_eq!(terminal_text("Dota 2 — 日本語"), "Dota 2 — 日本語");
    }
}
