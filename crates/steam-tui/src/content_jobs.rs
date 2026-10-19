use super::terminal_text;
use std::error::Error;
use steam_core::updates::{Intent, JobState, Queue, UpdatePolicy, Window};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;
fn id(s: Option<&String>) -> Result<u64> {
    Ok(s.ok_or("a job ID is required")?
        .parse::<u64>()?
        .checked_add(0)
        .filter(|id| *id != 0)
        .ok_or("job ID must be positive")?)
}
fn app(s: Option<&String>) -> Result<u32> {
    Ok(u32::try_from(id(s)?)?)
}
pub fn minute() -> u16 {
    steam_core::updates::local_minute()
}

fn clock(s: &str) -> Result<u16> {
    let (h, m) = s.split_once(':').ok_or("time must be HH:MM")?;
    let (h, m) = (h.parse::<u16>()?, m.parse::<u16>()?);
    if h >= 24 || m >= 60 {
        return Err("invalid hour/minute".into());
    }
    Ok(h * 60 + m)
}
pub async fn run(account: &str, settings_command: bool, args: &[String]) -> Result {
    let queue = Queue::open(account).map_err(|e| e.to_string())?;
    let command = args
        .first()
        .map(String::as_str)
        .unwrap_or(if settings_command { "show" } else { "list" });
    if settings_command {
        let mut settings = queue.settings().map_err(|e| e.to_string())?;
        match (command, args.len()) {
            ("show", 0 | 1) => {
                println!("Background downloads during games: {}", if settings.pause_during_games { "pause" } else { "allow" });
                match settings.window { None => println!("Download window: unrestricted"), Some(w) => println!("Download window: {:02}:{:02}–{:02}:{:02} local", w.start / 60, w.start % 60, w.end / 60, w.end % 60) }
                for (app, policy) in settings.policies { println!("{app}\t{policy:?}"); }
                for (app, priority) in settings.priorities { println!("{app}\tpriority {priority}"); }
                return Ok(());
            }
            ("policy", 3) => { settings.policies.insert(app(args.get(1))?, match args[2].as_str() { "automatic" => UpdatePolicy::Automatic, "launch-only" => UpdatePolicy::LaunchOnly, "manual" => UpdatePolicy::Manual, _ => return Err("policy must be automatic, launch-only, or manual".into()) }); }
            ("priority", 3) => { settings.priorities.insert(app(args.get(1))?, args[2].parse()?); }
            ("window", 2) if args[1] == "off" => settings.window = None,
            ("window", 3) => settings.window = Some(Window { start: clock(&args[1])?, end: clock(&args[2])? }),
            ("during-games", 2) => settings.pause_during_games = match args[1].as_str() { "pause" => true, "allow" => false, _ => return Err("use pause or allow".into()) },
            _ => return Err("updates show | policy APPID automatic|launch-only|manual | priority APPID NUMBER | window HH:MM HH:MM|off | during-games pause|allow".into()),
        }
        queue.save_settings(settings).map_err(|e| e.to_string())?;
        return Ok(());
    }
    match (command, args.len()) {
        ("list", 0 | 1) => {
            for job in queue.jobs().map_err(|e| e.to_string())? {
                println!(
                    "{}\t{}\t{:?}\t{}{}{}",
                    job.id,
                    job.intent.app_id,
                    job.state,
                    if job.intent.shader {
                        "shaders · "
                    } else {
                        "content · "
                    },
                    if job.foreground {
                        "foreground"
                    } else {
                        "background"
                    },
                    job.error
                        .as_ref()
                        .map(|e| format!("\t{}", terminal_text(e)))
                        .unwrap_or_default()
                );
            }
        }
        ("pause" | "resume" | "cancel", 2) => queue
            .control(
                id(args.get(1))?,
                match command {
                    "pause" => JobState::Paused,
                    "resume" => JobState::Pending,
                    _ => JobState::Cancelled,
                },
            )
            .map_err(|e| e.to_string())?,
        ("add", 2) => {
            let app_id = app(args.get(1))?;
            let game = steam_core::installed::scan(None, Some(account))
                .map_err(|e| e.to_string())?
                .games
                .into_iter()
                .find(|g| g.app_id == app_id)
                .ok_or("app is not installed; install it in the TUI first")?;
            println!(
                "Queued job {}",
                queue
                    .enqueue(Intent::installed(&game), true)
                    .map_err(|e| e.to_string())?
            );
        }
        ("run", 1) => {
            let connection = steam_core::account::login(account, false).await?;
            tokio::task::LocalSet::new().run_until(async {
                loop {
                    let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
                    let cancel = tokio_util::sync::CancellationToken::new();
                    let run = steam_core::updates::run_next(queue.clone(), connection.clone(), sender, cancel.clone(), minute(), !steam_core::launch::running_app_ids().is_empty());
                    tokio::pin!(run);
                    let outcome = loop {
                        tokio::select! {
                            outcome = &mut run => break outcome.map_err(|e| e.to_string())?,
                            Some(progress) = receiver.recv() => println!("[{}] {}", progress.app_id, terminal_text(&progress.message)),
                            _ = tokio::signal::ctrl_c(), if !cancel.is_cancelled() => cancel.cancel(),
                        }
                    };
                    match outcome { None => break, Some((id, _)) => println!("Job {id} complete") }
                    if cancel.is_cancelled() { break; }
                }
                Ok::<_, Box<dyn Error>>(())
            }).await?;
        }
        _ => return Err("jobs list | add APPID | run | pause ID | resume ID | cancel ID".into()),
    }
    Ok(())
}

