//! Parse the Windows install-script subset before any action is executed.
//! Unsupported setup directives fail explicitly; uninstall directives are kept
//! out of the launch phase.
use super::*;

#[derive(Debug)]
pub(super) struct Recipe {
    pub root: PathBuf,
    pub registry: Vec<RegValue>,
    pub processes: Vec<Process>,
}
#[derive(Debug)]
pub(super) struct RegValue {
    pub key: String,
    pub name: String,
    pub value: String,
    pub dword: bool,
}
#[derive(Debug)]
pub(super) struct Process {
    pub name: String,
    pub programs: Vec<(String, Vec<String>)>,
    pub has_run_key: Option<String>,
    pub minimum: u32,
    pub requirement: Requirement,
}
#[derive(Debug, Default)]
pub(super) struct Requirement {
    pub win64: Option<bool>,
    pub os: Option<String>,
}

fn scalar(node: &Node) -> Result<&str> {
    node.value
        .as_deref()
        .ok_or_else(|| failure("Expected scalar in install script"))
}
fn boolean(value: &str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(failure("Invalid boolean in install script")),
    }
}

pub(super) fn parse(source: &str, root: &Path, language: &str) -> Result<Recipe> {
    let document = crate::installed::text(source)?;
    let script = document
        .child("installscript")
        .ok_or_else(|| failure("Depot-flagged install script has no InstallScript section"))?;
    let mut result = Recipe {
        root: root.to_owned(),
        registry: Vec::new(),
        processes: Vec::new(),
    };
    for (section, node) in &script.children {
        match section.to_ascii_lowercase().as_str() {
            "registry" => {
                for (key, registry) in &node.children {
                    if ![
                        "hkey_local_machine\\",
                        "hkey_current_user\\",
                        "hkey_classes_root\\",
                        "hkey_users\\",
                    ]
                    .iter()
                    .any(|prefix| key.to_ascii_lowercase().starts_with(prefix))
                    {
                        return Err(failure("Unsupported registry hive in install script"));
                    }
                    for (kind, entries) in &registry.children {
                        if !matches!(kind.to_ascii_lowercase().as_str(), "string" | "dword") {
                            return Err(failure(format!(
                                "Unsupported install-script registry type {kind}"
                            )));
                        }
                        for (name, entry) in &entries.children {
                            if let Some(value) = &entry.value {
                                result.registry.push(RegValue {
                                    key: key.clone(),
                                    name: name.clone(),
                                    value: value.clone(),
                                    dword: kind.eq_ignore_ascii_case("dword"),
                                });
                            } else if name.eq_ignore_ascii_case(language) {
                                for (name, value) in &entry.children {
                                    result.registry.push(RegValue {
                                        key: key.clone(),
                                        name: name.clone(),
                                        value: scalar(value)?.into(),
                                        dword: kind.eq_ignore_ascii_case("dword"),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            "run process" => {
                for (name, entry) in &node.children {
                    let mut process = Process {
                        name: name.clone(),
                        programs: Vec::new(),
                        has_run_key: entry.text("hasrunkey").map(str::to_owned),
                        minimum: 1,
                        requirement: Requirement::default(),
                    };
                    let mut programs = BTreeMap::new();
                    let mut commands = BTreeMap::new();
                    for (key, value) in &entry.children {
                        let lower = key.to_ascii_lowercase();
                        if let Some((kind, index)) = lower.split_once(' ')
                            && matches!(kind, "process" | "command")
                        {
                            let index = index
                                .parse::<u32>()
                                .ok()
                                .filter(|i| (1..=32).contains(i))
                                .ok_or_else(|| failure("Invalid install-script process index"))?;
                            if kind == "process" {
                                programs.insert(index, scalar(value)?.to_owned());
                            } else {
                                commands.insert(index, windows_arguments(scalar(value)?)?);
                            }
                            continue;
                        }
                        match lower.as_str() {
                            "hasrunkey" => {
                                scalar(value)?;
                            }
                            "minimumhasrunvalue" => {
                                process.minimum = scalar(value)?
                                    .parse::<u32>()
                                    .map_err(|_| failure("Invalid MinimumHasRunValue"))?
                                    .max(1);
                            }
                            "nocleanup" | "ascurrentuser" => {
                                boolean(scalar(value)?)?;
                            }
                            "requirement_os" => {
                                for (key, value) in &value.children {
                                    match key.to_ascii_lowercase().as_str() {
                                        "is64bitwindows" => {
                                            process.requirement.win64 =
                                                Some(boolean(scalar(value)?)?)
                                        }
                                        "ostype" => {
                                            process.requirement.os = Some(scalar(value)?.into())
                                        }
                                        _ => {
                                            return Err(failure(format!(
                                                "Unsupported install-script OS requirement {key}"
                                            )));
                                        }
                                    }
                                }
                            }
                            _ => {
                                return Err(failure(format!(
                                    "Unsupported install-script process directive {key}"
                                )));
                            }
                        }
                    }
                    if !programs.contains_key(&1)
                        || commands.keys().any(|index| !programs.contains_key(index))
                    {
                        return Err(failure("Install-script command lacks a matching process"));
                    }
                    for (index, program) in programs {
                        process
                            .programs
                            .push((program, commands.remove(&index).unwrap_or_default()));
                    }
                    result.processes.push(process);
                }
            }
            "run process on uninstall" => {}
            _ => {
                return Err(failure(format!(
                    "Unsupported first-run install-script section {section}"
                )));
            }
        }
        if result.registry.len() + result.processes.len() > 1024 {
            return Err(failure("Too many install-script actions"));
        }
    }
    Ok(result)
}

pub(super) fn dword(value: &str) -> Result<u32> {
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map(|hex| u32::from_str_radix(hex, 16))
        .unwrap_or_else(|| value.parse())
        .map_err(|_| failure("Install-script DWORD is not a valid unsigned integer"))
}

/// Windows CRT quoting rules; backslashes in paths are not POSIX escapes.
pub(super) fn windows_arguments(input: &str) -> Result<Vec<String>> {
    if input.len() > 64 * 1024 || input.contains(['\0', '\r', '\n']) {
        return Err(failure("Invalid install-script command line"));
    }
    let mut result = Vec::new();
    let mut text = input.chars().peekable();
    while text.peek().is_some() {
        while text.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            text.next();
        }
        if text.peek().is_none() {
            break;
        }
        let mut arg = String::new();
        let mut quoted = false;
        while let Some(&character) = text.peek() {
            if !quoted && character.is_ascii_whitespace() {
                break;
            }
            if character == '\\' {
                let mut slashes = 0;
                while text.peek() == Some(&'\\') {
                    slashes += 1;
                    text.next();
                }
                if text.peek() == Some(&'"') {
                    arg.extend(std::iter::repeat_n('\\', slashes / 2));
                    if slashes % 2 == 1 {
                        arg.push('"');
                        text.next();
                    } else {
                        quoted = !quoted;
                        text.next();
                    }
                } else {
                    arg.extend(std::iter::repeat_n('\\', slashes));
                }
            } else if character == '"' {
                text.next();
                if quoted && text.peek() == Some(&'"') {
                    arg.push('"');
                    text.next();
                } else {
                    quoted = !quoted;
                }
            } else {
                arg.push(character);
                text.next();
            }
        }
        if quoted {
            return Err(failure("Unterminated quote in Windows install command"));
        }
        result.push(arg);
    }
    Ok(result)
}

pub(super) fn expand(input: &str, root: &Path) -> Result<String> {
    if input.contains(['\0', '\r', '\n']) {
        return Err(failure("Invalid install-script value"));
    }
    let root = format!("Z:{}", root.to_string_lossy().replace('/', "\\"));
    let variables = [
        ("installdir", root.as_str()),
        ("rootdrive", "Z"),
        ("windir", "C:\\windows"),
        ("systemroot", "C:\\windows"),
        ("appdata", "C:\\users\\steamuser\\AppData\\Roaming"),
        ("localappdata", "C:\\users\\steamuser\\AppData\\Local"),
        ("user_mydocs", "C:\\users\\steamuser\\Documents"),
        ("common_mydocs", "C:\\users\\Public\\Documents"),
    ];
    let mut result = String::new();
    let mut rest = input;
    while let Some(index) = rest.find('%') {
        result.push_str(&rest[..index]);
        rest = &rest[index + 1..];
        let end = rest
            .find('%')
            .ok_or_else(|| failure("Unterminated install-script variable"))?;
        let name = &rest[..end];
        let value = variables
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v)
            .ok_or_else(|| failure(format!("Unsupported install-script variable %{name}%")))?;
        result.push_str(value);
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

pub(super) fn program_path(input: &str, root: &Path) -> Result<(PathBuf, String)> {
    let text = input.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(text);
    let expanded = expand(text, root)?.replace('/', "\\");
    let windows_root = expand("%INSTALLDIR%", root)?;
    let relative = expanded
        .strip_prefix(&format!("{windows_root}\\"))
        .or_else(|| {
            (!expanded.contains(':') && !expanded.starts_with(['/', '\\']))
                .then_some(expanded.as_str())
        })
        .ok_or_else(|| {
            failure("Prerequisite executable must be inside its verified installation")
        })?;
    let path = contained(root, &relative.replace('\\', "/"))?;
    if !path.is_file() {
        return Err(failure("Prerequisite executable is not a regular file"));
    }
    if path.file_name().and_then(|p| p.to_str()).is_some_and(|p| {
        ["steam.exe", "steamwebhelper.exe"]
            .iter()
            .any(|name| p.eq_ignore_ascii_case(name))
    }) {
        return Err(failure(
            "Steam client helpers are not first-run prerequisites",
        ));
    }
    Ok((
        path.clone(),
        format!("Z:{}", path.to_string_lossy().replace('/', "\\")),
    ))
}
