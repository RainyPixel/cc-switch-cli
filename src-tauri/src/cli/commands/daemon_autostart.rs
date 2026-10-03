//! `cc-switch daemon autostart` — first-class daemon autostart management.
//!
//! Linux is handled with a systemd *user* unit; other platforms return a
//! structured "unsupported" error so launchd can slot in later. All
//! `systemctl` invocations go through [`SystemctlRunner`] so tests inject a
//! fake and never touch a real system.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Subcommand;

use crate::cli::ui::{highlight, info, success, warning};
use crate::error::AppError;

pub(crate) const UNIT_NAME: &str = "cc-switch-daemon.service";
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Subcommand, Debug, Clone)]
pub enum AutostartCommand {
    /// Install and start autostart for the cc-switch daemon
    Enable,
    /// Stop and remove the autostart unit (idempotent)
    Disable,
    /// Report whether autostart is installed/enabled and currently active
    Status,
}

/// Platform support for daemon autostart. New mechanisms (e.g. launchd)
/// extend this enum instead of scattering `cfg` across the flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutostartPlatform {
    /// systemd user units (Linux).
    SystemdUser,
    /// Autostart is not implemented for this platform yet.
    Unsupported,
}

impl AutostartPlatform {
    fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::SystemdUser
        } else {
            Self::Unsupported
        }
    }
}

pub fn execute(cmd: AutostartCommand) -> Result<(), AppError> {
    execute_with(&cmd, AutostartPlatform::current(), &RealSystemctl)
}

pub(crate) fn execute_with(
    cmd: &AutostartCommand,
    platform: AutostartPlatform,
    runner: &dyn SystemctlRunner,
) -> Result<(), AppError> {
    if platform == AutostartPlatform::Unsupported {
        return Err(AppError::Message(
            "daemon autostart is not supported on this platform yet \
             (currently only systemd user units on Linux)"
                .to_string(),
        ));
    }
    match cmd {
        AutostartCommand::Enable => enable(runner),
        AutostartCommand::Disable => disable(runner),
        AutostartCommand::Status => status(runner),
    }
}

// --- systemctl runner -------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemctlOutput {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl SystemctlOutput {
    fn combined(&self) -> String {
        [self.stdout.as_str(), self.stderr.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SystemctlError {
    /// The `systemctl` binary does not exist on this system.
    NotFound,
    /// The call exceeded [`SYSTEMCTL_TIMEOUT`].
    TimedOut,
    Io(String),
}

impl fmt::Display for SystemctlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SystemctlError::NotFound => write!(
                f,
                "systemctl not found; daemon autostart requires a systemd user session"
            ),
            SystemctlError::TimedOut => write!(
                f,
                "systemctl timed out after {}s",
                SYSTEMCTL_TIMEOUT.as_secs()
            ),
            SystemctlError::Io(reason) => write!(f, "{reason}"),
        }
    }
}

/// Runs `systemctl` with a timeout. Injectable for tests.
pub(crate) trait SystemctlRunner {
    fn run(&self, args: &[&str]) -> Result<SystemctlOutput, SystemctlError>;
}

pub(crate) struct RealSystemctl;

impl SystemctlRunner for RealSystemctl {
    fn run(&self, args: &[&str]) -> Result<SystemctlOutput, SystemctlError> {
        run_command_with_timeout("systemctl", args, SYSTEMCTL_TIMEOUT)
    }
}

fn run_command_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<SystemctlOutput, SystemctlError> {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SystemctlError::NotFound)
        }
        Err(error) => return Err(SystemctlError::Io(error.to_string())),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| SystemctlError::Io(error.to_string()))?;
                return Ok(SystemctlOutput {
                    success: output.status.success(),
                    stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                });
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SystemctlError::TimedOut);
            }
            Err(error) => return Err(SystemctlError::Io(error.to_string())),
        }
    }
}

fn systemctl_failure(context: &str, error: SystemctlError) -> AppError {
    AppError::Message(format!("{context} failed: {error}"))
}

