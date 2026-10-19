use super::*;
use script::{Process, Recipe};

struct Runner {
    command: Vec<String>,
    environment: BTreeMap<String, String>,
    directory: PathBuf,
    log: fs::File,
    log_path: PathBuf,
    token: String,
    sequence: usize,
    working_dir: PathBuf,
    cancel: CancellationToken,
}
impl Drop for Runner {
    fn drop(&mut self) {
        // Wine's SIGTERM handler flushes the prefix registry before exit; allow
        // slow storage to finish before falling back to SIGKILL.
        // https://github.com/wine-mirror/wine/blob/master/server/signal.c
        reaper::terminate(&self.token, Duration::from_secs(10));
    }
}

pub(super) fn execute(
    game: &InstalledGame,
    plan: &LaunchPlan,
    recipes: &[Recipe],
    directory: &Path,
    cancel: CancellationToken,
) -> Result<()> {
    let prefix = PathBuf::from(
        plan.environment
            .get("STEAM_COMPAT_DATA_PATH")
            .ok_or_else(|| failure("First-run setup needs a Proton prefix"))?,
    )
    .canonicalize()?;
    let _prefix_lock = safe::Lock::acquire(&prefix.join(".steam-tui-setup.lock"))?;
    let command = proton_command(plan)?;
    let token = reaper::new_token();
    let log_path = directory.join(format!("{}-{token}.log", game.app_id));
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log_path)?;
    let mut environment = plan.environment.clone();
    environment.insert(reaper::TOKEN_VAR.into(), token.clone());
    let mut runner = Runner {
        command,
        environment,
        directory: directory.to_owned(),
        log,
        log_path,
        token,
        sequence: 0,
        working_dir: game.install_dir.clone(),
        cancel,
    };
    let result = runner.recipes(&prefix, recipes);
    result.map_err(|error| {
        failure(format!(
            "First-run setup failed: {error}; log {}",
            runner.log_path.display()
        ))
    })
}

fn proton_command(plan: &LaunchPlan) -> Result<Vec<String>> {
    let paths: Vec<_> = std::env::split_paths(
        plan.environment
            .get("STEAM_COMPAT_TOOL_PATHS")
            .ok_or_else(|| failure("Proton setup lacks tool paths"))?,
    )
    .collect();
    if paths.is_empty() || paths.len() > 32 {
        return Err(failure("Invalid Proton setup tool chain"));
    }
    let mut command = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let document = read_vdf(&path.join("toolmanifest.vdf"))?;
        let manifest = document
            .child("manifest")
            .ok_or_else(|| failure("Missing setup tool manifest"))?;
        let expected = if index == 0 {
            "proton"
        } else {
            "container-runtime"
        };
        if manifest.text("compatmanager_layer_name") != Some(expected) {
            return Err(failure("Unexpected Proton setup tool layer"));
        }
        let mut wrapper = tool_command(path, manifest, "run")?;
        wrapper.extend(command);
        command = wrapper;
    }
    Ok(command)
}

