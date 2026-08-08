#![cfg_attr(not(unix), allow(dead_code))]

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

pub const SUPPORTED_CODEX_VERSION: &str = "0.146.0";
const CODEX_VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const VERSION_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRuntime {
    pub codex_bin: PathBuf,
    pub codex_home: PathBuf,
    pub version: String,
    pub standalone_managed: bool,
}

pub fn resolve_codex_runtime(
    codex_home: PathBuf,
    explicit: Option<&Path>,
    path_env: Option<&OsStr>,
) -> Result<CodexRuntime> {
    resolve_codex_runtime_with_timeout(codex_home, explicit, path_env, CODEX_VERSION_TIMEOUT)
}

fn resolve_codex_runtime_with_timeout(
    codex_home: PathBuf,
    explicit: Option<&Path>,
    path_env: Option<&OsStr>,
    timeout: Duration,
) -> Result<CodexRuntime> {
    let codex_bin = resolve_codex_binary_from(explicit, &codex_home, path_env)?;
    let output = run_version_command(&codex_bin, &codex_home, timeout)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`{}` --version exited with {}: {}",
            codex_bin.display(),
            output.status,
            stderr.trim()
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| anyhow!("`{}` --version output was not UTF-8", codex_bin.display()))?;
    let version = parse_supported_codex_version(&stdout)?;
    let standalone = codex_home
        .join("packages/standalone/current")
        .join(codex_executable_name());
    let standalone_managed = normalized_path(&codex_bin) == normalized_path(&standalone);
    Ok(CodexRuntime {
        codex_bin,
        codex_home,
        version,
        standalone_managed,
    })
}

fn run_version_command(codex_bin: &Path, codex_home: &Path, timeout: Duration) -> Result<Output> {
    let mut child = Command::new(codex_bin)
        .arg("--version")
        .env("CODEX_HOME", codex_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            anyhow!(
                "failed to execute `{}` --version: {err}",
                codex_bin.display()
            )
        })?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to wait for `{}` --version", codex_bin.display()))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "timed out after {} seconds waiting for `{}` --version",
                timeout.as_secs_f64(),
                codex_bin.display()
            );
        }
        std::thread::sleep(
            VERSION_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
        );
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_end(&mut stdout)
            .context("failed to read Codex version stdout")?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_end(&mut stderr)
            .context("failed to read Codex version stderr")?;
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn normalized_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn resolve_codex_binary_from(
    explicit: Option<&Path>,
    codex_home: &Path,
    path_env: Option<&OsStr>,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        ensure_executable(path, "configured Codex binary")?;
        return Ok(path.to_path_buf());
    }

    let standalone = codex_home
        .join("packages/standalone/current")
        .join(codex_executable_name());
    if standalone.is_file() {
        ensure_executable(&standalone, "standalone Codex binary")?;
        return Ok(standalone);
    }

    let path_env = path_env.ok_or_else(|| anyhow!("PATH is not set; pass --codex PATH"))?;
    for directory in std::env::split_paths(path_env) {
        let candidate = directory.join(codex_executable_name());
        if candidate.is_file() && ensure_executable(&candidate, "Codex binary on PATH").is_ok() {
            return Ok(candidate);
        }
    }
    bail!("could not find an executable `codex` on PATH; pass --codex PATH")
}

pub fn parse_supported_codex_version(stdout: &str) -> Result<String> {
    let mut fields = stdout.split_whitespace();
    let product = fields.next().unwrap_or_default();
    let version = fields.next().unwrap_or_default();
    if product != "codex-cli" || version.is_empty() || fields.next().is_some() {
        bail!("unexpected `codex --version` output: `{}`", stdout.trim());
    }
    if version != SUPPORTED_CODEX_VERSION {
        bail!(
            "Codex {version} is incompatible with the reviewed app-server protocol; expected {SUPPORTED_CODEX_VERSION}"
        );
    }
    Ok(version.to_string())
}

fn codex_executable_name() -> &'static str {
    if cfg!(windows) { "codex.exe" } else { "codex" }
}

