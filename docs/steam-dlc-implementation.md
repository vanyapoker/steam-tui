# DLC implementation and validation

Ownership was committed as `8134370`; this follow-up completes the DLC state APIs
and adds optional-content jobs to the built-in account backend.

## Implemented behavior

- Parent-scoped DLC enumeration from PICS, including store availability separately
  from ownership. Names and metadata use our own cache and CM requests.
- `GetDLCCount`, `BGetDLCDataByIndex`, `BIsDlcInstalled`, `GetInstalledDepots`,
  `GetDlcDownloadProgress`, and the installed build/branch getters have actual
  signatures, bounded output writes, and backend state.
- Installed DLC requires applicable committed depots, an enabled selection, and
  authenticated ownership. Explicit contentless DLC and fully described DLC
  bundled with the parent are handled separately from missing metadata. Other
  platforms, architectures, and languages do not become missing current depots.
- Enabled choices and pending operations persist per account, parent, and library.
  Failed or cancelled jobs retain intent for retry after restart or another
  explicit request. Jobs resume when the next backend starts serving Apps calls.
- `InstallDLC` and `UninstallDLC` queue background jobs. Install requests verify
  ownership before acceptance and again during work/completion. Uninstalling
  locally installed DLC does not require a current DLC entitlement.
- Live transactions retain the parent build, branch, platform, language, and
  every unrelated depot manifest. Changed paths must belong to the requested
  DLC or restore content it overlaid. Base files outside that scope are not
  restaged or replaced. Ordinary updates still require the game to be closed.
- Overlapping base/other-DLC files are restored from the remaining depot overlay.
  Only unchanged, exclusively removed DLC files are deleted. Modified files,
  configuration, and saves are retained. Removal renames and recovery use the
  same durable journal as installation.
- Private decoded depot-manifest receipts are bound to installed metadata by
  depot ID, manifest ID, source app, and hash. They contain no depot keys or
  credentials and permit classification of previously installed content after
  license revocation. Content chunks still use CM-authorized transport.
- DLC-specific shared redistributable payloads are installed through our own
  installer. Disabled DLC is excluded from normal update plans and prerequisite
  execution. Normal updates can reconcile removed DLC files using the same
  verified classification rules.
- `DlcInstalled_t` reaches the existing callback queue only after committed
  content, enabled state, and confirmed ownership agree. Queued/preparing jobs
  do not claim download progress; uninstall requests do not fabricate a callback.

## Evidence

The static reference remains the original Linux `steamclient.so` in
`/home/vp/steam.gpr`, SHA-256
`6cd0294f99f9d202167c2cdf8add53fc81780997d4d4a80916d79146bb36aef1`.
Ghidra establishes per-app `DisabledDLC` persistence (`01049370`, `01044f70`),
depot-based installed checks (`01051f90`), and the `NoContentDLC`/`AppManagesDLC`
selection branches (`0104fe55`, `0104fd30`). Enumeration instructions read
`DLCAvailableOnStore` near `01027878`; that field is not an entitlement.

The method signatures and completion payload agree with
[Valve's SDK header in Proton](https://github.com/ValveSoftware/Proton/blob/proton_10.0/lsteamclient/steamworks_sdk_165/isteamapps.h)
and [ISteamApps documentation](https://partner.steamgames.com/doc/api/ISteamApps).
`DlcInstalled_t` is callback 1005 with one 32-bit AppId; no matching uninstall
callback is invented. Incomplete Ghidra function tails are not treated as a
complete reconstruction of Valve's state machine.

## Validation and limits

Tests exercise SDK calls through an isolated process and mock Unix-socket
backend: owned/unowned/absent content, UTF-8 and buffer bounds, invalid indices,
download counters, build/branch queries, mutation invalidation, and failures.
Temporary-library tests cover actual staged commits, overlay restoration,
unchanged base inodes, retained mods/configuration/saves, receipt tampering,
selection persistence/account isolation, process guards, removal recovery,
disabled-DLC update plans, and callback eligibility/layout.

No Valve client, game window, paid-DLC transaction, or live prerequisite process
was launched for validation. Live CM/CDN behavior and a game's reaction to its
completion callback remain integration-validation items.

Legacy installations without our manifest receipts may still need authorized
manifest access to classify old files. Missing or ambiguous metadata fails
explicitly rather than guessing which files to delete. Installed-build changes
require restarting the game/backend. Windows setup scripts discovered in newly
installed DLC run during the next launch's existing verified setup phase.
`SetDlcContext` usage reporting remains unavailable and returns false; it no
longer claims success without sending usage state.