impl Runner {
    fn invoke(&mut self, arguments: &[String]) -> Result<(i32, String)> {
        if self.cancel.is_cancelled() {
            return Err(failure("First-run setup cancelled"));
        }
        self.sequence += 1;
        let capture = self
            .directory
            .join(format!("capture-{}-{}.log", self.token, self.sequence));
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&capture)?;
        let mut command = Command::new(&self.command[0]);
        command
            .args(&self.command[1..])
            .args(arguments)
            .envs(&self.environment)
            .current_dir(&self.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(self.log.try_clone()?));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if self.cancel.is_cancelled() || std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&capture);
                return Err(failure(if self.cancel.is_cancelled() {
                    "First-run setup cancelled"
                } else {
                    "Prerequisite command exceeded its ten-minute deadline"
                }));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut bytes = Vec::new();
        fs::File::open(&capture)?
            .take(64 * 1024)
            .read_to_end(&mut bytes)?;
        self.log.write_all(&bytes)?;
        fs::remove_file(&capture)?;
        let text = if bytes.starts_with(&[0xff, 0xfe]) {
            String::from_utf16_lossy(
                &bytes[2..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect::<Vec<_>>(),
            )
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        Ok((status.code().unwrap_or(-1), text))
    }

    fn checked(&mut self, args: &[String]) -> Result<String> {
        let (code, output) = self.invoke(args)?;
        if code != 0 {
            return Err(failure(format!(
                "Prerequisite command exited with code {code}"
            )));
        }
        Ok(output)
    }

    fn reg_set(&mut self, key: &str, name: &str, value: &str, dword: bool) -> Result<()> {
        let mut args = vec![
            "C:\\windows\\system32\\reg.exe".into(),
            "add".into(),
            key.into(),
        ];
        if name.eq_ignore_ascii_case("(Default)") {
            args.push("/ve".into());
        } else {
            args.extend(["/v".into(), name.into()]);
        }
        args.extend([
            "/t".into(),
            if dword { "REG_DWORD" } else { "REG_SZ" }.into(),
            "/d".into(),
            value.into(),
            "/f".into(),
            "/reg:32".into(),
        ]);
        self.checked(&args)?;
        Ok(())
    }

    fn completed(&mut self, process: &Process, root: &Path) -> Result<bool> {
        let Some(key) = &process.has_run_key else {
            return Ok(false);
        };
        let key = script::expand(key, root)?;
        let (code, text) = self.invoke(&[
            "C:\\windows\\system32\\reg.exe".into(),
            "query".into(),
            key,
            "/v".into(),
            process.name.clone(),
            "/reg:32".into(),
        ])?;
        if code == 1 {
            return Ok(false);
        }
        if code != 0 {
            return Err(failure(format!("HasRunKey query exited with code {code}")));
        }
        let fields: Vec<_> = text.split_whitespace().collect();
        let value = fields
            .windows(2)
            .find(|pair| pair[0].eq_ignore_ascii_case("REG_DWORD"))
            .and_then(|pair| {
                pair[1]
                    .strip_prefix("0x")
                    .and_then(|n| u32::from_str_radix(n, 16).ok())
                    .or_else(|| pair[1].parse().ok())
            })
            .ok_or_else(|| failure("HasRunKey is not a readable DWORD"))?;
        Ok(value >= process.minimum)
    }

    /// Microsoft's 2015-2022 Visual C++ redistributables share one binary-compatible
    /// 14.x runtime. Proton prefixes already carry one, and the installer bundle is
    /// unreliable under Wine, so an installed 14.x runtime of the same architecture
    /// satisfies the Valve prerequisite.
    fn runtime_satisfied(&mut self, process: &Process, root: &Path) -> Result<bool> {
        let Some(key) = &process.has_run_key else {
            return Ok(false);
        };
        let key = script::expand(key, root)?.to_ascii_lowercase();
        let Some(year) = key
            .rsplit_once("\\commonredist\\vcredist\\")
            .map(|(_, y)| y)
        else {
            return Ok(false);
        };
        if !matches!(year, "2015" | "2017" | "2019" | "2022") {
            return Ok(false);
        }
        let mut fields = process.name.split_whitespace();
        let (arch, view) = match fields.next() {
            Some(a) if a.eq_ignore_ascii_case("x64") => ("x64", "/reg:64"),
            Some(a) if a.eq_ignore_ascii_case("x86") => ("x86", "/reg:32"),
            _ => return Ok(false),
        };
        if fields.next().and_then(|v| v.split('.').next()) != Some("14") {
            return Ok(false);
        }
        let (code, text) = self.invoke(&[
            "C:\\windows\\system32\\reg.exe".into(),
            "query".into(),
            format!("HKLM\\SOFTWARE\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes\\{arch}"),
            view.into(),
        ])?;
        if code == 1 {
            return Ok(false);
        }
        if code != 0 {
            return Err(failure(format!("Runtime query exited with code {code}")));
        }
        Ok(runtime_installed(&text))
    }

    fn recipes(&mut self, prefix: &Path, recipes: &[Recipe]) -> Result<()> {
        let win64 = match fs::File::open(prefix.join("pfx/system.reg")) {
            Ok(file) => {
                let mut header = Vec::new();
                file.take(4096).read_to_end(&mut header)?;
                !String::from_utf8_lossy(&header).contains("#arch=win32")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(error.into()),
        };
        let mut os = None;
        if recipes
            .iter()
            .any(|r| r.processes.iter().any(|p| p.requirement.os.is_some()))
        {
            let text = self.checked(&[
                "C:\\windows\\system32\\cmd.exe".into(),
                "/d".into(),
                "/c".into(),
                "ver".into(),
            ])?;
            os = Some(windows_version(&text).ok_or_else(|| {
                failure("Cannot determine prefix Windows version for Requirement_OS")
            })?);
        }
        // Validate every applicable path/value before making registry changes.
        for recipe in recipes {
            for value in &recipe.registry {
                script::expand(&value.key, &recipe.root)?;
                let expanded = script::expand(&value.value, &recipe.root)?;
                if value.dword {
                    script::dword(&expanded)?;
                }
            }
            for process in &recipe.processes {
                if !applies(process, win64, os.as_deref()) {
                    continue;
                }
                if let Some(key) = &process.has_run_key {
                    script::expand(key, &recipe.root)?;
                }
                for (path, args) in &process.programs {
                    let (host, windows) = script::program_path(path, &recipe.root)?;
                    let expanded = args
                        .iter()
                        .map(|arg| script::expand(arg, &recipe.root))
                        .collect::<Result<Vec<_>>>()?;
                    process_command(&host, &windows, &expanded)?;
                }
            }
        }
        for recipe in recipes {
            self.working_dir = recipe.root.clone();
            for value in &recipe.registry {
                self.reg_set(
                    &script::expand(&value.key, &recipe.root)?,
                    &value.name,
                    &script::expand(&value.value, &recipe.root)?,
                    value.dword,
                )?;
            }
            for process in &recipe.processes {
                if !applies(process, win64, os.as_deref())
                    || self.completed(process, &recipe.root)?
                {
                    continue;
                }
                if self.runtime_satisfied(process, &recipe.root)? {
                    writeln!(
                        self.log,
                        "[steam-tui] {} satisfied by the prefix's installed 14.x runtime",
                        process.name
                    )?;
                    continue;
                }
                writeln!(self.log, "[steam-tui] Setting up {}", process.name)?;
                for (path, args) in &process.programs {
                    let (host, windows) = script::program_path(path, &recipe.root)?;
                    let args = args
                        .iter()
                        .map(|arg| script::expand(arg, &recipe.root))
                        .collect::<Result<Vec<_>>>()?;
                    let command = process_command(&host, &windows, &args)?;
                    self.checked(&command)?;
                }
                if let Some(key) = &process.has_run_key {
                    self.reg_set(
                        &script::expand(key, &recipe.root)?,
                        &process.name,
                        &process.minimum.to_string(),
                        true,
                    )?;
                }
            }
        }
        self.log.sync_all()?;
        Ok(())
    }
}

pub(super) fn runtime_installed(text: &str) -> bool {
    let dword = |name: &str| {
        let fields: Vec<_> = text.split_whitespace().collect();
        fields.windows(3).find_map(|w| {
            (w[0].eq_ignore_ascii_case(name) && w[1].eq_ignore_ascii_case("REG_DWORD"))
                .then(|| {
                    w[2].strip_prefix("0x")
                        .and_then(|n| u32::from_str_radix(n, 16).ok())
                })
                .flatten()
        })
    };
    dword("Installed") == Some(1) && dword("Major") == Some(14)
}

fn applies(process: &Process, win64: bool, os: Option<&str>) -> bool {
    process
        .requirement
        .win64
        .is_none_or(|required| required == win64)
        && process
            .requirement
            .os
            .as_deref()
            .is_none_or(|required| os.is_some_and(|os| os.eq_ignore_ascii_case(required)))
}

fn windows_version(text: &str) -> Option<String> {
    // cmd's version number is independent of output language.
    [
        ("10.0.", "Windows 10"),
        ("6.3.", "Windows 8.1"),
        ("6.2.", "Windows 8"),
        ("6.1.", "Windows 7"),
        ("6.0.", "Windows Vista"),
        ("5.1.", "Windows XP"),
    ]
    .into_iter()
    .find(|(version, _)| text.contains(version))
    .map(|(_, name)| name.into())
}

fn process_command(host: &Path, windows: &str, args: &[String]) -> Result<Vec<String>> {
    match host
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "exe" => Ok(std::iter::once(windows.into())
            .chain(args.iter().cloned())
            .collect()),
        "msi" => Ok([
            "C:\\windows\\system32\\msiexec.exe".into(),
            "/i".into(),
            windows.into(),
        ]
        .into_iter()
        .chain(args.iter().cloned())
        .collect()),
        "cmd" | "bat" => {
            // Execute batch payloads with Wine's cmd, never the host shell.
            // cmd metacharacters in installation paths/arguments are rejected.
            let values: Vec<_> = std::iter::once(windows)
                .chain(args.iter().map(String::as_str))
                .collect();
            if values
                .iter()
                .any(|value| value.contains(['"', '&', '|', '<', '>', '^', '%', '!', '\r', '\n']))
            {
                return Err(failure(
                    "Unsupported command-shell characters in prerequisite batch invocation",
                ));
            }
            let line = values
                .iter()
                .map(|value| format!("\"{value}\""))
                .collect::<Vec<_>>()
                .join(" ");
            Ok(vec![
                "C:\\windows\\system32\\cmd.exe".into(),
                "/d".into(),
                "/s".into(),
                "/c".into(),
                format!("\"{line}\""),
            ])
        }
        _ => Err(failure(
            "Unsupported prerequisite executable type; expected exe, msi, cmd or bat",
        )),
    }
}
