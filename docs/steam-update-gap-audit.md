# Steam updates, dependencies and shader pre-caching audit

Updated 2026-10-19 after implementing the update plan. Primary evidence is the original Linux `steamclient.so` in the existing headless Ghidra project `/home/vp/steam.gpr`, SHA-256 `6cd0294f99f9d202167c2cdf8add53fc81780997d4d4a80916d79146bb36aef1`. No Valve client or game was started during this work.

## Implementation status

| Original gap | Implemented behavior | Main sources |
|---|---|---|
| Proton/runtime dependency chain | Typed missing tools, recursive bounded resolution, cycle detection, own downloader and launch retry. | `launch/tools.rs`, `launch.rs` |
| Shared redistributables | Per-library shared depot state, ownership-aware planning and prerequisite setup per Proton prefix. | `catalog/shared.rs`, `launch/setup.rs` |
| Windows first-run prerequisites | Receipt/hash-verified scripts, registry values, process execution, requirements and has-run tracking within Proton. Unsupported directives fail explicitly before execution. | `launch/setup/` |
| Native first-run prerequisites | POSIX script versions 1/2, symlink/chmod, case-preserving v1 filenames, contained paths, full validation before mutation, versioned completion receipts. Linux `defaults` is a no-op in this binary. | `launch/setup/native.rs` |
| `additional_dependencies` | Reads **extended/additional_dependencies**, filters source/destination OS, validates the complete bounded graph, installs children before parents, retries launch. | `launch/tools.rs` |
| DLC state and selection | Actual ownership/selection/installed bytes agree with Apps APIs; persistent SDK requests and completion callbacks; content properties can enable/disable DLC using normal closed-game transactions. | `backend/apps.rs`, `dlc.rs`, `content_options.rs` |
| Update scheduling | Durable account-scoped queue, runner lease, atomic persistence, crash recovery, priority, automatic/launch-only/manual policies, cross-midnight windows, pause during games, explicit foreground override. No daemon. | `updates.rs`, `updates/store.rs` |
| Launch freshness | Normal TUI/CLI launch freshly checks the desired content and installs a confirmed update first. Unknown freshness is visible. `l` or CLI `--installed` explicitly bypasses that check. Pending launches are volatile and bound to the exact content job; shader completions cannot release them. Optional shader downloads permit launch and yield to a required pre-launch game update. Cancellation clears pending launches even if commit finishes. | `steam-tui/src/{main.rs,tui.rs}` |
| Branch/language controls | User-facing properties and CLI, validated selection, fresh planning, persisted desired choice without falsifying installed state. New installs retain public/English defaults. | `catalog.rs`, `catalog/metadata.rs`, `content_options.rs` |
| Protected branches | Check beta password, decode returned branch key, confirm private PICS access, merge private depot section only in memory. SecretService stores returned keys, never passwords. TUI F2 offers explicit session-only mode. RPC debug output redacts secrets. | `branches.rs`, `account/store.rs` |
| Obsolete managed files | Ordinary updates reconcile old depot ownership. Only unchanged old managed files absent from every selected depot are removed. Configuration, modified files, saves, mods and unmanaged content are preserved. Existing transaction journal covers removal/recovery. | `install.rs`, `install/transaction.rs` |
| Depot deltas | Authenticated optional patch lookup/download, bounded framing and identity checks, verified old source chunks, XOR and LZMA/Zstd dictionary reconstruction. Target chunk and whole-file hashes remain mandatory. Unsupported, oversized, corrupt, uneconomical or unavailable patches fall back to full chunks. | `content/delta.rs`, `content/formats.rs`, `install.rs` |
| Shader downloads | Independent SteamSwarm/G7 Vulkan FOZ v6 bucket lookup, full-width bucket IDs, CM-authorized app/depot mapping, TLS/chunk/file verification, resumable staging, atomic generations, persistent download jobs and separate status. | `shaders.rs`, `updates.rs` |
| Shader processing | Optional pre-launch standalone Fossilize replay, GPU/driver/game/tool/helper identity, stable driver cache paths, low priority, two threads, memory/time limits, elapsed progress, skip/kill/reap, pause while games run, successful completion receipts and safe pruning. | `shaders/{device.rs,replay.rs}`, `launch.rs` |
| Shader packaging | Pinned source build and standalone helper installation beside our client library, with third-party notices. No Valve client binary, installed Steam helper or capture layer is required. | `tools/build-fossilize.sh`, `install.sh` |

