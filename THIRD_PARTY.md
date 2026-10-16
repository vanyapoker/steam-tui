# Third-party code

steam-tui itself is MIT licensed (see `LICENSE`). It includes or builds the following.

## Vendored in this repository

| Path | License | Notes |
| --- | --- | --- |
| `vendor/steam-vent` (with `core/`, `crypto/`) | MIT | Copyright (c) Robin Appelman. Upstream: https://codeberg.org/steam-vent/steam-vent, revision in `vendor/steam-vent/UPSTREAM.md`. The crate manifests declare `license = "MIT"`; upstream ships no separate license file at that revision. Local changes are listed in `UPSTREAM.md`. |
| `vendor/GameNetworkingSockets` | BSD-3-Clause | Copyright (c) 2018, Valve Corporation. The full text is in `vendor/GameNetworkingSockets/LICENSE`. Its own bundled third-party sources keep their notices in `vendor/GameNetworkingSockets/src/external`. |

## Built from source at compile time

`crates/steam-client/build.rs` builds and statically links OpenSSL (Apache-2.0) and protobuf (BSD-3-Clause) through the `openssl-src` and `protobuf-src` crates.

## Cargo dependencies

Registry dependencies are under permissive licenses (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0 and similar), with one MPL-2.0 crate (`option-ext`, file-level copyleft; unmodified). Run `cargo metadata` for the full list.

## Not included

No Steamworks SDK headers or Valve binaries are distributed. The optional C++ ABI test (`crates/steam-client/tests/abi.cpp`) needs an SDK you download yourself under Valve's terms.

Steam and VAC are trademarks of Valve Corporation. This project is not affiliated with or endorsed by Valve.