fn require_success(
    context: &str,
    result: Result<SystemctlOutput, SystemctlError>,
) -> Result<(), AppError> {
    let output = result.map_err(|error| systemctl_failure(context, error))?;
    if !output.success {
        let detail = output.combined();
        return Err(AppError::Message(format!(
            "{context} failed: {}",
            if detail.is_empty() {
                "systemctl exited non-zero".to_string()
            } else {
                detail
            }
        )));
    }
    Ok(())
}

// --- paths and unit rendering -----------------------------------------------

pub(crate) fn systemd_user_dir() -> Result<PathBuf, AppError> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("systemd").join("user"));
    }
    let home = crate::config::home_dir().ok_or_else(|| {
        AppError::Message("cannot resolve home directory for systemd user units".to_string())
    })?;
    Ok(home.join(".config").join("systemd").join("user"))
}

pub(crate) fn unit_path() -> Result<PathBuf, AppError> {
    Ok(systemd_user_dir()?.join(UNIT_NAME))
}

fn current_executable() -> Result<PathBuf, AppError> {
    std::env::current_exe()
        .map_err(|err| AppError::Message(format!("resolve current executable: {err}")))
}

/// Quote one ExecStart token per systemd.syntax: double quotes plus backslash
/// escapes, applied only when the path needs them.
fn quote_exec_arg(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        raw.into_owned()
    }
}

pub(crate) fn render_unit(exec: &Path) -> String {
    format!(
        "[Unit]\nDescription=cc-switch daemon (proxy supervisor and Codex daemon bridge)\n\n[Service]\nExecStart={} daemon start\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        quote_exec_arg(exec)
    )
}

/// Extract the binary path from a rendered unit's ExecStart line.
pub(crate) fn parse_exec_start_binary(content: &str) -> Option<PathBuf> {
    let line = content
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("ExecStart="))?;
    let value = line["ExecStart=".len()..].trim_start();
    if let Some(rest) = value.strip_prefix('"') {
        let mut token = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => token.push(chars.next()?),
                '"' => break,
                other => token.push(other),
            }
        }
        Some(PathBuf::from(token))
    } else {
        value.split_whitespace().next().map(PathBuf::from)
    }
}

// --- flows -------------------------------------------------------------------

fn enable(runner: &dyn SystemctlRunner) -> Result<(), AppError> {
    let exe = current_executable()?;
    let unit_path = unit_path()?;
    let content = render_unit(&exe);
    crate::config::atomic_write_private(&unit_path, content.as_bytes())?;
    require_success(
        "systemctl --user daemon-reload",
        runner.run(&["--user", "daemon-reload"]),
    )?;
    require_success(
        "systemctl --user enable --now",
        runner.run(&["--user", "enable", "--now", UNIT_NAME]),
    )?;
    println!("{}", success("cc-switch daemon autostart enabled"));
    println!("  unit:      {}", unit_path.display());
    println!("  ExecStart: {} daemon start", quote_exec_arg(&exe));
    println!(
        "{}",
        info("re-run `cc-switch daemon autostart enable` after self-updates to refresh ExecStart")
    );
    Ok(())
}

fn disable(runner: &dyn SystemctlRunner) -> Result<(), AppError> {
    let unit_path = unit_path()?;
    let mut systemctl_missing = false;
    match runner.run(&["--user", "disable", "--now", UNIT_NAME]) {
        Ok(output) if output.success => {}
        Ok(output) => {
            // Tolerate an already-absent unit so disable stays idempotent.
            let detail = output.combined();
            if !is_absent_unit_error(&detail) {
                return Err(AppError::Message(format!(
                    "systemctl --user disable --now failed: {detail}"
                )));
            }
        }
        Err(SystemctlError::NotFound) => {
            systemctl_missing = true;
            println!(
                "{}",
                warning("systemctl not found; removing the unit file only")
            );
        }
        Err(error) => return Err(systemctl_failure("systemctl --user disable --now", error)),
    }
    match std::fs::remove_file(&unit_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::Message(format!(
                "remove unit {} failed: {error}",
                unit_path.display()
            )))
        }
    }
    if !systemctl_missing {
        // Best-effort: a stale unit cache must not fail an idempotent disable.
        if let Err(error) = runner.run(&["--user", "daemon-reload"]) {
            println!(
                "{}",
                warning(&format!("systemctl --user daemon-reload failed: {error}"))
            );
        }
    }
    println!("{}", success("cc-switch daemon autostart disabled"));
    Ok(())
}

