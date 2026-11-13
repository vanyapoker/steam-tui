use super::terminal_text;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use steam_core::install::{Destination, InstallPhase, InstallProgress};
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

struct CliProgress {
    terminal: bool,
    width: usize,
    last: Option<(InstallPhase, u8)>,
    displayed: bool,
}

impl CliProgress {
    fn new() -> Self {
        let terminal = io::stdout().is_terminal();
        let width = if terminal {
            crossterm::terminal::size().map_or(80, |(columns, _)| usize::from(columns))
        } else {
            80
        };
        Self {
            terminal,
            width,
            last: None,
            displayed: false,
        }
    }

    fn show(&mut self, progress: &InstallProgress) -> io::Result<()> {
        let (done, total, label) = match progress.phase {
            InstallPhase::Preparing => (0, 0, "Preparing"),
            InstallPhase::Downloading => (
                progress.completed_bytes,
                progress.total_bytes,
                "Downloading",
            ),
            InstallPhase::Verifying => (
                progress.verify_bytes_done,
                progress.verify_bytes_total,
                "Verifying",
            ),
            InstallPhase::Committing => (
                progress
                    .message
                    .split_whitespace()
                    .last()
                    .and_then(|word| word.strip_suffix('%'))
                    .and_then(|number| number.parse::<u64>().ok())
                    .unwrap_or(0),
                100,
                "Committing",
            ),
            InstallPhase::Finished => (0, 0, "Finished"),
            InstallPhase::Cancelled => (0, 0, "Cancelled"),
        };
        let percent = if total > 0 {
            ((done.min(total) as u128 * 100) / total as u128) as u8
        } else {
            0
        };
        let bucket = if self.terminal { percent } else { percent / 10 };
        let key = (progress.phase, bucket);
        if self.last == Some(key) {
            return Ok(());
        }
        self.last = Some(key);
        let line = if total > 0 && self.width < 40 {
            format!("{label} {percent}%")
        } else if total > 0 {
            let bar_width = self.width.saturating_sub(43).clamp(8, 32);
            let filled = usize::from(percent) * bar_width / 100;
            let (done_size, total_size, unit) = if progress.phase == InstallPhase::Committing {
                (0, 0, "")
            } else if total < 1024 * 1024 {
                (done / 1024, total.div_ceil(1024), " KiB")
            } else {
                (done / (1024 * 1024), total.div_ceil(1024 * 1024), " MiB")
            };
            let size = if unit.is_empty() {
                String::new()
            } else {
                format!("  {done_size}/{total_size}{unit}")
            };
            format!(
                "{label:<11} [{}{}] {percent:>3}%{size}",
                "#".repeat(filled),
                "-".repeat(bar_width - filled),
            )
        } else {
            label.to_owned()
        };
        let mut out = io::stdout().lock();
        if self.terminal {
            write!(out, "\r\x1b[2K{line}")?;
        } else {
            writeln!(out, "{line}")?;
        }
        out.flush()?;
        self.displayed = true;
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.terminal && self.displayed {
            let mut out = io::stdout().lock();
            writeln!(out)?;
            out.flush()?;
        }
        Ok(())
    }
}

fn read_selection(input: &mut impl BufRead) -> Result<String> {
    let mut line = String::new();
    let read = io::Read::take(&mut *input, 4097).read_line(&mut line)?;
    if read == 0 {
        return Err("destination selection cancelled".into());
    }
    if line.len() > 4096 || !line.ends_with('\n') {
        return Err("destination input is too long or incomplete".into());
    }
    Ok(line.trim().to_owned())
}

fn choose_destination(
    input: &mut impl BufRead,
    output: &mut impl Write,
    destinations: &[Destination],
    required_bytes: u64,
) -> Result<Destination> {
    writeln!(output, "Choose an install destination:")?;
    for (index, destination) in destinations.iter().enumerate() {
        let free = destination.free_bytes.map_or_else(
            || "space unknown".to_owned(),
            |bytes| format!("{:.1} GiB free", bytes as f64 / 1024.0_f64.powi(3)),
        );
        let status = if !destination.available {
            destination.message.as_deref().unwrap_or("unavailable")
        } else if destination
            .free_bytes
            .is_some_and(|free| free < required_bytes)
        {
            "less free space than full game size"
        } else {
            "available"
        };
        writeln!(
            output,
            "  {}. {} ({free}; {})",
            index + 1,
            terminal_text(&destination.path.display().to_string()),
            terminal_text(status)
        )?;
    }
    writeln!(output, "  c. Enter a custom library path")?;
    writeln!(output, "  q. Cancel")?;
    loop {
        write!(output, "Destination number or c/q: ")?;
        output.flush()?;
        let choice = read_selection(input)?;
        if choice.eq_ignore_ascii_case("q") {
            return Err("destination selection cancelled".into());
        }
        let destination = if choice.eq_ignore_ascii_case("c") {
            write!(output, "Library path: ")?;
            output.flush()?;
            let path = read_selection(input)?;
            if path.is_empty() {
                writeln!(output, "Enter a nonempty path.")?;
                continue;
            }
            match steam_core::install::destination(&PathBuf::from(path)) {
                Ok(destination) => destination,
                Err(error) => {
                    writeln!(
                        output,
                        "Cannot use that path: {}",
                        terminal_text(&error.to_string())
                    )?;
                    continue;
                }
            }
        } else if let Ok(number) = choice.parse::<usize>() {
            if let Some(destination) = number
                .checked_sub(1)
                .and_then(|index| destinations.get(index))
            {
                destination.clone()
            } else {
                writeln!(output, "Choose a listed number or c/q.")?;
                continue;
            }
        } else {
            writeln!(output, "Choose a listed number or c/q.")?;
            continue;
        };
        if !destination.available {
            writeln!(
                output,
                "Destination unavailable: {}",
                terminal_text(destination.message.as_deref().unwrap_or("unknown reason"))
            )?;
            continue;
        }
        if destination
            .free_bytes
            .is_some_and(|free| free < required_bytes)
        {
            write!(
                output,
                "Free space is below the full game size. Let the installer check exact requirements? [y/N]: "
            )?;
            output.flush()?;
            if !matches!(
                read_selection(input)?.to_ascii_lowercase().as_str(),
                "y" | "yes"
            ) {
                continue;
            }
        }
        return Ok(destination);
    }
}