The dependency/shared-redist/DLC foundation was committed before this implementation pass (`be4eda0`, `8134370`, `5cd565c`). The new queue resolves fresh metadata and ownership at execution; saved jobs contain intents and freshness hints, not frozen authorization or credentials. The SDK's existing DLC worker retains its own durable selection state and shares the library transaction lock.

## Original-binary evidence

Ghidra has incomplete function bodies and incorrect non-return annotations on common helpers. Where decompilation stopped early, the corresponding raw disassembly of the same binary was inspected. Addresses here are Ghidra addresses; ELF addresses are lower by `0x100000`.

- `01086c10`: parent/child/depot relationships; `01197ac0`: compatibility-tool command resolution.
- `01085680` / `01080b60`: shared versus installed depot state; `01049370` / `01044f70`: disabled-DLC state.
- `011a0300`: `additional_dependencies` from appinfo section **3 (Extended)**. Config is section 4 elsewhere in the same path. Entry fields are `appid`, `src_os`, `dest_os`.
- `0139afe0`: POSIX install-script manager, version default 1, `symlink`, `chmod`, `defaults`. `0139ac10`: v1 link/target scalar pairs and v2 nested fields. `0139a940`: v1 filename/octal-mode pairs and v2 file/mode fields. `0139b062..0139b082` looks up `defaults` and returns without executing it.
- `021ac830`: delta magic `0x502f15e5`, protobuf length, `ContentDeltaChunks`, trailer `0xf3dea982`, embedded or appended data. `0136d690` / `0136d540`: decompressed XOR and dictionary methods. `02507730`, `025066b0`, `02507310`: VZd/VSZd dictionary streams.
- `016e4e80`, `016e8fa0`: independent shader scheduling and app/tool events. `016f2bd0`, `016f28d0`, `016f2000`: bucket contexts, manifest lookup and result IDs/size.
- `016fb260` is the shader job constructor. `016fb500` is setup; **`016fb970..016fba31` puts appid in both the source-app and depot fields and uses the shader manifest ID**. `011bbc60` / `011b9c00` / `01055d40` pass that identity into ordinary manifest/content reconstruction. The bucket ID is separate 64-bit metadata, not a depot ID.
- `016f8110`: pre-launch FOZ processing, whitelist/replay paths, replay helper, disabling Fossilize capture for processing. `016f7460`: completion. Original context lists include `SteamSwarm` / `G7:VulkanPipelinesV6` independently of legacy GPU-specific entries.