fn is_absent_unit_error(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("not loaded")
        || detail.contains("not found")
        || detail.contains("does not exist")
        || detail.contains("no such file")
}

/// Freshness of the unit's ExecStart binary relative to this executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecFreshness {
    /// Same binary as the running executable.
    Current,
    /// The ExecStart binary no longer exists on disk.
    Missing,
    /// The ExecStart binary exists but differs from the running executable
    /// (typical after a self-update).
    Differs,
}

#[derive(Debug)]
pub(crate) struct AutostartStatus {
    pub(crate) unit_path: PathBuf,
    pub(crate) installed: bool,
    /// Raw `is-enabled` state ("enabled", "disabled", ...); `None` when
    /// systemctl is unavailable.
    pub(crate) enabled: Option<String>,
    /// Raw `is-active` state ("active", "inactive", "failed", ...); `None`
    /// when systemctl is unavailable.
    pub(crate) active: Option<String>,
    pub(crate) exec_binary: Option<PathBuf>,
    pub(crate) exec_freshness: Option<ExecFreshness>,
    pub(crate) systemctl_unavailable: bool,
}

pub(crate) fn collect_status(runner: &dyn SystemctlRunner) -> Result<AutostartStatus, AppError> {
    let unit_path = unit_path()?;
    let installed = unit_path.is_file();
    let mut exec_binary = None;
    let mut exec_freshness = None;
    if installed {
        if let Ok(content) = std::fs::read_to_string(&unit_path) {
            exec_binary = parse_exec_start_binary(&content);
            exec_freshness = exec_binary.as_deref().map(classify_exec_freshness);
        }
    }

    let mut status = AutostartStatus {
        unit_path,
        installed,
        enabled: None,
        active: None,
        exec_binary,
        exec_freshness,
        systemctl_unavailable: false,
    };
    match runner.run(&["--user", "is-enabled", UNIT_NAME]) {
        Ok(output) => {
            status.enabled = Some(state_or(
                &output,
                if output.success {
                    "enabled"
                } else {
                    "disabled"
                },
            ))
        }
        Err(SystemctlError::NotFound) => {
            status.systemctl_unavailable = true;
            return Ok(status);
        }
        Err(error) => return Err(systemctl_failure("systemctl --user is-enabled", error)),
    }
    match runner.run(&["--user", "is-active", UNIT_NAME]) {
        Ok(output) => {
            status.active = Some(state_or(
                &output,
                if output.success { "active" } else { "inactive" },
            ))
        }
        Err(SystemctlError::NotFound) => status.systemctl_unavailable = true,
        Err(error) => return Err(systemctl_failure("systemctl --user is-active", error)),
    }
    Ok(status)
}

fn state_or(output: &SystemctlOutput, fallback: &str) -> String {
    if output.stdout.is_empty() {
        fallback.to_string()
    } else {
        output.stdout.clone()
    }
}

fn classify_exec_freshness(exec: &Path) -> ExecFreshness {
    if !exec.exists() {
        return ExecFreshness::Missing;
    }
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match std::env::current_exe() {
        Ok(current) if canonical(exec) == canonical(&current) => ExecFreshness::Current,
        _ => ExecFreshness::Differs,
    }
}