pub async fn content(account: &str, args: &[String]) -> Result {
    let usage = "content show APPID | branch APPID NAME | unlock APPID BRANCH | language APPID NAME | dlc APPID DLCID enable|disable";
    let command = args.first().map(String::as_str).ok_or(usage)?;
    if !matches!(
        (command, args.len()),
        ("show", 2) | ("branch" | "language" | "unlock", 3) | ("dlc", 4)
    ) {
        return Err(usage.into());
    }
    let app_id = app(args.get(1))?;
    let game = steam_core::installed::scan(None, Some(account))
        .map_err(|e| e.to_string())?
        .games
        .into_iter()
        .find(|g| g.app_id == app_id)
        .ok_or("app is not installed")?;
    let connection = steam_core::account::login(account, false).await?;
    let queue = Queue::open(account).map_err(|e| e.to_string())?;
    if command == "unlock" {
        let password = steam_core::account::branch_password().await?;
        steam_core::branches::unlock(&connection, app_id, &args[2], password, true)
            .await
            .map_err(|e| e.to_string())?;
        println!("Branch unlocked; use content branch to select it");
        return Ok(());
    }
    let id = match command {
        "show" => {
            let data = steam_core::content_options::properties(&connection, &game)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "Installed branch: {}\nInstalled language: {}",
                terminal_text(&game.branch),
                terminal_text(&game.language)
            );
            if let Some(desired) = queue
                .settings()
                .map_err(|e| e.to_string())?
                .selections
                .get(&app_id)
            {
                println!(
                    "Desired branch: {}\nDesired language: {}",
                    desired
                        .branch
                        .as_deref()
                        .map(terminal_text)
                        .unwrap_or_else(|| "installed".into()),
                    desired
                        .language
                        .as_deref()
                        .map(terminal_text)
                        .unwrap_or_else(|| "installed".into())
                );
            }
            for b in data.branches {
                println!(
                    "branch\t{}{}",
                    terminal_text(&b.name),
                    if b.password_required {
                        "\tpassword required"
                    } else {
                        ""
                    }
                );
            }
            for language in data.languages {
                println!("language\t{}", terminal_text(&language));
            }
            for d in data.dlc {
                println!(
                    "dlc\t{}\t{}\t{}\t{}\t{}",
                    d.app_id,
                    terminal_text(&d.name),
                    if d.owned { "owned" } else { "unowned" },
                    if d.enabled { "enabled" } else { "disabled" },
                    if d.installed {
                        "installed"
                    } else {
                        "not installed"
                    }
                );
            }
            return Ok(());
        }
        "dlc" => {
            let dlc = app(args.get(2))?;
            let enabled = match args[3].as_str() {
                "enable" => true,
                "disable" => false,
                _ => return Err(usage.into()),
            };
            steam_core::content_options::select_dlc(&connection, &queue, &game, dlc, enabled).await
        }
        _ => {
            let mut selection = queue
                .settings()
                .map_err(|e| e.to_string())?
                .selections
                .get(&app_id)
                .cloned()
                .unwrap_or_default();
            if command == "branch" {
                selection.branch = Some(args[2].clone());
            } else {
                selection.language = Some(args[2].clone());
            }
            steam_core::content_options::select(&connection, &queue, &game, selection).await
        }
    }
    .map_err(|e| e.to_string())?;
    println!("Content change queued as job {id}; run jobs run or open the TUI");
    Ok(())
}

