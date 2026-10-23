//! Opt-in (STEAM_TUI_CRASH_TRACE=1) SIGSEGV reporter for bootstrap debugging.
//! Prints the fault address, RIP and stack words that fall inside executable
//! mappings, so crashes can be attributed without a core dump. Not async-signal
//! safe; it only runs just before the process dies anyway.
//!
//! Under Wine, faults in Windows code are Wine's to turn into exceptions, and
//! faults on mapped memory may be Wine's own write watches or guard pages even
//! inside Linux code. The first few faults with RIP in a Linux shared object
//! are reported; every fault then goes to the handler installed before
//! (Wine's), which decides whether the process survives.
use super::trace;

#[repr(C)]
struct SigAction {
    handler: usize,
    mask: [u64; 16],
    flags: i32,
    restorer: usize,
}
unsafe extern "C" {
    fn sigaction(sig: i32, act: *const SigAction, old: *mut SigAction) -> i32;
}

struct Map {
    start: u64,
    end: u64,
    offset: u64,
    path: String,
}
fn maps() -> Vec<Map> {
    let text = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let (range, perms, offset) = (it.next()?, it.next()?, it.next()?);
            if !perms.contains('x') {
                return None;
            }
            let (a, b) = range.split_once('-')?;
            // Paths may contain spaces: take the rest of the line after the inode.
            let path = line
                .splitn(6, char::is_whitespace)
                .nth(5)
                .map_or("?", str::trim)
                .to_string();
            Some(Map {
                start: u64::from_str_radix(a, 16).ok()?,
                end: u64::from_str_radix(b, 16).ok()?,
                offset: u64::from_str_radix(offset, 16).ok()?,
                path,
            })
        })
        .collect()
}
/// Whether `addr` lies in any mapping, executable or not.
fn mapped(addr: u64) -> bool {
    let text = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    text.lines().any(|line| {
        line.split_whitespace()
            .next()
            .and_then(|range| range.split_once('-'))
            .and_then(|(a, b)| {
                Some((
                    u64::from_str_radix(a, 16).ok()?,
                    u64::from_str_radix(b, 16).ok()?,
                ))
            })
            .is_some_and(|(start, end)| addr >= start && addr < end)
    })
}
fn describe(maps: &[Map], addr: u64) -> Option<String> {
    let m = maps.iter().find(|m| addr >= m.start && addr < m.end)?;
    Some(format!("{}+{:#x}", m.path, addr - m.start + m.offset))
}

const SA_SIGINFO: i32 = 4;
static mut PREVIOUS: SigAction = SigAction {
    handler: 0,
    mask: [0; 16],
    flags: 0,
    restorer: 0,
};

/// Hands the fault to the handler installed before ours.
unsafe fn chain(sig: i32, info: *const u8, ctx: *const u8) {
    // SAFETY: written once in `install` before this handler can run.
    let previous = unsafe { &*std::ptr::addr_of!(PREVIOUS) };
    match previous.handler {
        // SIG_DFL / SIG_IGN: restore it and return; the fault repeats and
        // the default action applies.
        0 | 1 => unsafe {
            sigaction(sig, previous, std::ptr::null_mut());
        },
        handler if previous.flags & SA_SIGINFO != 0 => unsafe {
            std::mem::transmute::<usize, unsafe extern "C" fn(i32, *const u8, *const u8)>(handler)(
                sig, info, ctx,
            )
        },
        handler => unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(i32)>(handler)(sig) },
    }
}

unsafe extern "C" fn on_segv(sig: i32, info: *const u8, ctx: *const u8) {
    // SAFETY: SA_SIGINFO delivers valid siginfo_t and ucontext_t on x86-64
    // glibc: si_addr at +16, gregs at +40 (RSP=15, RIP=16).
    let (fault, regs) = unsafe {
        (
            info.add(16).cast::<u64>().read_unaligned(),
            ctx.add(40).cast::<u64>(),
        )
    };
    let (rsp, rip) = unsafe { (regs.add(15).read_unaligned(), regs.add(16).read_unaligned()) };
    let maps = maps();
    let linux = maps
        .iter()
        .find(|m| rip >= m.start && rip < m.end)
        .is_some_and(|m| m.path.contains(".so"));
    // Report a few faults in Linux code, then always let Wine decide: faults
    // on mapped memory may be its own write watches or guard pages.
    static REPORTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if linux && REPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
        trace(format_args!(
            "CRASH? signal {sig} fault={fault:#x} ({}) rip={:?}",
            if mapped(fault) { "mapped" } else { "unmapped" },
            describe(&maps, rip)
        ));
        // Only read stack words inside the stack's own mapping.
        let top = stack_end(rsp).unwrap_or(rsp);
        let mut word_at = rsp;
        let mut shown = 0;
        while word_at + 8 <= top && word_at < rsp + 64 * 1024 && shown < 48 {
            // SAFETY: word_at lies within the mapping that contains RSP.
            let word = unsafe { (word_at as *const u64).read_volatile() };
            if let Some(d) = describe(&maps, word) {
                trace(format_args!("  stack+{:#x}: {d}", word_at - rsp));
                shown += 1;
            }
            word_at += 8;
        }
    }
    unsafe { chain(sig, info, ctx) };
}

/// End of the mapping that contains `addr`.
fn stack_end(addr: u64) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/maps").ok()?;
    text.lines().find_map(|line| {
        let (a, b) = line.split_whitespace().next()?.split_once('-')?;
        let (start, end) = (
            u64::from_str_radix(a, 16).ok()?,
            u64::from_str_radix(b, 16).ok()?,
        );
        (addr >= start && addr < end).then_some(end)
    })
}

pub fn install() {
    if std::env::var_os("STEAM_TUI_CRASH_TRACE").is_none() {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        trace("crash trace enabled");
        // SAFETY: valid sigaction layout for x86-64 glibc; PREVIOUS is only
        // written here, before our handler is installed.
        unsafe {
            let previous = &mut *std::ptr::addr_of_mut!(PREVIOUS);
            sigaction(11, std::ptr::null(), previous);
            // Keep the previous handler's flags (Wine needs its alternate
            // signal stack) and mask; add SA_SIGINFO for our signature.
            let act = SigAction {
                handler: on_segv as unsafe extern "C" fn(i32, *const u8, *const u8) as usize,
                mask: previous.mask,
                flags: previous.flags | SA_SIGINFO,
                restorer: previous.restorer,
            };
            sigaction(11, &act, std::ptr::null_mut());
        }
    });
}
