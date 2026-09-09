# RainyPixel local build

This branch adds distribution metadata to the upstream account-activation patch.
The upstream PR branch is `feat/codex-account-use`; local installation uses
`build/rainypixel-codex-accounts` and version `5.10.4+rainypixel.1`.

Build with the pinned toolchain from `src-tauri`:

```sh
cargo build --release --locked
```

Run `cc-switch auth list`, then `cc-switch auth use <account-id>` and start a new
Codex process. In the TUI use Settings -> Managed accounts -> **u / Use in Codex**.

This source build does not have a signed binary release channel. Self-update is
blocked so upstream releases cannot silently remove the local functionality.
Update by reviewing/rebuilding this branch, or return to upstream once the PR is
released. `update --check` reports the installed local version.

On the original installation the previous executable is retained under
`~/.local/share/cc-switch-local/backups/`; the installation record in that directory's
parent records source commit and SHA-256. Replacing `~/.local/bin/cc-switch` with
that saved executable rolls back the installation without deleting account data.
