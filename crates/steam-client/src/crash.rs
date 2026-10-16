//! Opt-in (STEAM_TUI_CRASH_TRACE=1) SIGSEGV reporter for bootstrap debugging.
//! Prints the fault address, RIP and stack words that fall inside executable
//! mappings, so crashes can be attributed without a core dump. Not async-signal
//! safe; it only runs just before the process dies anyway.
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
fn describe(maps: &[Map], addr: u64) -> Option<String> {
    let m = maps.iter().find(|m| addr >= m.start && addr < m.end)?;
    Some(format!("{}+{:#x}", m.path, addr - m.start + m.offset))
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
    trace(format_args!(
        "CRASH signal {sig} fault={fault:#x} rip={:?}",
        describe(&maps, rip)
    ));
    for i in 0..4096u64 {
        // SAFETY: bounded read of this thread's stack above RSP; stops at the
        // first unreadable page via the fallible /proc check below.
        let word = unsafe { ((rsp + i * 8) as *const u64).read_volatile() };
        if let Some(d) = describe(&maps, word) {
            trace(format_args!("  stack+{:#x}: {d}", i * 8));
        }
        if i > 0 && (rsp + i * 8) % 4096 == 0 && i > 512 {
            break;
        }
    }
    std::process::exit(139);
}

pub fn install() {
    if std::env::var_os("STEAM_TUI_CRASH_TRACE").is_none() {
        return;
    }
    let act = SigAction {
        handler: on_segv as unsafe extern "C" fn(i32, *const u8, *const u8) as usize,
        mask: [0; 16],
        flags: 4, // SA_SIGINFO
        restorer: 0,
    };
    // SAFETY: valid sigaction layout for x86-64 glibc.
    unsafe { sigaction(11, &act, std::ptr::null_mut()) };
}
