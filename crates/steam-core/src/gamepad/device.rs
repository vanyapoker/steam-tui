//! evdev access to the physical pad, the uinput virtual pad and the forwarding loop.
use super::ioctl::*;
use super::layout::*;
use libc::c_int;
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RESCAN: Duration = Duration::from_secs(1);
const FF_EFFECTS_MAX: u32 = 16;

fn open(path: &Path, flags: c_int) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid NUL-terminated path; the result is checked before use.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ioctl<T>(fd: &OwnedFd, request: libc::c_ulong, argument: *mut T) -> io::Result<()> {
    // SAFETY: every caller passes a pointer to a live value of the type the request expects.
    if unsafe { libc::ioctl(fd.as_raw_fd(), request, argument) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ioctl_int(fd: &OwnedFd, request: libc::c_ulong, value: c_int) -> io::Result<()> {
    // SAFETY: requests taking an int by value.
    if unsafe { libc::ioctl(fd.as_raw_fd(), request, value) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn test_bit(bits: &[u8], bit: usize) -> bool {
    bits.get(bit / 8).is_some_and(|b| b >> (bit % 8) & 1 == 1)
}

fn event_bits(fd: &OwnedFd, event: u16, bytes: usize) -> io::Result<Vec<u8>> {
    let mut bits = vec![0u8; bytes];
    ioctl(fd, eviocgbit(event, bytes), bits.as_mut_ptr())?;
    Ok(bits)
}

fn read_capabilities(fd: &OwnedFd) -> io::Result<Capabilities> {
    // SAFETY: plain-old-data kernel structs.
    let mut id: libc::input_id = unsafe { zeroed() };
    ioctl(fd, EVIOCGID, &mut id)?;
    let mut name = [0u8; 128];
    ioctl(fd, eviocgname(name.len()), name.as_mut_ptr())?;
    let name = String::from_utf8_lossy(&name[..name.iter().position(|b| *b == 0).unwrap_or(0)])
        .into_owned();
    let mut caps = Capabilities {
        vendor: id.vendor,
        product: id.product,
        name,
        ..Default::default()
    };
    let types = event_bits(fd, 0, 4)?;
    if !test_bit(&types, EV_ABS.into()) || !test_bit(&types, EV_KEY.into()) {
        return Ok(caps);
    }
    let abs = event_bits(fd, EV_ABS, 8)?;
    for code in 0..64u16 {
        if test_bit(&abs, code.into()) {
            // SAFETY: plain-old-data kernel struct.
            let mut info: libc::input_absinfo = unsafe { zeroed() };
            ioctl(fd, eviocgabs(code), &mut info)?;
            caps.abs.insert(
                code,
                Range {
                    min: info.minimum,
                    max: info.maximum,
                },
            );
        }
    }
    let keys = event_bits(fd, EV_KEY, 96)?;
    caps.keys = (0..0x300u16)
        .filter(|code| test_bit(&keys, (*code).into()))
        .collect();
    if test_bit(&types, EV_FF.into())
        && let Ok(ff) = event_bits(fd, EV_FF, 16)
    {
        caps.rumble = test_bit(&ff, FF_RUMBLE.into());
    }
    Ok(caps)
}

pub struct Physical {
    fd: OwnedFd,
    pub path: PathBuf,
    pub caps: Capabilities,
    layout: Layout,
    writable: bool,
}

/// The first connected pad this module can mirror.
pub fn scan() -> Option<Physical> {
    scan_where(|_| true)
}

fn scan_where(wanted: impl Fn(&Capabilities) -> bool) -> Option<Physical> {
    let mut nodes: Vec<(u32, PathBuf)> = std::fs::read_dir("/dev/input")
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let number = entry
                .file_name()
                .to_str()?
                .strip_prefix("event")?
                .parse()
                .ok()?;
            Some((number, entry.path()))
        })
        .collect();
    nodes.sort();
    for (_, path) in nodes {
        let flags = libc::O_NONBLOCK;
        let (fd, writable) = match open(&path, flags | libc::O_RDWR) {
            Ok(fd) => (fd, true),
            Err(_) => match open(&path, flags | libc::O_RDONLY) {
                Ok(fd) => (fd, false),
                Err(_) => continue,
            },
        };
        let Ok(caps) = read_capabilities(&fd) else {
            continue;
        };
        if wanted(&caps)
            && let Some(layout) = detect(&caps)
        {
            return Some(Physical {
                fd,
                path,
                caps,
                layout,
                writable,
            });
        }
    }
    None
}

/// Why no virtual pad can be offered, for a user-visible notice (only when a pad exists).
pub fn uinput_error() -> Option<io::Error> {
    open(Path::new("/dev/uinput"), libc::O_RDWR | libc::O_NONBLOCK).err()
}

pub struct Virtual {
    fd: OwnedFd,
}

impl Virtual {
    pub fn create(slot: u32) -> io::Result<Self> {
        let fd = open(Path::new("/dev/uinput"), libc::O_RDWR | libc::O_NONBLOCK)?;
        ioctl_int(&fd, UI_SET_EVBIT, EV_KEY.into())?;
        ioctl_int(&fd, UI_SET_EVBIT, EV_ABS.into())?;
        ioctl_int(&fd, UI_SET_EVBIT, EV_FF.into())?;
        for button in BUTTONS {
            ioctl_int(&fd, UI_SET_KEYBIT, button.into())?;
        }
        ioctl_int(&fd, UI_SET_FFBIT, FF_RUMBLE.into())?;
        for (axis, min, max, fuzz, flat) in AXES {
            ioctl_int(&fd, UI_SET_ABSBIT, axis.into())?;
            let mut setup = libc::uinput_abs_setup {
                code: axis,
                absinfo: libc::input_absinfo {
                    value: 0,
                    minimum: min,
                    maximum: max,
                    fuzz,
                    flat,
                    resolution: 0,
                },
            };
            ioctl(&fd, UI_ABS_SETUP, &mut setup)?;
        }
        // SAFETY: plain-old-data kernel struct.
        let mut setup: libc::uinput_setup = unsafe { zeroed() };
        setup.id = libc::input_id {
            bustype: BUS_USB,
            vendor: VIRTUAL_VENDOR,
            product: VIRTUAL_PRODUCT,
            version: 0,
        };
        // SDL and Wine read the slot from the digits after "pad ".
        for (target, byte) in setup
            .name
            .iter_mut()
            .zip(format!("Microsoft X-Box 360 pad {slot}").bytes())
        {
            *target = byte as libc::c_char;
        }
        setup.ff_effects_max = FF_EFFECTS_MAX;
        ioctl(&fd, UI_DEV_SETUP, &mut setup)?;
        ioctl(&fd, UI_DEV_CREATE, std::ptr::null_mut::<u8>())?;
        Ok(Self { fd })
    }

    fn emit(&self, events: &[Event]) {
        // SAFETY: plain-old-data kernel struct.
        let blank: libc::input_event = unsafe { zeroed() };
        let mut batch: Vec<libc::input_event> = events
            .iter()
            .map(|(type_, code, value)| libc::input_event {
                type_: *type_,
                code: *code,
                value: *value,
                ..blank
            })
            .collect();
        batch.push(libc::input_event {
            type_: EV_SYN,
            code: SYN_REPORT,
            ..blank
        });
        // SAFETY: the buffer is `batch.len()` live events; a short or failed write only
        // loses one input frame.
        unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                batch.as_ptr().cast(),
                batch.len() * size_of::<libc::input_event>(),
            );
        }
    }
}