fn ensure_executable(path: &Path, scope: &str) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .map_err(|err| anyhow!("{scope} `{}` is unavailable: {err}", path.display()))?;
    if !metadata.is_file() {
        bail!("{scope} `{}` is not a file", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("{scope} `{}` is not executable", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    fn executable(path: &Path) {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn explicit_codex_binary_wins_without_silent_fallback() {
        let temp = TempDir::new().unwrap();
        let explicit = temp.path().join("explicit-codex");
        let path_dir = temp.path().join("path-bin");
        fs::create_dir(&path_dir).unwrap();
        executable(&explicit);
        executable(&path_dir.join("codex"));

        let selected =
            resolve_codex_binary_from(Some(&explicit), temp.path(), Some(path_dir.as_os_str()))
                .unwrap();
        assert_eq!(selected, explicit);

        let missing = temp.path().join("missing-codex");
        let error =
            resolve_codex_binary_from(Some(&missing), temp.path(), Some(path_dir.as_os_str()))
                .unwrap_err();
        assert!(error.to_string().contains("missing-codex"));
    }

    #[cfg(unix)]
    #[test]
    fn codex_version_check_has_a_hard_timeout() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let binary = temp.path().join("codex");
        fs::write(&binary, "#!/bin/sh\nexec sleep 5\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();

        let started = Instant::now();
        let error = resolve_codex_runtime_with_timeout(
            temp.path().to_path_buf(),
            Some(&binary),
            None,
            Duration::from_millis(50),
        )
        .unwrap_err();

        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn standalone_codex_wins_before_path() {
        let temp = TempDir::new().unwrap();
        let standalone = temp
            .path()
            .join("packages/standalone/current")
            .join(if cfg!(windows) { "codex.exe" } else { "codex" });
        fs::create_dir_all(standalone.parent().unwrap()).unwrap();
        executable(&standalone);
        let path_dir = temp.path().join("path-bin");
        fs::create_dir(&path_dir).unwrap();
        executable(&path_dir.join(if cfg!(windows) { "codex.exe" } else { "codex" }));

        let selected =
            resolve_codex_binary_from(None, temp.path(), Some(path_dir.as_os_str())).unwrap();
        assert_eq!(selected, standalone);
    }

    #[test]
    fn path_codex_is_the_final_fallback() {
        let temp = TempDir::new().unwrap();
        let path_dir = temp.path().join("path-bin");
        fs::create_dir(&path_dir).unwrap();
        let path_codex = path_dir.join(if cfg!(windows) { "codex.exe" } else { "codex" });
        executable(&path_codex);

        let selected =
            resolve_codex_binary_from(None, temp.path(), Some(path_dir.as_os_str())).unwrap();
        assert_eq!(selected, path_codex);
    }

    #[test]
    fn version_parser_accepts_only_the_reviewed_release() {
        assert_eq!(
            parse_supported_codex_version("codex-cli 0.146.0\n").unwrap(),
            SUPPORTED_CODEX_VERSION
        );
        let error = parse_supported_codex_version("codex-cli 0.147.0\n").unwrap_err();
        assert!(error.to_string().contains("0.147.0"));
        assert!(error.to_string().contains(SUPPORTED_CODEX_VERSION));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_resolution_executes_version_check_with_selected_home() {
        let temp = TempDir::new().unwrap();
        let binary = temp.path().join("codex");
        let observed_home = temp.path().join("observed-home");
        let script = format!(
            "#!/bin/sh\nprintf '%s' \"$CODEX_HOME\" > '{}'\nprintf 'codex-cli 0.146.0\\n'\n",
            observed_home.display()
        );
        fs::write(&binary, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let codex_home = temp.path().join("home");

        let runtime = resolve_codex_runtime(codex_home.clone(), Some(&binary), None).unwrap();
        assert_eq!(runtime.codex_bin, binary);
        assert_eq!(runtime.codex_home, codex_home);
        assert_eq!(runtime.version, SUPPORTED_CODEX_VERSION);
        assert_eq!(
            fs::read_to_string(observed_home).unwrap(),
            codex_home.to_string_lossy()
        );
    }
}
