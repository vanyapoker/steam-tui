# Virtual gamepad

Games started on the built-in backend get one stable virtual Xbox 360 pad instead of the raw
controller (`crates/steam-core/src/gamepad/`). Input is forwarded unchanged; there is no remapping.

## What Valve's client does (verified in its launch-environment builder and in SDL/Wine sources)

- Steam Input grabs the physical pad and creates a pad through `/dev/uinput` with USB id
  `28de:11ff`, named `Microsoft X-Box 360 pad N` (N is the slot). SDL (`SDL_sysjoystick.c`) takes the
  slot from the digits after `pad ` in the evdev name; Wine's udev backend (`bus_udev.c`) keeps the
  device, forces its version to 0 and parses `Microsoft X-Box 360 pad %u`. Wine's SDL backend ignores
  `28de:11ff` so the pad is not seen twice.
- The game's environment gets `SDL_GAMECONTROLLER_IGNORE_DEVICES=0xVVVV/0xPPPP,...` (the physical
  pads; both Wine backends honour it), `SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD`,
  `SDL_JOYSTICK_HIDAPI_STEAMXBOX=0` and `SteamVirtualGamepadInfo` (`SteamVirtualGamepadInfo_Proton`
  for Proton, which copies it over). Valve's file is `<steam>/config/virtualgamepadinfo.txt`.
- The info file (SDL `SDL_steam_virtual_gamepad.c`) is `[slot N]` sections with `name=`, `VID=`,
  `PID=`, `type=` (an SDL controller type such as `xbox360`) and `handle=`. Proton's winebus clears the
  hint for itself (it needs the evdev name to find the slot), so the file only matters to native SDL
  games.

## What steam-tui does

- A connected Xbox-compatible pad (Microsoft vendor id or an "Xbox" name; xpad layout or the HID
  layout used over Bluetooth, where the right stick is Z/RZ and the triggers are Brake/Gas), DualSense
  (054c:0ce6, 0df2; xpad axes) or DualShock 4 (054c:05c4, 09cc, 0ba0; right stick Z/RZ, triggers RX/RY as
  hid-sony reports them) is grabbed (`EVIOCGRAB`) and mirrored onto `Microsoft X-Box 360 pad 0`: sticks scaled to ±32767,
  triggers to 0-255, hat, buttons by their xpad codes. Rumble uploaded to the virtual pad is forwarded
  to the physical one.
- If the physical pad drops out (Bluetooth), the virtual pad stays and rests; when a pad comes back
  (rescan every second) it is attached again. The game never sees a disconnect.
- The game's environment gets the variables above (existing launch options win). The info file is
  written to `<steam>/config/steam-tui-virtualgamepad-<app>.txt` and removed when the game ends.
- Sony pads are shown to games as the same virtual Xbox 360 pad (what XInput games need); the info
  file and Steam Input report the real type (`ps4`/`ps5`). Touchpad, motion sensor and lightbar are not
  forwarded (their evdev nodes have no face buttons and are never picked).
- Steam Input presence: the game's environment carries `STEAM_TUI_VIRTUAL_GAMEPAD=<xbox360|xboxone|ps4|ps5>`.
  With it, `ISteamInput` (Init succeeds) and `ISteamController` report one connected device (handle 1, gamepad
  slot 0, type 2/3/5/13), `EnableDeviceCallbacks` queues `SteamInputDeviceConnected_t` (2801) and
  `SteamInputGamepadSlotChange_t` (2804), and `GetControllerForGamepadIndex(0)`/`GetGamepadIndexForController`
  agree. Action data and motion data are always inactive/zero. Without a virtual pad `ISteamInput::Init` still
  fails, so games keep using their plain gamepad path. `STEAM_TUI_NO_GAMEPAD_PRESENCE=1` turns the presence
  reporting off while keeping the virtual pad.
- `STEAM_TUI_NO_VIRTUAL_GAMEPAD=1` disables all of it. Without `/dev/uinput` access, without a
  supported pad, or on Valve-host launches, nothing changes: the game sees the raw device.

## Limits

- One pad, picked at launch; a pad of another model connected later is not hidden or mirrored.
  Pads outside the Xbox family (DualShock/DualSense, Switch) are left to Wine as before.
- Games that rely on Steam Input actions (not XInput/SDL) get a connected device but no input from it:
  action sets, bindings and remapping are not implemented. If such a game misbehaves, set
  `STEAM_TUI_NO_GAMEPAD_PRESENCE=1`.
- Pads of other families (Switch, generic DirectInput) are left to Wine as before.
- Checked against a fake Bluetooth-layout pad on `/dev/uinput`
  (`cargo test -p steam-core gamepad -- --ignored`: forwarding, drop-out, re-attach). Rumble and
  a real game session are not covered yet.