struct Active {
    pad: Physical,
    translator: Translator,
}

fn read_events(fd: &OwnedFd, buffer: &mut [libc::input_event]) -> io::Result<usize> {
    // SAFETY: the kernel fills at most `buffer.len()` whole events.
    let read = unsafe {
        libc::read(
            fd.as_raw_fd(),
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(buffer),
        )
    };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(read as usize / size_of::<libc::input_event>())
}

impl Active {
    /// Emits the pad's current state (after attaching or a dropped-events notice).
    fn resync(&mut self, virt: &Virtual) {
        let mut events = Vec::new();
        for code in self.pad.caps.abs.keys().copied().collect::<Vec<_>>() {
            // SAFETY: plain-old-data kernel struct.
            let mut info: libc::input_absinfo = unsafe { zeroed() };
            if ioctl(&self.pad.fd, eviocgabs(code), &mut info).is_ok() {
                events.extend(self.translator.translate(EV_ABS, code, info.value));
            }
        }
        let mut keys = [0u8; 96];
        if ioctl(&self.pad.fd, eviocgkey(keys.len()), keys.as_mut_ptr()).is_ok() {
            for code in self.pad.caps.keys.iter().copied().collect::<Vec<_>>() {
                let down = i32::from(test_bit(&keys, code.into()));
                events.extend(self.translator.translate(EV_KEY, code, down));
            }
        }
        virt.emit(&events);
    }