pub async fn shaders(account: &str, args: &[String]) -> Result {
    let usage = "shaders show APPID | add APPID | process APPID | prune APPID | clear APPID | budget MIB | enabled on|off";
    if args.len() != 2 {
        return Err(usage.into());
    }
    let queue = Queue::open(account).map_err(|e| e.to_string())?;
    if args[0] == "budget" {
        let mib = args[1].parse::<u64>()?;
        if !(256..=1024 * 1024).contains(&mib) {
            return Err("Shader archive budget must be 256..1048576 MiB".into());
        }
        let mut settings = queue.settings().map_err(|e| e.to_string())?;
        settings.shader_archive_budget = mib * 1024 * 1024;
        queue.save_settings(settings).map_err(|e| e.to_string())?;
        return Ok(());
    }
    if args[0] == "enabled" {
        let mut settings = queue.settings().map_err(|e| e.to_string())?;
        settings.shaders_enabled = match args[1].as_str() {
            "on" => true,
            "off" => false,
            _ => return Err(usage.into()),
        };
        queue.save_settings(settings).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let app_id = app(args.get(1))?;
    match args[0].as_str() {
        "show" => {
            let state = steam_core::shaders::state(&queue, app_id).map_err(|e| e.to_string())?;
            println!(
                "Bucket: {}\nManifest: {}\nArchive bytes: {}\nLast checked: {}\nProcessed context: {}",
                state.bucket,
                state.manifest,
                state.size,
                state.checked_at,
                state.processed.as_deref().unwrap_or("pending")
            );
        }
        "clear" => steam_core::shaders::clear(&queue, app_id).map_err(|e| e.to_string())?,
        "prune" => steam_core::shaders::prune(&queue, app_id).map_err(|e| e.to_string())?,
        "add" | "process" => {
            let game = steam_core::installed::scan(None, Some(account))
                .map_err(|e| e.to_string())?
                .games
                .into_iter()
                .find(|g| g.app_id == app_id)
                .ok_or("app is not installed")?;
            if args[0] == "add" {
                println!(
                    "Queued shader job {}",
                    queue
                        .enqueue(Intent::shader(&game), true)
                        .map_err(|e| e.to_string())?
                );
            } else {
                if !queue.settings().map_err(|e| e.to_string())?.shaders_enabled {
                    return Err("Shader processing is disabled; use shaders enabled on".into());
                }
                if steam_core::shaders::state(&queue, app_id)
                    .map_err(|e| e.to_string())?
                    .manifest
                    == 0
                {
                    return Err(
                        "No downloaded shader archive; use shaders add APPID, then jobs run".into(),
                    );
                }
                let mut plan = steam_core::launch::resolve(&game).map_err(|e| e.to_string())?;
                let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
                let process = steam_core::shaders::prepare(&queue, &game, &mut plan, Some(&sender));
                tokio::pin!(process);
                let result = loop {
                    tokio::select! {
                        result = &mut process => break result,
                        Some(event) = receiver.recv() => println!("{}", terminal_text(&event.title)),
                        _ = tokio::signal::ctrl_c() => { steam_core::shaders::skip(app_id); },
                    }
                };
                // prepare logs optional failures; drain a final completion or
                // skip event which arrived in the same scheduler turn.
                while let Ok(event) = receiver.try_recv() {
                    println!("{}", terminal_text(&event.title));
                }
                result.map_err(|e| e.to_string())?;
            }
        }
        _ => return Err(usage.into()),
    }
    Ok(())
}