fn pick_destination(destinations: &[Destination], required_bytes: u64) -> Result<Destination> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| "interactive destination selection needs /dev/tty; use --library PATH")?;
    let mut input = BufReader::new(tty.try_clone()?);
    choose_destination(&mut input, &mut tty, destinations, required_bytes)
}

pub async fn uninstall(account: Option<&str>, app_id: u32, yes: bool) -> Result {
    let inventory = steam_core::installed::scan(None, account).map_err(|e| e.to_string())?;
    let game = inventory
        .games
        .into_iter()
        .find(|game| game.app_id == app_id)
        .ok_or("that app is not installed")?;
    if !yes {
        let mut tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|_| "uninstall needs an interactive terminal or --yes")?;
        write!(
            tty,
            "Uninstall {} ({app_id}) from {}? This deletes {}. Saves in the Proton prefix and workshop items are kept. Type the app id to confirm: ",
            terminal_text(&game.name),
            game.library_root.display(),
            game.install_dir.display()
        )?;
        tty.flush()?;
        let mut input = BufReader::new(tty);
        if read_selection(&mut input)? != app_id.to_string() {
            return Err("uninstall cancelled".into());
        }
    }
    let name = game.name.clone();
    tokio::task::spawn_blocking(move || steam_core::install::uninstall(&game))
        .await
        .map_err(|e| format!("Uninstall worker failed: {e}"))?
        .map_err(|e| e.to_string())?;
    if let Some(account) = account
        && let Ok(queue) = Queue::open(account)
    {
        steam_core::updates::cancel_app_jobs(&queue, app_id).map_err(|e| e.to_string())?;
    }
    println!("Uninstalled {} ({app_id})", terminal_text(&name));
    Ok(())
}