    /// Forwards everything the pad has queued. `Err` means the pad is gone.
    fn pump(&mut self, virt: &Virtual) -> io::Result<()> {
        // SAFETY: plain-old-data kernel struct.
        let mut buffer: [libc::input_event; 64] = unsafe { zeroed() };
        let mut frame: Vec<Event> = Vec::new();
        let mut dropped = false;
        loop {
            let count = match read_events(&self.pad.fd, &mut buffer) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            for event in &buffer[..count] {
                match (event.type_, event.code) {
                    (EV_SYN, SYN_DROPPED) => {
                        dropped = true;
                        frame.clear();
                    }
                    (EV_SYN, SYN_REPORT) if dropped => {
                        dropped = false;
                        self.resync(virt);
                    }
                    (EV_SYN, SYN_REPORT) => {
                        if !frame.is_empty() {
                            virt.emit(&std::mem::take(&mut frame));
                        }
                    }
                    _ if dropped => {}
                    (kind, code) => {
                        frame.extend(self.translator.translate(kind, code, event.value))
                    }
                }
            }
        }
    }
}

/// Rumble effects uploaded to the virtual pad, mapped onto the physical pad's slots.
#[derive(Default)]
struct Effects(HashMap<i16, i16>);

impl Effects {
    fn upload(&mut self, virt: &Virtual, active: Option<&Active>, request: u32) {
        // SAFETY: plain-old-data kernel struct.
        let mut upload: libc::uinput_ff_upload = unsafe { zeroed() };
        upload.request_id = request;
        if ioctl(&virt.fd, UI_BEGIN_FF_UPLOAD, &mut upload).is_err() {
            return;
        }
        let mut retval = 0;
        if let Some(active) = active.filter(|a| a.pad.writable) {
            let virtual_id = upload.effect.id;
            let mut effect = upload.effect;
            effect.id = self.0.get(&virtual_id).copied().unwrap_or(-1);
            match ioctl(&active.pad.fd, EVIOCSFF, &mut effect) {
                Ok(()) => {
                    self.0.insert(virtual_id, effect.id);
                }
                Err(error) => retval = -error.raw_os_error().unwrap_or(libc::EINVAL),
            }
        }
        upload.retval = retval;
        let _ = ioctl(&virt.fd, UI_END_FF_UPLOAD, &mut upload);
    }

    fn erase(&mut self, virt: &Virtual, active: Option<&Active>, request: u32) {
        // SAFETY: plain-old-data kernel struct.
        let mut erase: libc::uinput_ff_erase = unsafe { zeroed() };
        erase.request_id = request;
        if ioctl(&virt.fd, UI_BEGIN_FF_ERASE, &mut erase).is_err() {
            return;
        }
        if let Some(physical) = self.0.remove(&(erase.effect_id as i16))
            && let Some(active) = active
        {
            let _ = ioctl_int(&active.pad.fd, EVIOCRMFF, physical.into());
        }
        erase.retval = 0;
        let _ = ioctl(&virt.fd, UI_END_FF_ERASE, &mut erase);
    }

    fn play(&self, active: Option<&Active>, code: u16, value: i32) {
        let Some(active) = active.filter(|a| a.pad.writable) else {
            return;
        };
        // Codes above the effect slots (gain, autocenter) address the device itself.
        let code = if code >= 0x60 {
            code
        } else {
            match self.0.get(&(code as i16)) {
                Some(physical) => *physical as u16,
                None => return,
            }
        };
        // SAFETY: plain-old-data kernel struct; the write sends exactly one event.
        unsafe {
            let mut event: libc::input_event = zeroed();
            event.type_ = EV_FF;
            event.code = code;
            event.value = value;
            libc::write(
                active.pad.fd.as_raw_fd(),
                (&raw const event).cast(),
                size_of::<libc::input_event>(),
            );
        }
    }
}

/// Answers force-feedback requests the game makes on the virtual pad.
fn serve_virtual(virt: &Virtual, active: Option<&Active>, effects: &mut Effects) {
    // SAFETY: plain-old-data kernel struct.
    let mut buffer: [libc::input_event; 16] = unsafe { zeroed() };
    loop {
        let Ok(count) = read_events(&virt.fd, &mut buffer) else {
            return;
        };
        if count == 0 {
            return;
        }
        for event in &buffer[..count] {
            match (event.type_, event.code) {
                (EV_UINPUT, UI_FF_UPLOAD) => effects.upload(virt, active, event.value as u32),
                (EV_UINPUT, UI_FF_ERASE) => effects.erase(virt, active, event.value as u32),
                (EV_FF, code) => effects.play(active, code, event.value),
                _ => {}
            }
        }
    }
}

