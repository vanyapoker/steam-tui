#!/usr/bin/env python3
"""Time steam-tui startup in a pty (no display needed, nothing opens on the desktop).

Usage: scripts/bench-startup.py [--runs N] [--offline] [--account NAME] [--binary PATH]

Milestones come from the app itself (STEAM_TUI_STARTUP_TRACE): first_frame, signed_in,
catalog_interim, catalog_live, friends_loaded. "first byte" is the first pty output.
A run is usable once the library is populated (catalog_interim or catalog_live) and, when
online, friends are loaded; it is stopped with `q` right after.
"""
import argparse, fcntl, os, pty, select, statistics, struct, tempfile, termios, time

def run_once(binary, args, usable, timeout):
    with tempfile.TemporaryDirectory() as tmp:
        trace = os.path.join(tmp, "trace")
        env = dict(os.environ, TERM="xterm-256color", STEAM_TUI_STARTUP_TRACE=trace)
        started = time.monotonic()
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(binary, [binary, *args], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))
        first_byte = None
        while time.monotonic() - started < timeout:
            ready, _, _ = select.select([fd], [], [], 0.02)
            if ready:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    break
                if data and first_byte is None:
                    first_byte = (time.monotonic() - started) * 1000
            seen = events(trace)
            if usable <= seen.keys():
                break
        seen = events(trace)
        try:
            os.write(fd, b"q")
            time.sleep(0.3)
            os.close(fd)
        except OSError:
            pass
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
        seen["first_byte"] = first_byte
        return seen

def events(path):
    out = {}
    try:
        for line in open(path):
            ms, name = line.split()
            out.setdefault(name, float(ms))
    except FileNotFoundError:
        pass
    return out

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--account", help="account name (default: the remembered one)")
    parser.add_argument("--binary", default="./target/release/steam-tui")
    parser.add_argument("--timeout", type=float, default=30)
    options = parser.parse_args()
    if options.offline:
        args, usable = ["tui", "--offline"], {"catalog_live"}
    else:
        args = ["--account", options.account] if options.account else []
        usable = {"catalog_interim", "friends_loaded"}
    runs = [run_once(options.binary, args, usable, options.timeout) for _ in range(options.runs)]
    names = ["first_byte", "first_frame", "signed_in", "catalog_interim", "catalog_live", "friends_loaded"]
    print(f"{'milestone':<16} {'runs':>4} {'median ms':>10} {'min':>8} {'max':>8}")
    for name in names:
        values = [r[name] for r in runs if r.get(name) is not None]
        if values:
            print(f"{name:<16} {len(values):>4} {statistics.median(values):>10.0f} {min(values):>8.0f} {max(values):>8.0f}")
        else:
            print(f"{name:<16} {0:>4} {'-':>10}")

main()