pub async fn install(account: &str, app_id: u32, library: Option<PathBuf>) -> Result {
    let connection = steam_core::account::login(account, false).await?;
    let inventory = steam_core::installed::scan(None, Some(account)).map_err(|e| e.to_string())?;
    let existing = inventory
        .games
        .into_iter()
        .find(|game| game.app_id == app_id);
    let plan = steam_core::catalog::plan(&connection, app_id, existing.as_ref()).await?;
    let steam_root = match &existing {
        Some(game) => game.steam_root.clone(),
        None => steam_core::installed::discover_root(None).map_err(|e| e.to_string())?,
    };
    let destination = if let Some(path) = library {
        steam_core::install::destination(&path).map_err(|e| e.to_string())?
    } else if let Some(game) = &existing {
        steam_core::install::destination(&game.library_root).map_err(|e| e.to_string())?
    } else {
        let destinations =
            steam_core::install::destinations(Some(&steam_root)).map_err(|e| e.to_string())?;
        pick_destination(&destinations, plan.installed_bytes)?
    };
    if let Some(game) = &existing
        && game.library_root != destination.path
    {
        return Err("installed games must be updated in their existing library".into());
    }
    if !destination.available {
        return Err(destination
            .message
            .unwrap_or_else(|| "library destination is unavailable".into())
            .into());
    }
    let queue = Queue::open(account).map_err(|e| e.to_string())?;
    let job_id = queue
        .enqueue(
            Intent {
                app_id,
                steam_root,
                destination: destination.path.clone(),
                dependency: None,
                shader: false,
                revision: None,
            },
            true,
        )
        .map_err(|e| e.to_string())?;
    println!(
        "Installing {app_id} to {} (job {job_id})",
        destination.path.display()
    );
    tokio::task::LocalSet::new()
        .run_until(async {
            let (progress, mut reports) = tokio::sync::mpsc::channel(64);
            let cancel = tokio_util::sync::CancellationToken::new();
            let run = steam_core::updates::run_selected(
                queue,
                Some(job_id),
                connection,
                progress,
                cancel.clone(),
                steam_core::install::PauseGate::default(),
                minute(),
                steam_core::launch::games_running(),
            );
            tokio::pin!(run);
            let mut display = CliProgress::new();
            let result = loop {
                tokio::select! {
                    result = &mut run => break result,
                    Some(report) = reports.recv() => { let _ = display.show(&report); },
                    _ = tokio::signal::ctrl_c(), if !cancel.is_cancelled() => cancel.cancel(),
                }
            };
            let _ = display.finish();
            if cancel.is_cancelled() {
                return Err(format!(
                    "installation interrupted; run jobs run to resume job {job_id}"
                )
                .into());
            }
            let result = result.map_err(|e| e.to_string())?;
            if result.is_none() {
                return Err("install job could not start; inspect it with jobs list".into());
            }
            println!("Installed {app_id}");
            Ok::<(), Box<dyn Error>>(())
        })
        .await
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
                    let run = steam_core::updates::run_next(queue.clone(), connection.clone(), sender, cancel.clone(), minute(), steam_core::launch::games_running());
                    tokio::pin!(run);
                    let mut display = CliProgress::new();
                    let outcome = loop {
                        tokio::select! {
                            outcome = &mut run => break outcome,
                            Some(progress) = receiver.recv() => { let _ = display.show(&progress); },
                            _ = tokio::signal::ctrl_c(), if !cancel.is_cancelled() => cancel.cancel(),
                        }
                    };
                    let _ = display.finish();
                    let outcome = outcome.map_err(|e| e.to_string())?;
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
    let usage = "content show APPID | branch APPID NAME | unlock APPID BRANCH | language APPID NAME | dlc APPID DLCID enable|disable | launch-option APPID [INDEX|default]";
    let command = args.first().map(String::as_str).ok_or(usage)?;
    if command == "launch-option" {
        return launch_option(account, args, usage);
    }
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

/// Local only: reads cached app metadata and stores the choice; needs no login.
fn launch_option(account: &str, args: &[String], usage: &str) -> Result {
    if !matches!(args.len(), 2 | 3) {
        return Err(usage.into());
    }
    let app_id = app(args.get(1))?;
    let game = steam_core::installed::scan(None, Some(account))
        .map_err(|e| e.to_string())?
        .games
        .into_iter()
        .find(|g| g.app_id == app_id)
        .ok_or("app is not installed")?;
    if let Some(value) = args.get(2) {
        let index = match value.as_str() {
            "default" => None,
            number => Some(number.parse::<u32>().map_err(|_| usage.to_string())?),
        };
        steam_core::launch::options::choose(&game, index).map_err(|e| e.to_string())?;
    }
    let options = steam_core::launch::options::available(&game).map_err(|e| e.to_string())?;
    let current = steam_core::launch::options::effective(&game).map_err(|e| e.to_string())?;
    if options.len() < 2 {
        println!("This game has no alternative launch options");
    }
    for option in options {
        println!(
            "{}\t{}\t{}",
            option.index,
            terminal_text(&option.description),
            if option.index == current.index {
                "selected"
            } else {
                ""
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod destination_tests {
    use super::*;
    use std::io::Cursor;

    fn destination(path: &str, free_bytes: u64, available: bool) -> Destination {
        Destination {
            path: PathBuf::from(path),
            free_bytes: Some(free_bytes),
            available,
            message: (!available).then(|| "unavailable".into()),
        }
    }

    #[test]
    fn picker_selects_a_listed_library_and_skips_unavailable_entries() {
        let destinations = [
            destination("/first", 100, false),
            destination("/second", 100, true),
        ];
        let mut input = Cursor::new(b"1\n2\n".as_slice());
        let mut output = Vec::new();
        let selected = choose_destination(&mut input, &mut output, &destinations, 50).unwrap();
        assert_eq!(selected.path, PathBuf::from("/second"));
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Destination unavailable")
        );
    }

    #[test]
    fn picker_requires_confirmation_when_free_space_is_low() {
        let destinations = [
            destination("/small", 10, true),
            destination("/large", 100, true),
        ];
        let mut input = Cursor::new(b"1\nn\n2\n".as_slice());
        let mut output = Vec::new();
        let selected = choose_destination(&mut input, &mut output, &destinations, 50).unwrap();
        assert_eq!(selected.path, PathBuf::from("/large"));
    }

    #[test]
    fn picker_cancellation_never_selects_a_destination() {
        let mut input = Cursor::new(b"q\n".as_slice());
        let mut output = Vec::new();
        assert!(choose_destination(&mut input, &mut output, &[], 50).is_err());
    }

    #[test]
    fn picker_accepts_custom_path() {
        let path = std::env::temp_dir().join("steam-tui-picker-custom-library");
        let mut input = Cursor::new(format!("c\n{}\n", path.display()));
        let mut output = Vec::new();
        let selected = choose_destination(&mut input, &mut output, &[], 0).unwrap();
        assert_eq!(selected.path, path);
    }
}
