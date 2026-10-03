# Codex daemon bridge (fork feature)

Design investigation and implementation notes for live Codex account switching
without restarting the app-server daemon. Sources: `openai/codex` tag
`rust-v0.160.0` (inspect a fresh clone when adapting to newer releases).

## Why file-based switching cannot reach a running daemon

- `AuthManager` (codex-rs/login/src/auth/manager.rs) loads `auth.json` once at
  process start and caches it: "External modifications to `auth.json` will NOT
  be observed until `reload()` is called explicitly." There is no file watcher
  and no RPC that triggers `reload()` on demand.
- The only implicit reload is the 401 recovery path, and
  `reload_if_account_id_matches` returns `ReloadOutcome::Skipped` when the
  on-disk `account_id` differs — token refresh of the *same* account is picked
  up, an account *switch* never is.
- `account/logout` revokes tokens server-side and deletes `auth.json`; it does
  not make the daemon re-read a rewritten file. SIGHUP/SIGTERM are shutdown.

## The mechanism: external auth via chatgptAuthTokens

- Control socket: `<CODEX_HOME>/app-server-control/app-server-control.sock`
  (symlink). Speaks WebSocket, one JSON-RPC message per text frame.
- Handshake: `initialize` with `capabilities.experimentalApi = true`, then the
  `initialized` notification (no `params`).
- Push/switch: `account/login/start` with
  `{type: "chatgptAuthTokens", accessToken, chatgptAccountId, chatgptPlanType}`
  installs process-wide external auth in memory (`AuthManager::set_external_auth`);
  `auth.json` is not touched. A second push replaces it — that is the switch.
  Marked `[UNSTABLE] FOR OPENAI INTERNAL USE ONLY`; re-verify the
  app-server-protocol diff on every codex upgrade.
- Refresh: on 401 the daemon broadcasts `account/chatgptAuthTokens/refresh`
  (10s timeout, `external_auth.rs`). Only a client holding refresh tokens can
  answer; the codex TUI explicitly ignores this request. There is no fallback
  to `auth.json` while external auth is installed.
- Socket close == daemon death/restart; external auth is memory-only, so the
  bridge re-pushes after every reconnect.

## Failure semantics (verified)

- All bridge/refresh errors classify as `RefreshTokenError::Transient` by
  default: they are never recorded as permanent, every new request retries
  recovery from scratch. A dead bridge fails *new* turns with an auth error;
  running SSE streams are unaffected; threads/sessions persist and self-heal
  once the bridge returns. No revoke/logout occurs.
- Daemon restart without a synced `auth.json` silently falls back to whatever
  account `auth.json` holds. Therefore every push must be paired with the
  file-based publication (`auth use` already does this); `auth push` is the
  manual recovery tool and warns on mismatch.

## Decisions

- Push only together with auth.json sync (`auth use`) or explicitly
  (`auth push`). Never push on `auth default`: with proxy takeover enabled the
  daemon talks through the cc-switch proxy and external auth must not be
  installed; without takeover, default-only push would desync daemon memory
  from auth.json (silent wrong account after restart).
- Refresh answers always carry the currently pushed account, even when
  `previousAccountId` differs — that is the switch semantics.
- Proxy route (takeover) remains the zero-restart alternative for setups where
  the experimental API is unacceptable; it requires cc-switch alive per request
  and covers only the proxy's HTTP routes.

## Open risks

- Experimental protocol drift on codex upgrades (mitigate: fallback to
  file-based activation when `chatgptAuthTokens` is rejected).
- One account per daemon process (same as auth.json today).
- Concurrent VS Code extension with its own external auth could race refresh
  answers (broadcast semantics).