/// Mirrors `first` (and any pad that replaces it) onto `virt` until `stop` fires. The
/// virtual pad stays alive, at rest, while the physical one is away, so the game never
/// sees a disconnect.
pub fn run(
    stop: Arc<OwnedFd>,
    virt: Virtual,
    first: Physical,
    note: impl Fn(&str),
    rescan: impl Fn() -> Option<Physical>,
) {
    let mut effects = Effects::default();
    let attach = |pad: Physical, virt: &Virtual| -> Active {
        if let Err(error) = ioctl_int(&pad.fd, EVIOCGRAB, 1) {
            note(&format!(
                "Could not grab {} ({error}); the game may also see the physical pad",
                pad.path.display()
            ));
        }
        let translator = Translator::new(pad.layout.clone(), &pad.caps);
        let mut active = Active { pad, translator };
        active.resync(virt);
        active
    };
    let mut active: Option<Active> = Some(attach(first, &virt));
    let mut last_scan = Instant::now();
    loop {
        let mut fds = vec![
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: virt.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if let Some(active) = &active {
            fds.push(libc::pollfd {
                fd: active.pad.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let timeout = if active.is_some() {
            -1
        } else {
            RESCAN.saturating_sub(last_scan.elapsed()).as_millis() as c_int
        };
        // SAFETY: `fds` is a live array of `fds.len()` pollfd entries.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[0].revents != 0 {
            return;
        }
        if fds[1].revents & libc::POLLIN != 0 {
            serve_virtual(&virt, active.as_ref(), &mut effects);
        }
        if fds.len() > 2 && fds[2].revents != 0 {
            let lost = active.as_mut().is_some_and(|a| a.pump(&virt).is_err());
            if lost {
                if let Some(gone) = active.take() {
                    note(&format!(
                        "{} disconnected; the virtual pad stays",
                        gone.pad.caps.name
                    ));
                }
                effects.0.clear();
                virt.emit(&Translator::neutral());
                last_scan = Instant::now();
            }
        }
        if active.is_none() && last_scan.elapsed() >= RESCAN {
            last_scan = Instant::now();
            if let Some(pad) = rescan() {
                note(&format!("{} reconnected", pad.caps.name));
                active = Some(attach(pad, &virt));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake Bluetooth-style Xbox pad on /dev/uinput (Brake/Gas triggers, Z/RZ right stick).
    fn fake_pad(name: &str) -> OwnedFd {
        let fd = open(Path::new("/dev/uinput"), libc::O_RDWR | libc::O_NONBLOCK).unwrap();
        for event in [EV_KEY, EV_ABS] {
            ioctl_int(&fd, UI_SET_EVBIT, event.into()).unwrap();
        }
        for key in [
            BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST, BTN_TL, BTN_TR, BTN_SELECT, BTN_START,
            BTN_MODE, BTN_THUMBL, BTN_THUMBR,
        ] {
            ioctl_int(&fd, UI_SET_KEYBIT, key.into()).unwrap();
        }
        for (axis, min, max) in [
            (ABS_X, 0, 65535),
            (ABS_Y, 0, 65535),
            (ABS_Z, 0, 65535),
            (ABS_RZ, 0, 65535),
            (ABS_GAS, 0, 1023),
            (ABS_BRAKE, 0, 1023),
            (ABS_HAT0X, -1, 1),
            (ABS_HAT0Y, -1, 1),
        ] {
            ioctl_int(&fd, UI_SET_ABSBIT, axis.into()).unwrap();
            let mut setup = libc::uinput_abs_setup {
                code: axis,
                absinfo: libc::input_absinfo {
                    value: 0,
                    minimum: min,
                    maximum: max,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            };
            ioctl(&fd, UI_ABS_SETUP, &mut setup).unwrap();
        }
        let mut setup: libc::uinput_setup = unsafe { zeroed() };
        setup.id = libc::input_id {
            bustype: 5,
            vendor: 0x045e,
            product: 0x0b13,
            version: 0x0513,
        };
        for (target, byte) in setup.name.iter_mut().zip(name.bytes()) {
            *target = byte as libc::c_char;
        }
        ioctl(&fd, UI_DEV_SETUP, &mut setup).unwrap();
        ioctl(&fd, UI_DEV_CREATE, std::ptr::null_mut::<u8>()).unwrap();
        fd
    }

    fn write_events(fd: &OwnedFd, events: &[Event]) {
        let blank: libc::input_event = unsafe { zeroed() };
        let mut batch: Vec<libc::input_event> = events
            .iter()
            .map(|(type_, code, value)| libc::input_event {
                type_: *type_,
                code: *code,
                value: *value,
                ..blank
            })
            .collect();
        batch.push(libc::input_event {
            type_: EV_SYN,
            code: SYN_REPORT,
            ..blank
        });
        let written = unsafe {
            libc::write(
                fd.as_raw_fd(),
                batch.as_ptr().cast(),
                batch.len() * size_of::<libc::input_event>(),
            )
        };
        assert!(written > 0);
    }

    /// Finds an event node by exact device name, waiting for udev to grant access.
    fn find_node(name: &str) -> OwnedFd {
        for _ in 0..60 {
            for entry in std::fs::read_dir("/dev/input").unwrap().flatten() {
                if !entry.file_name().to_string_lossy().starts_with("event") {
                    continue;
                }
                if let Ok(fd) = open(&entry.path(), libc::O_RDWR | libc::O_NONBLOCK)
                    && let Ok(caps) = read_capabilities(&fd)
                    && caps.name == name
                {
                    return fd;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("no accessible event node named {name:?}");
    }

    fn drain(fd: &OwnedFd, wanted: usize) -> Vec<Event> {
        let mut seen = Vec::new();
        // SAFETY: plain-old-data kernel struct.
        let mut buffer: [libc::input_event; 32] = unsafe { zeroed() };
        for _ in 0..50 {
            if let Ok(count) = read_events(fd, &mut buffer) {
                seen.extend(
                    buffer[..count]
                        .iter()
                        .filter(|e| e.type_ != EV_SYN)
                        .map(|e| (e.type_, e.code, e.value)),
                );
            }
            if seen.len() >= wanted {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        seen
    }

    /// Needs write access to /dev/uinput: `cargo test -p steam-core gamepad -- --ignored`.
    #[test]
    #[ignore]
    fn forwards_a_bluetooth_pad_onto_the_virtual_xbox_360_pad() {
        let fake = fake_pad("steam-tui test Xbox Wireless Controller");
        let physical = (0..60)
            .find_map(|_| {
                let found = scan_where(|caps| caps.name.starts_with("steam-tui test"));
                if found.is_none() {
                    std::thread::sleep(Duration::from_millis(100));
                }
                found
            })
            .expect("the fake pad is eligible and accessible");
        assert!(
            physical.caps.name.starts_with("steam-tui test"),
            "{}",
            physical.caps.name
        );
        assert!(physical.writable || physical.path.exists());
        let virt = Virtual::create(9).unwrap();
        let stop = Arc::new(unsafe {
            OwnedFd::from_raw_fd(libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))
        });
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                run(
                    stop,
                    virt,
                    physical,
                    |_| {},
                    || scan_where(|caps| caps.name.starts_with("steam-tui test")),
                )
            })
        };
        let node = find_node("Microsoft X-Box 360 pad 9");
        let caps = read_capabilities(&node).unwrap();
        assert_eq!(
            (caps.vendor, caps.product),
            (VIRTUAL_VENDOR, VIRTUAL_PRODUCT)
        );
        assert!(caps.rumble);
        drain(&node, 0);

        write_events(
            &fake,
            &[
                (EV_KEY, BTN_SOUTH, 1),
                (EV_ABS, ABS_BRAKE, 1023),
                (EV_ABS, ABS_X, 65535),
                (EV_ABS, ABS_Z, 65535),
            ],
        );
        let seen = drain(&node, 4);
        assert!(seen.contains(&(EV_KEY, BTN_SOUTH, 1)), "{seen:?}");
        assert!(seen.contains(&(EV_ABS, ABS_Z, 255)), "{seen:?}");
        assert!(seen.contains(&(EV_ABS, ABS_X, 32767)), "{seen:?}");
        assert!(seen.contains(&(EV_ABS, ABS_RX, 32767)), "{seen:?}");

        // The pad vanishes (Bluetooth drop-out): the virtual pad stays and rests.
        drop(fake);
        let seen = drain(&node, 5);
        assert!(seen.contains(&(EV_KEY, BTN_SOUTH, 0)), "{seen:?}");
        assert!(seen.contains(&(EV_ABS, ABS_X, 0)), "{seen:?}");
        // ...and is picked up again when it comes back.
        let fake = fake_pad("steam-tui test Xbox Wireless Controller");
        std::thread::sleep(Duration::from_millis(2500));
        drain(&node, 0);
        write_events(&fake, &[(EV_KEY, BTN_EAST, 1)]);
        assert!(drain(&node, 1).contains(&(EV_KEY, BTN_EAST, 1)));

        unsafe { libc::eventfd_write(stop.as_raw_fd(), 1) };
        thread.join().unwrap();
    }
}
