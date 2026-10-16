# Upstream source provenance

- Canonical repository: https://codeberg.org/steam-vent/steam-vent
- Exact revision: `b0e40109dd7a1b5101e1546ed27f62663dbe5c4b`
- Package version: `steam-vent` 0.6.0
- Imported: 2026-10-14
- Pristine import commit: `07997bd`

The import is the complete tracked source tree from `git archive` at that
revision, without its Git metadata. No upstream source or manifest adjustments
were made in the pristine import. Local changes below were applied afterward.
This provenance file was added after the pristine import.
The sibling `core/` and `crypto/` source crates retain their original paths.
The application workspace excludes all three vendored packages from membership;
Cargo resolves them as local path dependencies. Protobuf crates remain registry
dependencies. The root `Cargo.lock` governs application builds; upstream lockfiles
are preserved unchanged as part of the import.

## Local changes

- `src/session.rs`: anonymous client/server sessions no longer require a refresh
  token. Authenticated sessions still require valid tokens. Session storage and
  `Connection::refresh_token()` now use `Option<RefreshToken>` and
  `Option<&RefreshToken>` respectively. Regression tests cover absent anonymous
  tokens, missing authenticated tokens, valid tokens, and malformed tokens.
- `src/connection/mod.rs`, `src/connection/filter.rs`: multipart response
  subscriptions use RAII cleanup on completion, error, and cancellation.
  Message routing releases its DashMap guard before awaiting channel send.
  A regression test checks cancellation cleanup.
- `src/net.rs`, `src/connection/mod.rs`, `src/connection/unauthenticated.rs`:
  broadcast subscription overflow now yields `NetworkError::Lagged(count)`
  instead of `EOF`, preserving the distinction from actual stream closure.
  Regression tests check overflow counts and closed-stream behavior. The
  application can distinguish lost updates from a lost network connection and
  require a fresh social session when live persona state cannot be reconstructed.
- `src/connection/filter.rs`, `src/connection/mod.rs`: `take_unprocessed_kind`
  drains only one message kind from the 32-message startup buffer, preserving other
  services' pushes and order. Synchronous locks use `parking_lot`. A regression test
  covers selective draining.
- `examples/refresh_token.rs`: migrated the optional-token API and removed raw
  token printing. Authentication API documentation and `CHANGELOG.md` were
  updated. Upstream examples are references, not the application's secure login
  interface.

- `crypto/src/lib.rs`: adds the Steam Cloud variant of SymmetricEncrypt, with a
  full random IV rather than the session-message HMAC IV. It reuses the existing
  AES-ECB IV / AES-CBC-PKCS7 implementation. Ghidra cloud-path evidence and an
  independent OpenSSL test vector are recorded in `docs/save-sync-review.md`.


Upstream declares MIT in the root and crypto package manifests. No standalone
LICENSE or COPYING file exists in this revision. All supplied source headers,
credit notices, and third-party protobuf notices are preserved. Resolve the
missing standalone license text before redistributing releases.

The upstream README does not welcome LLM-driven contributions. This is a local
source import, not an upstream contribution.