Private-beta message IDs and key semantics were cross-checked against primary [SteamKit handler sources](https://github.com/SteamRE/SteamKit/blob/master/SteamKit2/SteamKit2/Steam/Handlers/SteamApps/SteamApps.cs) and [generated message mappings](https://github.com/SteamRE/SteamKit/blob/master/SteamKit2/SteamKit2/Base/Generated/SteamLanguage.cs), because the installed proto package has private PICS structures but lacks their message-kind mapping. Replay flags and build options were checked against the pinned [official Fossilize source](https://github.com/ValveSoftware/Fossilize/tree/556da9ff7d7616704dda515756219ab1dab693ec).

## Controls and lifecycle

- **d** opens the downloads queue (content and shader jobs): pause/resume, retry, cancel, priority and app policy.
- **m** opens manage content: branch, language and DLC. Protected branches have masked password entry; F2 selects remembered returned key versus session-only access.
- **c while the selected game is launching** skips optional shader processing and continues launch. CLI launch/process uses Ctrl+C for this optional stage.
- `jobs list|add APPID|run|pause ID|resume ID|cancel ID` manages durable work. `jobs add` targets installed games; new installation remains available through the TUI.
- `updates show|policy APPID automatic|launch-only|manual|priority APPID N|window HH:MM HH:MM|window off|during-games pause|allow` manages scheduling. Times are local, with cross-midnight windows supported.
- `content show APPID|branch APPID NAME|language APPID NAME|dlc APPID DLCID enable|disable|unlock APPID BRANCH` manages desired content. CLI unlock prompts for a password and stores the returned key; session-only access is available in the TUI.
- `shaders show APPID|add APPID|process APPID|prune APPID|clear APPID|budget MIB|enabled on|off` manages optional Vulkan caches. Download jobs run through the ordinary queue; compilation runs on demand before launch or through the explicit process command.

Queue/settings and generated shader data are under `$XDG_STATE_HOME/steam-tui/updates-ACCOUNT` (default `~/.local/state`). The account directory is private. Shader archive budget defaults to **8 GiB**, including staging; reaching it reports a reclaim/raise-budget action instead of evicting another game's active cache. It excludes driver-created cache data. Individual archives are bounded to 8 GiB, chunks to 16 MiB, manifest parsing to the existing content limits. Retired staged generations, committed old generations and obsolete replay contexts are pruned. A running queue also reclaims recognized caches of uninstalled apps after scanning configured libraries, while respecting launch/cache locks and preserving unknown files.

Fossilize is built at `556da9ff7d7616704dda515756219ab1dab693ec` with pinned submodules. Replay uses two workers, low CPU priority, 256 MiB shader-object cache, 2 GiB address-space limit per process, and 30-minute overall limit. Skip kills its private process group and reaps it. Missing helper/Vulkan support, driver failure or a limit failure remains visible and allows launch. No processed marker is written on failure or skip. GPU/driver identity is a local headless Vulkan properties query; it is **not** a guessed Valve legacy bucket hash. Archive freshness and processed-context identity remain separate from game build freshness.

## Validation

- Workspace regression suite: **412 passed, 2 ignored** (steam-client 10, steam-core 284, steam-ipc 10, steam-tui 108). Nested client-isolation subprocess checks also passed.
- Queue tests cover restart, abandoned running jobs, exclusive runners, account isolation, policies/windows, game pauses, foreground override, priority, corrupt/symlink state, deduplication and cancellation revisions.
- Installer tests cover unchanged obsolete-file deletion with modified/config/save preservation and transaction recovery. Delta fixtures cover framing/truncation/identity failures, verified source changes, XOR, LZMA dictionaries, and an independent libzstd-generated dictionary sample.
- Native script tests cover both versions, mixed-case v1 filenames, idempotence, traversal, unsafe permissions and collisions. Additional-dependency tests cover OS filters, child-first ordering and cycles. Branch tests cover metadata isolation, key decoding and redacted RPC debug formatting.
- Shader tests cover separate queue identity, preferences, cache state/pruning/path refusal, archive budgets, GPU/driver/build/tool/helper invalidation, helper failure, and skip with kill/reap. Fake helpers are used; no desktop window or real driver compilation is part of unit tests.
- Live authenticated Rust-only probe for app `3124540`: bucket `44771565`, manifest `5886531712837077318`, **230,896,364 archive bytes**, 261 chunks. Original client was not run. An isolated `/tmp` production-downloader pass completed chunk and whole-file verification; a second pass verified/reused the committed archive and retained `processed=false`.
- The standalone pinned helper built successfully. A **null-device**, one-graphics-pipeline replay of that archive parsed two shader modules and one graphics pipeline successfully. This checks archive/helper compatibility without loading a Vulkan driver or creating a window; it is not a real GPU compilation/performance result.
- Release build and complete isolated-prefix installation passed, including the standalone helper and license files; no user installation was replaced. Clippy completed with existing repository warnings.

## Remaining compatibility and evidence limits

- Download/replay covers **GPU-independent Vulkan FOZ v6**. Valve's legacy GPU-specific Mesa/AMD/NVIDIA binary bucket formats and media-transcode caches are not implemented; their exact key algorithms/contexts remain unverified. Normal driver caches are still available. This implementation is not complete format parity with every Valve shader cache.
- Shader capture/upload is intentionally outside this download/replay implementation. No automatic recording or upload is enabled.
- A real GPU/driver replay and its stutter/performance benefit have not been measured. The helper may reject unsupported pipelines or exceed configured limits; this is optional work and never fabricates success.
- Private-beta code is covered by fixtures and primary protocol evidence, but no live password-protected branch was exercised. Manifest request codes use the authorized private PICS context plus branch name; servers needing additional unsupported authorization fail explicitly.
- Windows prerequisite support remains a scoped registry/process/requirements/has-run subset. Unsupported directives fail explicitly; arbitrary Windows setup semantics are not claimed. Native POSIX defaults is a verified no-op for this Linux binary, not an inferred implementation.
- Delta patches are optional and conservative: small missing groups can prefer full chunks even when a depot-wide patch would help. Unsupported methods/containers use verified ordinary chunks. No live delta optimization benchmark was run.
- Manifest signature sections are structurally validated, not cryptographically verified; current trust remains CM-authorized identities, CDN TLS, filename/chunk/file integrity. This existing transport boundary is unchanged.
