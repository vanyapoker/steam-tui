//! Virtual XInput gamepad for games started without Valve's client.
//!
//! Valve's client grabs the physical pad and shows games one stable virtual pad (USB id
//! 28de:11ff, named "Microsoft X-Box 360 pad N"), hiding the physical one with
//! `SDL_GAMECONTROLLER_IGNORE_DEVICES`. This does the same for a single Xbox-compatible
//! pad: input is forwarded 1:1 (no remapping), rumble is forwarded back, and the
//! virtual pad outlives Bluetooth drop-outs so the game never sees a disconnect.
//! Without `/dev/uinput` access, or without a supported pad, games get the raw device
//! exactly as before.
mod device;
mod ioctl;
mod layout;

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SLOT: u32 = 0;
pub const DISABLE_VAR: &str = "STEAM_TUI_NO_VIRTUAL_GAMEPAD";
/// Tells the Steam API library which pad type to report through Steam Input.
const PRESENCE_VAR: &str = "STEAM_TUI_VIRTUAL_GAMEPAD";

/// How games identify the physical pad behind the virtual one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    vendor: u16,
    product: u16,
    name: String,
}

/// Environment variables Valve's client sets for games while its virtual pad exists.
fn environment(
    identity: &Identity,
    info_file: Option<&Path>,
    proton: bool,
) -> Vec<(String, String)> {
    let mut variables = vec![
        (
            "SDL_GAMECONTROLLER_IGNORE_DEVICES".to_owned(),
            format!("0x{:04x}/0x{:04x},", identity.vendor, identity.product),
        ),
        (
            "SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD".to_owned(),
            "1".to_owned(),
        ),
        ("SDL_JOYSTICK_HIDAPI_STEAMXBOX".to_owned(), "0".to_owned()),
        // For the Steam API library inside the game (Steam Input device presence).
        (
            PRESENCE_VAR.to_owned(),
            layout::kind(identity.vendor, identity.product).to_owned(),
        ),
    ];
    if let Some(file) = info_file {
        let name = if proton {
            "SteamVirtualGamepadInfo_Proton"
        } else {
            "SteamVirtualGamepadInfo"
        };
        variables.push((name.to_owned(), file.to_string_lossy().into_owned()));
    }
    variables
}

/// The file SDL reads to learn the real identity behind each virtual pad slot.
fn info_text(identity: &Identity) -> String {
    let kind = layout::kind(identity.vendor, identity.product);
    format!(
        "[slot {SLOT}]\nname={}\nVID=0x{:04x}\nPID=0x{:04x}\ntype={kind}\n",
        identity.name.replace(['\r', '\n'], " "),
        identity.vendor,
        identity.product
    )
}

/// Keeps the virtual pad alive for one game; dropping it removes the pad and releases
/// the physical one.
pub struct Service {
    stop: Arc<OwnedFd>,
    thread: Option<std::thread::JoinHandle<()>>,
    info_file: Option<PathBuf>,
}

impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: the eventfd is owned by `stop` for the whole call.
        unsafe { libc::eventfd_write(self.stop.as_raw_fd(), 1) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(file) = &self.info_file {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// Starts mirroring the connected pad and adds the variables games need to
/// `environment`. Returns `None`, leaving the environment alone, when there is nothing
/// to mirror or the virtual pad cannot be created. `note` reports user-visible events.
pub fn start(
    environment_vars: &mut BTreeMap<String, String>,
    steam_root: &Path,
    app_id: u32,
    note: impl Fn(&str) + Send + 'static,
) -> Option<Service> {
    if std::env::var_os(DISABLE_VAR).is_some() {
        return None;
    }
    let pad = device::scan()?;
    let identity = Identity {
        vendor: pad.caps.vendor,
        product: pad.caps.product,
        name: pad.caps.name.clone(),
    };
    let virtual_pad = match device::Virtual::create(SLOT) {
        Ok(virtual_pad) => virtual_pad,
        Err(error) => {
            let hint = device::uinput_error()
                .map(|_| " (is /dev/uinput writable for your user?)")
                .unwrap_or_default();
            note(&format!(
                "Virtual gamepad unavailable: {error}{hint}; the game sees {} directly",
                identity.name
            ));
            return None;
        }
    };
    let info_file = steam_root
        .join("config")
        .join(format!("steam-tui-virtualgamepad-{app_id}.txt"));
    let info_file = std::fs::write(&info_file, info_text(&identity))
        .map(|()| info_file)
        .map_err(|error| note(&format!("Gamepad info file not written: {error}")))
        .ok();
    let proton = environment_vars.contains_key("STEAM_COMPAT_DATA_PATH");
    // Explicit launch options win over what is set here.
    for (key, value) in environment(&identity, info_file.as_deref(), proton) {
        environment_vars.entry(key).or_insert(value);
    }
    // SAFETY: plain eventfd creation; the result is checked before it is owned.
    let stop = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if stop < 0 {
        return None;
    }
    // SAFETY: `stop` is a fresh descriptor nothing else owns.
    let stop = Arc::new(unsafe { OwnedFd::from_raw_fd(stop) });
    note(&format!(
        "Virtual gamepad \"Microsoft X-Box 360 pad {SLOT}\" mirrors {} ({}); input is forwarded unchanged",
        identity.name,
        pad.path.display()
    ));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("virtual-gamepad".into())
            .spawn(move || device::run(stop, virtual_pad, pad, note, device::scan))
            .ok()
    };
    Some(Service {
        stop,
        thread,
        info_file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            vendor: 0x045e,
            product: 0x0b13,
            name: "Xbox Wireless Controller".into(),
        }
    }

    #[test]
    fn environment_hides_the_physical_pad_and_points_at_the_info_file() {
        let file = Path::new("/steam/config/info.txt");
        let native: BTreeMap<_, _> = environment(&identity(), Some(file), false)
            .into_iter()
            .collect();
        assert_eq!(
            native["SDL_GAMECONTROLLER_IGNORE_DEVICES"],
            "0x045e/0x0b13,"
        );
        assert_eq!(
            native["SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD"],
            "1"
        );
        assert_eq!(native["SDL_JOYSTICK_HIDAPI_STEAMXBOX"], "0");
        assert_eq!(native["STEAM_TUI_VIRTUAL_GAMEPAD"], "xboxone");
        assert_eq!(native["SteamVirtualGamepadInfo"], "/steam/config/info.txt");
        assert!(!native.contains_key("SteamVirtualGamepadInfo_Proton"));
        let proton: BTreeMap<_, _> = environment(&identity(), Some(file), true)
            .into_iter()
            .collect();
        assert_eq!(
            proton["SteamVirtualGamepadInfo_Proton"],
            "/steam/config/info.txt"
        );
        assert!(!proton.contains_key("SteamVirtualGamepadInfo"));
        let none = environment(&identity(), None, true);
        assert!(none.iter().all(|(key, _)| !key.starts_with("SteamVirtual")));
        let dualshock = Identity {
            vendor: 0x054c,
            product: 0x09cc,
            name: "Wireless Controller".into(),
        };
        let dualshock: BTreeMap<_, _> = environment(&dualshock, None, false).into_iter().collect();
        assert_eq!(dualshock["STEAM_TUI_VIRTUAL_GAMEPAD"], "ps4");
        assert_eq!(
            dualshock["SDL_GAMECONTROLLER_IGNORE_DEVICES"],
            "0x054c/0x09cc,"
        );
    }

    #[test]
    fn info_file_uses_the_format_sdl_parses() {
        assert_eq!(
            info_text(&identity()),
            "[slot 0]\nname=Xbox Wireless Controller\nVID=0x045e\nPID=0x0b13\ntype=xboxone\n"
        );
        let wired = Identity {
            vendor: 0x045e,
            product: 0x028e,
            name: "Microsoft X-Box 360 pad\n".into(),
        };
        let text = info_text(&wired);
        assert!(text.contains("type=xbox360"));
        assert!(text.contains("name=Microsoft X-Box 360 pad \n"));
    }
}
