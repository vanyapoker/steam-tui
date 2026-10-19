use super::*;
use crate::backend::{Direction, Event, EventSender};
use crate::launch::LaunchPlan;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
fn active() -> &'static Mutex<HashMap<u32, CancellationToken>> {
    static ACTIVE: OnceLock<Mutex<HashMap<u32, CancellationToken>>> = OnceLock::new();
    ACTIVE.get_or_init(Mutex::default)
}
/// Skip only the selected app's optional processing, then continue launch.
pub fn skip(app: u32) -> bool {
    if let Ok(map) = active().lock()
        && let Some(token) = map.get(&app)
    {
        token.cancel();
        true
    } else {
        false
    }
}
struct Registration(u32);
impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut map) = active().lock() {
            map.remove(&self.0);
        }
    }
}
fn emit(events: Option<&EventSender>, app: u32, title: &str) {
    if let Some(events) = events {
        let _ = events.send(Event {
            at: SystemTime::now(),
            app_id: app,
            direction: Direction::Local,
            channel: "Shaders",
            title: title.into(),
            detail: "Optional pre-launch Vulkan processing; S skips and continues launch".into(),
        });
    }
}
fn helper() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("STEAM_TUI_FOSSILIZE_REPLAY") {
        candidates.push(PathBuf::from(path));
    } else if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        candidates.push(parent.join("fossilize-replay"));
        candidates.push(parent.join("../lib/steam-tui/fossilize-replay"));
    }
    candidates
        .into_iter()
        .find_map(|p| p.canonicalize().ok().filter(|p| p.is_file()))
        .ok_or_else(|| {
            failure(
                "Standalone fossilize-replay missing; run install.sh to install shader processing",
            )
        })
}
fn fingerprint_file(hash: &mut Sha1, path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    hash.update(path.as_os_str().as_bytes());
    let file = fs::File::open(path)?;
    // Executables/scripts are bounded independently of downloaded game content.
    if file.metadata()?.len() > 256 * 1024 * 1024 {
        return Err(failure("Shader helper/tool exceeds identity size bound"));
    }
    let mut reader = std::io::BufReader::new(file);
    let mut buffer = [0; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(())
}
fn context(game: &InstalledGame, plan: &LaunchPlan, helper: &Path) -> Result<String> {
    context_with_gpu(game, plan, helper, &device::identity()?)
}
pub(super) fn context_with_gpu(
    game: &InstalledGame,
    plan: &LaunchPlan,
    helper: &Path,
    gpu: &[u8],
) -> Result<String> {
    let mut hash = Sha1::new();
    hash.update(b"steam-tui-fossilize-context-v1");
    hash.update(gpu);
    hash.update(game.app_id.to_le_bytes());
    hash.update(game.build_id.unwrap_or_default().to_le_bytes());
    fingerprint_file(&mut hash, helper)?;
    fingerprint_file(&mut hash, &plan.executable)?;
    // The selected Proton/runtime and graphics overrides affect processing even
    // with the same physical GPU. Hash values only; never persist environment.
    for (key, value) in &plan.environment {
        if key.starts_with("STEAM_COMPAT_")
            && !matches!(
                key.as_str(),
                "STEAM_COMPAT_DATA_PATH" | "STEAM_COMPAT_CLIENT_INSTALL_PATH"
            )
            || key.starts_with("VK_")
            || key.starts_with("MESA_")
            || key == "DRI_PRIME"
            || key.starts_with("__NV_")
            || key == "RADV_DEBUG"
        {
            hash.update(key.as_bytes());
            hash.update([0]);
            hash.update(value.as_bytes());
            hash.update([0]);
            for path in value
                .split(':')
                .map(Path::new)
                .filter(|p| p.is_absolute() && p.is_dir())
            {
                // Compatibility tools publish a version file. Its contents
                // distinguish in-place updates at a stable tool directory.
                let version = path.join("version");
                if version.is_file() {
                    fingerprint_file(&mut hash, &version)?;
                }
            }
        }
    }
    for key in [
        "VK_ICD_FILENAMES",
        "VK_DRIVER_FILES",
        "MESA_LOADER_DRIVER_OVERRIDE",
        "DRI_PRIME",
        "__NV_PRIME_RENDER_OFFLOAD",
        "RADV_DEBUG",
    ] {
        if let Some(value) = std::env::var_os(key) {
            use std::os::unix::ffi::OsStrExt;
            hash.update(key);
            hash.update(value.as_os_str().as_bytes());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn environment(plan: &mut LaunchPlan, directory: &Path) -> Result<()> {
    let mesa = directory.join("mesa");
    let nvidia = directory.join("nvidia");
    safe::private_directory(&mesa)?;
    safe::private_directory(&nvidia)?;
    for (key, path) in [
        ("MESA_SHADER_CACHE_DIR", &mesa),
        ("__GL_SHADER_DISK_CACHE_PATH", &nvidia),
    ] {
        plan.environment
            .insert(key.into(), path.to_string_lossy().into_owned());
    }
    plan.environment
        .insert("__GL_SHADER_DISK_CACHE".into(), "1".into());
    let path = directory.to_string_lossy();
    plan.environment
        .entry("PRESSURE_VESSEL_FILESYSTEMS_RW".into())
        .and_modify(|v| {
            v.push(':');
            v.push_str(&path);
        })
        .or_insert_with(|| path.into_owned());
    Ok(())
}
/// Optional launch stage. Failures are visible but never make shaders a game
/// prerequisite. GPU/driver/tool/build/helper changes require a new replay.
/// Cancellation skips optional work; no game process has been started yet.
pub async fn prepare(
    queue: &Queue,
    game: &InstalledGame,
    plan: &mut LaunchPlan,
    events: Option<&EventSender>,
) -> Result<()> {
    if !queue.settings()?.shaders_enabled {
        return Ok(());
    }
    let cancel = CancellationToken::new();
    {
        let mut map = active()
            .lock()
            .map_err(|_| failure("Shader processing controls unavailable"))?;
        if map.contains_key(&game.app_id) {
            return Err(failure("Shader processing is already active for this app"));
        }
        map.insert(game.app_id, cancel.clone());
    }
    let _registration = Registration(game.app_id);
    let result = process(queue, game, plan, events, &cancel).await;
    if let Err(error) = &result {
        emit(
            events,
            game.app_id,
            &format!("Shader processing skipped: {error}"),
        );
    }
    result
}
async fn process(
    queue: &Queue,
    game: &InstalledGame,
    plan: &mut LaunchPlan,
    events: Option<&EventSender>,
    cancel: &CancellationToken,
) -> Result<()> {
    let root = root(queue, game.app_id)?;
    // Never wait on a running download or another launcher while holding game
    // launch resources. The next launch can use its committed generation.
    let Some(_lock) = safe::Lock::try_acquire(&root.join("cache.lock"))? else {
        emit(
            events,
            game.app_id,
            "Shader download is active; continuing launch",
        );
        return Ok(());
    };
    let mut state = load(&root)?;
    if state.manifest == 0 || state.sha1.is_none() {
        return Ok(());
    }
    let archive = safe::checked(&root, Path::new(&format!("{}.foz", state.manifest)), true)?;
    let helper = helper()?;
    let g = game.clone();
    let p = plan.clone();
    let h = helper.clone();
    let context = tokio::task::spawn_blocking(move || context(&g, &p, &h))
        .await
        .map_err(|_| failure("Shader identity worker failed"))??;
    if cancel.is_cancelled() {
        return Err(failure("Skipped by user"));
    }
    let directory = root.join(format!("replay-{context}"));
    let already_present = directory.is_dir();
    safe::private_directory(&directory)?;
    environment(plan, &directory)?;
    let size = state.size;
    let a = archive.clone();
    if tokio::task::spawn_blocking(move || hash(&a, size))
        .await
        .map_err(|_| failure("Shader verification worker failed"))??
        != state.sha1
    {
        return Err(failure(
            "Downloaded shader archive changed; queue a fresh download",
        ));
    }
    if already_present && state.processed.as_deref() == Some(&context) {
        return Ok(());
    }
    emit(events, game.app_id, "Processing Vulkan shaders · S to skip");
    run_helper(
        &helper,
        &archive,
        &directory,
        &plan.environment,
        queue,
        game.app_id,
        cancel,
        events,
    )
    .await?;
    state.processed = Some(context);
    save(&root, &state)?;
    prune_locked(&root, &state, true)?;
    emit(events, game.app_id, "Vulkan shader processing complete");
    Ok(())
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_helper(
    helper: &Path,
    archive: &Path,
    directory: &Path,
    environment: &std::collections::BTreeMap<String, String>,
    queue: &Queue,
    app: u32,
    cancel: &CancellationToken,
    events: Option<&EventSender>,
) -> Result<()> {
    use std::process::Stdio;
    let mut command = tokio::process::Command::new(helper);
    command
        .args([
            "--num-threads",
            "2",
            "--shader-cache-size",
            "256",
            "--timeout-seconds",
            "60",
            "--on-disk-pipeline-cache",
        ])
        .arg(directory.join("pipeline-cache"))
        .arg("--replayer-cache")
        .arg(directory.join("replay-cache"))
        .arg(archive)
        .envs(environment)
        .env_remove("ENABLE_VK_LAYER_VALVE_steam_fossilize_1")
        .env_remove("FOSSILIZE_DUMP_PATH")
        .env_remove("FOSSILIZE_DUMP_SYNC")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    // A robust replayer can fork subprocesses. Put it in its own group and kill
    // the entire group on skip/error; reap the supervisor before returning.
    unsafe {
        command.pre_exec(|| {
            libc::nice(10);
            let limit = libc::rlimit {
                rlim_cur: 2 * 1024 * 1024 * 1024,
                rlim_max: 2 * 1024 * 1024 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|_| failure("Cannot start standalone shader helper"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| failure("Shader helper stderr unavailable"))?;
    struct Diagnostics(tokio::task::JoinHandle<Vec<u8>>);
    impl Drop for Diagnostics {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut diagnostics = Diagnostics(tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut stderr = stderr;
        let mut tail = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buffer[..n]);
                    if tail.len() > 8192 {
                        tail.drain(..tail.len() - 8192);
                    }
                }
            }
        }
        tail
    }));
    let pgid = child
        .id()
        .ok_or_else(|| failure("Shader helper has no PID"))? as i32;
    struct Group(i32);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
    let _group = Group(pgid);
    let start = std::time::Instant::now();
    let mut paused = false;
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => { unsafe { libc::kill(-pgid, libc::SIGKILL); } let _ = child.wait().await; return Err(failure("Skipped by user")); },
            status = child.wait() => {
                let status = status?;
                if status.success() { return Ok(()); }
                unsafe { libc::kill(-pgid, libc::SIGKILL); }
                let tail = tokio::time::timeout(Duration::from_secs(1), &mut diagnostics.0).await.ok().and_then(Result::ok).unwrap_or_default();
                let text: String = String::from_utf8_lossy(&tail).chars().filter(|c| !c.is_control()).collect();
                let detail: String = text.chars().rev().take(512).collect::<Vec<_>>().into_iter().rev().collect();
                return Err(failure(format!("Shader helper exited with {status}: {detail}"))); },
            _ = poll.tick() => {
                if start.elapsed() > Duration::from_secs(30 * 60) { unsafe { libc::kill(-pgid, libc::SIGKILL); } let _ = child.wait().await; return Err(failure("Shader processing reached its 30-minute limit")); }
                let suspend = queue.settings().map(|s| s.pause_during_games && !crate::launch::running_app_ids().is_empty()).unwrap_or(true);
                if suspend != paused { unsafe { libc::kill(-pgid, if suspend { libc::SIGSTOP } else { libc::SIGCONT }); } paused = suspend; }
                emit(events, app, &format!("{} Vulkan shaders · {}s · S to skip", if paused { "Paused processing" } else { "Processing" }, start.elapsed().as_secs()));
            }
        }
    }
}
