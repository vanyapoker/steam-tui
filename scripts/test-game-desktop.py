#!/usr/bin/env python3
"""Offline game workspace integration test: fake compositor, no windows/games."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/release/steam-tui').resolve(strict=True))
with tempfile.TemporaryDirectory(prefix='steam-tui-desktop-') as directory:
    runtime = Path(directory)
    endpoint = runtime / 'hypr' / 'fixture' / '.socket.sock'
    endpoint.parent.mkdir(parents=True)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(endpoint))
    listener.listen()
    listener.settimeout(.1)
    token = 'offline-desktop-fixture'
    game = subprocess.Popen(['/usr/bin/sleep', '30'], env={**os.environ, 'STEAM_TUI_LAUNCH_TOKEN': token})
    helper = None
    stop = threading.Event()
    commands = []
    errors = []
    stage = [0]
    unrelated = {'address': '0xdead', 'pid': os.getpid(), 'mapped': True, 'hidden': False}
    owned = {'address': '0xab1', 'pid': game.pid, 'mapped': True, 'hidden': False}
    replacement = {**owned, 'address': '0xab2'}
    def serve():
        try:
            while not stop.is_set():
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                with connection:
                    command = connection.recv(65536).decode()
                    if command == 'j/clients':
                        clients = [unrelated, owned, {'address': '0xbad', 'pid': -1, 'mapped': True}, {'address': '0xbad2', 'mapped': True}] + ([replacement] if stage[0] else [])
                        connection.sendall(json.dumps(clients).encode())
                    else:
                        commands.append(command)
                        connection.sendall(b'ok')
        except Exception as error:
            errors.append(error)
    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    def await_count(count):
        limit = time.monotonic() + 6
        while len(commands) < count and time.monotonic() < limit:
            time.sleep(.05)
        assert len(commands) == count, (count, commands, errors)
    try:
        start = Path(f'/proc/{game.pid}/stat').read_text().rsplit(')', 1)[1].split()[19]
        helper = subprocess.Popen([binary, '--internal-game-desktop', str(game.pid), start, token], env={**os.environ, 'XDG_RUNTIME_DIR': directory, 'HYPRLAND_INSTANCE_SIGNATURE': 'fixture'}, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        await_count(1)
        assert 'address:0xab1' in commands[0] and '0xdead' not in commands[0]
        stage[0] = 1
        await_count(2)
        assert 'address:0xab2' in commands[1]
        time.sleep(.7)
        assert len(commands) == 2, 'existing windows must not be repeatedly forced fullscreen'
        # Execute the actual generated Lua against a model compositor. Includes
        # empty active workspaces, parallel launches, unrelated occupants and PID reuse.
        setup = f'''
local pid={game.pid}
local windows={{["address:0xab1"]={{pid=pid,mapped=true,hidden=false}},["address:0xab2"]={{pid=pid,mapped=true,hidden=false}}}}
local workspaces={{{{id=1}},{{id=2}},{{id=4}}}}
local occupants={{}}
local actions={{}}
hl={{dsp={{window={{}}}}}}
function hl.get_window(selector) return windows[selector] end
function hl.get_workspaces() return workspaces end
function hl.get_workspace_windows(id) return occupants[id] or {{}} end
function hl.dsp.window.move(args) args.kind="move"; return args end
function hl.dsp.window.fullscreen(args) args.kind="fullscreen"; return args end
function hl.dispatch(args)
  table.insert(actions,args)
  assert(windows[args.window],"must target game window")
  if args.kind=="move" then
    table.insert(workspaces,{{id=tonumber(args.workspace)}})
    occupants[args.workspace]={{{{pid=pid}}}}
  else assert(args.mode=="fullscreen" and args.action=="set") end
end
'''
        first, second = [c.removeprefix('eval ') for c in commands]
        lua = setup + '\n' + first + '\n' + second + f'''
assert(#actions==4 and actions[1].workspace=="3" and actions[3].workspace=="3")
'''
        # Competing launch must get another unused workspace.
        lua += first.replace(token, 'parallel-fixture') + '\nassert(actions[5].workspace=="5")\n'
        # A workspace that acquired an unrelated window must no longer be reused.
        lua += 'occupants["3"]={{pid=99999999}}\n' + first + '\nassert(actions[7].workspace=="6")\n'
        # Window address reuse with a different process must cause no action.
        lua += 'windows["address:0xab1"].pid=99999999\n' + 'do\n' + first + '\nend\nassert(#actions==8)\n'
        result = subprocess.run(['lua', '-'], input=lua, text=True, capture_output=True, timeout=5)
        assert result.returncode == 0, result.stderr
        game.terminate()
        game.wait(timeout=5)
        stdout, stderr = helper.communicate(timeout=6)
        assert helper.returncode == 0, stderr.decode()
        assert len(commands) == 3 and '["offline-desktop-fixture"]=nil' in commands[-1], commands
        assert not errors, errors
    finally:
        if game.poll() is None:
            game.terminate(); game.wait(timeout=5)
        if helper is not None and helper.poll() is None:
            helper.terminate(); helper.wait(timeout=5)
        stop.set()
        thread.join(timeout=2)
        listener.close()
print('PASS: exact launch ownership, new/replacement windows, fullscreen, empty workspace allocation, simultaneous launches, PID reuse, one-time placement and helper cleanup; no windows opened')