fn status(runner: &dyn SystemctlRunner) -> Result<(), AppError> {
    let status = collect_status(runner)?;
    println!("{}", highlight("cc-switch daemon autostart"));
    println!("  unit:      {}", status.unit_path.display());
    println!(
        "  installed: {}",
        if status.installed { "yes" } else { "no" }
    );
    if let Some(exec) = &status.exec_binary {
        println!("  ExecStart: {} daemon start", quote_exec_arg(exec));
    }
    match status.exec_freshness {
        Some(ExecFreshness::Missing) => println!(
            "{}",
            warning(
                "  warning:   ExecStart binary no longer exists; re-run `cc-switch daemon autostart enable` after updates"
            )
        ),
        Some(ExecFreshness::Differs) => println!(
            "{}",
            warning(
                "  warning:   ExecStart binary differs from the current executable; re-run `cc-switch daemon autostart enable` to refresh"
            )
        ),
        _ => {}
    }
    if status.systemctl_unavailable {
        println!(
            "{}",
            warning("  systemctl: unavailable (daemon autostart requires a systemd user session)")
        );
        return Ok(());
    }
    println!(
        "  enabled:   {}",
        status.enabled.as_deref().unwrap_or("unknown")
    );
    println!(
        "  active:    {}",
        status.active.as_deref().unwrap_or("unknown")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeSystemctl {
        calls: Mutex<Vec<Vec<String>>>,
        responses: Mutex<VecDeque<Result<SystemctlOutput, SystemctlError>>>,
    }

    impl FakeSystemctl {
        fn new(responses: Vec<Result<SystemctlOutput, SystemctlError>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            }
        }

        fn ok() -> Result<SystemctlOutput, SystemctlError> {
            Ok(SystemctlOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SystemctlRunner for FakeSystemctl {
        fn run(&self, args: &[&str]) -> Result<SystemctlOutput, SystemctlError> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(Self::ok)
        }
    }

    /// Restore XDG_CONFIG_HOME after the test (TestEnvGuard does not track it).
    struct XdgConfigGuard(Option<std::ffi::OsString>);

    impl Drop for XdgConfigGuard {
        fn drop(&mut self) {
            crate::test_support::restore_env("XDG_CONFIG_HOME", &self.0);
        }
    }

    fn isolated_env() -> (
        tempfile::TempDir,
        crate::test_support::TestEnvGuard,
        XdgConfigGuard,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let env = crate::test_support::TestEnvGuard::isolated(temp.path());
        let xdg = XdgConfigGuard(std::env::var_os("XDG_CONFIG_HOME"));
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        (temp, env, xdg)
    }

    fn argv(calls: &[&[&str]]) -> Vec<Vec<String>> {
        calls
            .iter()
            .map(|call| call.iter().map(|arg| arg.to_string()).collect())
            .collect()
    }

    #[test]
    fn render_unit_quotes_exec_path_with_spaces_and_roundtrips() {
        let exec = Path::new("/opt/my tools/cc-switch");
        let unit = render_unit(exec);
        assert!(unit.contains("Description=cc-switch daemon"));
        assert!(unit.contains("ExecStart=\"/opt/my tools/cc-switch\" daemon start\n"));
        assert!(unit.contains("Restart=on-failure\n"));
        assert!(unit.contains("RestartSec=5\n"));
        assert!(unit.contains("WantedBy=default.target\n"));
        assert_eq!(parse_exec_start_binary(&unit).as_deref(), Some(exec));
    }

    #[test]
    fn render_unit_leaves_simple_path_unquoted() {
        let exec = Path::new("/home/user/.local/bin/cc-switch");
        let unit = render_unit(exec);
        assert!(unit.contains("ExecStart=/home/user/.local/bin/cc-switch daemon start\n"));
        assert_eq!(parse_exec_start_binary(&unit).as_deref(), Some(exec));
    }

    #[test]
    fn parse_exec_start_binary_unescapes_quoted_chars() {
        let unit = render_unit(Path::new("/opt/weird\"dir/cc-switch"));
        assert_eq!(
            parse_exec_start_binary(&unit).as_deref(),
            Some(Path::new("/opt/weird\"dir/cc-switch"))
        );
    }

    #[test]
    fn unsupported_platform_returns_clear_error_without_calling_systemctl() {
        let fake = FakeSystemctl::new(vec![]);
        for cmd in [
            AutostartCommand::Enable,
            AutostartCommand::Disable,
            AutostartCommand::Status,
        ] {
            let err = execute_with(&cmd, AutostartPlatform::Unsupported, &fake).unwrap_err();
            assert!(
                err.to_string().contains("not supported on this platform"),
                "unexpected error: {err}"
            );
        }
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn enable_writes_unit_and_invokes_systemctl_in_order() {
        let (_temp, _env, _xdg) = isolated_env();
        let fake = FakeSystemctl::new(vec![]);
        execute_with(
            &AutostartCommand::Enable,
            AutostartPlatform::SystemdUser,
            &fake,
        )
        .unwrap();

        let unit_path = unit_path().unwrap();
        assert!(unit_path.is_file());
        let content = std::fs::read_to_string(&unit_path).unwrap();
        let current = std::env::current_exe().unwrap();
        assert!(content.contains(&format!("{} daemon start", quote_exec_arg(&current))));
        assert!(content.contains("WantedBy=default.target"));
        assert_eq!(
            fake.calls(),
            argv(&[
                &["--user", "daemon-reload"],
                &["--user", "enable", "--now", "cc-switch-daemon.service"],
            ])
        );
    }

    #[test]
    fn enable_reports_missing_systemctl_clearly() {
        let (_temp, _env, _xdg) = isolated_env();
        let fake = FakeSystemctl::new(vec![Err(SystemctlError::NotFound)]);
        let err = execute_with(
            &AutostartCommand::Enable,
            AutostartPlatform::SystemdUser,
            &fake,
        )
        .unwrap_err();
        assert!(err.to_string().contains("systemctl not found"), "{err}");
        assert!(err.to_string().contains("systemd user session"), "{err}");
    }

    #[test]
    fn disable_removes_unit_and_reloads() {
        let (_temp, _env, _xdg) = isolated_env();
        let unit_path = unit_path().unwrap();
        std::fs::create_dir_all(unit_path.parent().unwrap()).unwrap();
        std::fs::write(&unit_path, render_unit(Path::new("/bin/cc-switch"))).unwrap();

        let fake = FakeSystemctl::new(vec![]);
        execute_with(
            &AutostartCommand::Disable,
            AutostartPlatform::SystemdUser,
            &fake,
        )
        .unwrap();
        assert!(!unit_path.exists());
        assert_eq!(
            fake.calls(),
            argv(&[
                &["--user", "disable", "--now", "cc-switch-daemon.service"],
                &["--user", "daemon-reload"],
            ])
        );
    }

    #[test]
    fn disable_is_idempotent_when_unit_is_absent() {
        let (_temp, _env, _xdg) = isolated_env();
        let fake = FakeSystemctl::new(vec![Ok(SystemctlOutput {
            success: false,
            stdout: String::new(),
            stderr: "Unit cc-switch-daemon.service not loaded.".to_string(),
        })]);
        execute_with(
            &AutostartCommand::Disable,
            AutostartPlatform::SystemdUser,
            &fake,
        )
        .unwrap();
        assert!(!unit_path().unwrap().exists());
        assert_eq!(
            fake.calls(),
            argv(&[
                &["--user", "disable", "--now", "cc-switch-daemon.service"],
                &["--user", "daemon-reload"],
            ])
        );
    }

    #[test]
    fn disable_without_systemctl_still_removes_unit_file() {
        let (_temp, _env, _xdg) = isolated_env();
        let unit_path = unit_path().unwrap();
        std::fs::create_dir_all(unit_path.parent().unwrap()).unwrap();
        std::fs::write(&unit_path, render_unit(Path::new("/bin/cc-switch"))).unwrap();

        let fake = FakeSystemctl::new(vec![Err(SystemctlError::NotFound)]);
        execute_with(
            &AutostartCommand::Disable,
            AutostartPlatform::SystemdUser,
            &fake,
        )
        .unwrap();
        assert!(!unit_path.exists());
        // No daemon-reload is attempted once systemctl is known to be missing.
        assert_eq!(
            fake.calls(),
            argv(&[&["--user", "disable", "--now", "cc-switch-daemon.service"]])
        );
    }

    #[test]
    fn status_reflects_installed_enabled_active_from_fake_outputs() {
        let (_temp, _env, _xdg) = isolated_env();
        let unit_path = unit_path().unwrap();
        std::fs::create_dir_all(unit_path.parent().unwrap()).unwrap();
        let current = std::env::current_exe().unwrap();
        std::fs::write(&unit_path, render_unit(&current)).unwrap();

        let fake = FakeSystemctl::new(vec![
            Ok(SystemctlOutput {
                success: true,
                stdout: "enabled".to_string(),
                stderr: String::new(),
            }),
            Ok(SystemctlOutput {
                success: true,
                stdout: "active".to_string(),
                stderr: String::new(),
            }),
        ]);
        let status = collect_status(&fake).unwrap();
        assert!(status.installed);
        assert_eq!(status.enabled.as_deref(), Some("enabled"));
        assert_eq!(status.active.as_deref(), Some("active"));
        assert_eq!(status.exec_binary.as_deref(), Some(current.as_path()));
        assert_eq!(status.exec_freshness, Some(ExecFreshness::Current));
        assert!(!status.systemctl_unavailable);
        assert_eq!(
            fake.calls(),
            argv(&[
                &["--user", "is-enabled", "cc-switch-daemon.service"],
                &["--user", "is-active", "cc-switch-daemon.service"],
            ])
        );
    }

    #[test]
    fn status_without_unit_and_inactive_service_reports_not_installed() {
        let (_temp, _env, _xdg) = isolated_env();
        let fake = FakeSystemctl::new(vec![
            Ok(SystemctlOutput {
                success: false,
                stdout: "disabled".to_string(),
                stderr: String::new(),
            }),
            Ok(SystemctlOutput {
                success: false,
                stdout: "inactive".to_string(),
                stderr: String::new(),
            }),
        ]);
        let status = collect_status(&fake).unwrap();
        assert!(!status.installed);
        assert_eq!(status.enabled.as_deref(), Some("disabled"));
        assert_eq!(status.active.as_deref(), Some("inactive"));
        assert_eq!(status.exec_binary, None);
    }

    #[test]
    fn status_marks_missing_systemctl_as_unavailable_without_failing() {
        let (_temp, _env, _xdg) = isolated_env();
        let fake = FakeSystemctl::new(vec![Err(SystemctlError::NotFound)]);
        let status = collect_status(&fake).unwrap();
        assert!(status.systemctl_unavailable);
        assert_eq!(status.enabled, None);
        assert_eq!(status.active, None);
    }

    #[test]
    fn status_flags_missing_and_stale_exec_binary() {
        let (_temp, _env, _xdg) = isolated_env();
        let unit_path = unit_path().unwrap();
        std::fs::create_dir_all(unit_path.parent().unwrap()).unwrap();

        std::fs::write(&unit_path, render_unit(Path::new("/nonexistent/cc-switch"))).unwrap();
        let fake = FakeSystemctl::new(vec![]);
        let status = collect_status(&fake).unwrap();
        assert_eq!(status.exec_freshness, Some(ExecFreshness::Missing));

        // Existing but different binary (self-update staleness hint).
        std::fs::write(&unit_path, render_unit(Path::new("/bin/sh"))).unwrap();
        let fake = FakeSystemctl::new(vec![]);
        let status = collect_status(&fake).unwrap();
        assert_eq!(status.exec_freshness, Some(ExecFreshness::Differs));
    }

    #[test]
    fn run_command_with_timeout_maps_missing_binary_to_not_found() {
        let err = run_command_with_timeout(
            "cc-switch-definitely-missing-systemctl-binary",
            &["--user", "status"],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(err, SystemctlError::NotFound);
    }
}
