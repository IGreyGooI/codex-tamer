#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::io::Read;
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::codex_runtime::SUPPORTED_CODEX_VERSION;
#[cfg(unix)]
use crate::codex_runtime::resolve_codex_runtime;
use crate::config::{AppConfig, Endpoint, resolve_codex_home};
use crate::rpc::{CLIENT_NAME, InitializeInfo, PeerIdentity, RpcClient};

const START_LOCK_FILE: &str = "start.lock";
const PROCESS_FILE: &str = "process.json";
const STDERR_LOG_FILE: &str = "app-server.stderr.log";
const START_TIMEOUT: Duration = Duration::from_secs(10);
const START_POLL_INTERVAL: Duration = Duration::from_millis(100);
const LISTENER_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const LOG_TAIL_BYTES: usize = 4096;
const PROCESS_RECORD_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedServerReport {
    pub status: String,
    pub backend: String,
    pub endpoint: String,
    pub codex_home: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_bin: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProcessRecord {
    schema_version: u32,
    pid: u32,
    process_group_id: u32,
    process_start_time: String,
    boot_id: Option<String>,
    codex_bin: PathBuf,
    codex_version: String,
    codex_home: PathBuf,
    endpoint: String,
}

struct ManagedPaths {
    directory: PathBuf,
    lock: PathBuf,
    process: PathBuf,
    stderr_log: PathBuf,
}

pub async fn connect_or_start(
    config: &AppConfig,
    endpoint: &Endpoint,
) -> Result<(RpcClient, ManagedServerReport)> {
    let codex_home = resolve_codex_home(config)?;

    #[cfg(not(unix))]
    {
        let _ = (config, endpoint, codex_home);
        bail!("automatic managed app-server startup is supported only on Unix")
    }

    #[cfg(unix)]
    {
        let paths = ManagedPaths::new(endpoint)?;
        paths.prepare()?;
        let startup_deadline = Instant::now() + START_TIMEOUT;

        if let Some(client) =
            connect_existing(endpoint, &codex_home, false, startup_deadline).await?
        {
            let report = reused_report(endpoint, &codex_home, &client)?;
            return Ok((client, report));
        }

        let lock_file = open_private_file(&paths.lock, false)?;
        run_before_start_deadline(
            startup_deadline,
            acquire_start_lock(&lock_file),
            "managed app-server start lock",
        )
        .await?;

        if let Some(client) =
            connect_existing(endpoint, &codex_home, true, startup_deadline).await?
        {
            let report = reused_report(endpoint, &codex_home, &client)?;
            return Ok((client, report));
        }

        if let Some(record) = read_active_process_record(&paths.process)? {
            if normalized_path(&record.codex_home) != normalized_path(&codex_home)
                || record.endpoint != endpoint.display()
            {
                bail!("managed app-server process record does not match the selected target");
            }
            bail!(
                "managed app-server pid {} is still running but `{}` is not ready; refusing to start a duplicate process",
                record.pid,
                endpoint.display()
            );
        }

        let runtime = resolve_codex_runtime(
            codex_home.clone(),
            config.managed.codex.as_deref(),
            std::env::var_os("PATH").as_deref(),
        )?;
        let log = open_private_file(&paths.stderr_log, true)?;
        let mut command = Command::new(&runtime.codex_bin);
        command
            .args(["app-server", "--listen", endpoint.display().as_str()])
            .env("CODEX_HOME", &runtime.codex_home)
            .env_remove("CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start Codex app-server using `{}`",
                runtime.codex_bin.display()
            )
        })?;
        let pid = child.id();
        let process_start_time = match read_process_start_time(pid) {
            Ok(process_start_time) => process_start_time,
            Err(err) => return Err(cleanup_spawn_failure(&mut child, err)),
        };
        let process_group_id = match read_process_group(pid) {
            Ok(process_group_id) if process_group_id == pid => process_group_id,
            Ok(process_group_id) => {
                return Err(cleanup_spawn_failure(
                    &mut child,
                    anyhow!(
                        "managed app-server pid {pid} started in unexpected process group {process_group_id}"
                    ),
                ));
            }
            Err(err) => return Err(cleanup_spawn_failure(&mut child, err)),
        };
        let boot_id = match current_boot_id() {
            Ok(boot_id) => boot_id,
            Err(err) => return Err(cleanup_spawn_failure(&mut child, err)),
        };
        let record = ProcessRecord {
            schema_version: PROCESS_RECORD_SCHEMA_VERSION,
            pid,
            process_group_id,
            process_start_time,
            boot_id,
            codex_bin: runtime.codex_bin.clone(),
            codex_version: runtime.version.clone(),
            codex_home: runtime.codex_home.clone(),
            endpoint: endpoint.display(),
        };
        if let Err(err) = write_process_record(&paths.process, &record) {
            return Err(cleanup_spawn_failure(&mut child, err));
        }

        match wait_for_server(endpoint, &runtime.codex_home, &record).await {
            Ok(client) => Ok((
                client,
                ManagedServerReport {
                    status: "started".to_string(),
                    backend: "codex-tamer".to_string(),
                    endpoint: endpoint.display(),
                    codex_home: runtime.codex_home,
                    codex_bin: Some(runtime.codex_bin),
                    codex_version: Some(runtime.version),
                    pid: Some(pid),
                },
            )),
            Err(err) => {
                let err = match terminate_and_reap(&mut child) {
                    Ok(()) => {
                        let _ = fs::remove_file(&paths.process);
                        err
                    }
                    Err(cleanup_error) => err.context(format!(
                        "also failed to terminate and reap spawned app-server; preserving ownership record: {cleanup_error:#}"
                    )),
                };
                let tail = read_log_tail(&paths.stderr_log).unwrap_or_default();
                if tail.is_empty() {
                    Err(err).context("managed Codex app-server did not become ready")
                } else {
                    Err(err).context(format!(
                        "managed Codex app-server did not become ready; stderr tail:\n{tail}"
                    ))
                }
            }
        }
    }
}

pub async fn probe(
    config: &AppConfig,
    endpoint: &Endpoint,
) -> Result<(RpcClient, ManagedServerReport)> {
    probe_optional(config, endpoint).await?.ok_or_else(|| {
        anyhow!(
            "managed app-server at `{}` is not running",
            endpoint.display()
        )
    })
}

pub async fn probe_optional(
    config: &AppConfig,
    endpoint: &Endpoint,
) -> Result<Option<(RpcClient, ManagedServerReport)>> {
    let codex_home = resolve_codex_home(config)?;
    #[cfg(unix)]
    {
        let paths = ManagedPaths::new(endpoint)?;
        paths.prepare()?;
    }
    let client = match connect_validated(endpoint, &codex_home).await {
        Ok(client) => client,
        Err(error) => {
            if endpoint_accepts_connection(endpoint).await {
                return Err(error);
            }
            return Ok(None);
        }
    };
    let report = reused_report(endpoint, &codex_home, &client)?;
    Ok(Some((client, report)))
}

pub async fn stop(config: &AppConfig, endpoint: &Endpoint) -> Result<ManagedServerReport> {
    let codex_home = resolve_codex_home(config)?;

    #[cfg(not(unix))]
    {
        let _ = (endpoint, codex_home);
        bail!("managed app-server shutdown is supported only on Unix")
    }

    #[cfg(unix)]
    {
        let paths = ManagedPaths::new(endpoint)?;
        paths.prepare()?;
        let lock_file = open_private_file(&paths.lock, false)?;
        acquire_start_lock(&lock_file).await?;
        let Some(record) = read_active_process_record(&paths.process)? else {
            let deadline = Instant::now() + START_TIMEOUT;
            match run_before_start_deadline(
                deadline,
                connect_validated(endpoint, &codex_home),
                "managed app-server shutdown handshake",
            )
            .await
            {
                Ok(_) => bail!(
                    "app-server at `{}` is reachable but is not owned by codex-tamer; refusing to stop it",
                    endpoint.display()
                ),
                Err(error) if endpoint_accepts_connection(endpoint).await => {
                    return Err(error).context(
                        "managed app-server endpoint is reachable but cannot be verified for shutdown",
                    );
                }
                Err(_) => {}
            }
            return Ok(ManagedServerReport {
                status: "notRunning".to_string(),
                backend: "codex-tamer".to_string(),
                endpoint: endpoint.display(),
                codex_home,
                codex_bin: None,
                codex_version: None,
                pid: None,
            });
        };
        if normalized_path(&record.codex_home) != normalized_path(&codex_home)
            || record.endpoint != endpoint.display()
        {
            bail!("managed app-server process record does not match the selected target");
        }
        match connect_validated(endpoint, &codex_home).await {
            Ok(client) => validate_peer_ownership(&client, &record)?,
            Err(error) => {
                if endpoint_accepts_connection(endpoint).await {
                    return Err(error).context(
                        "managed app-server endpoint is reachable but cannot be verified for shutdown",
                    );
                }
                if !leader_matches_record(&record)? {
                    bail!(
                        "managed app-server endpoint is unavailable and pid {} cannot prove ownership of process group {}; refusing to stop it",
                        record.pid,
                        record.process_group_id
                    );
                }
            }
        }
        signal_managed_process_group(&record, libc::SIGTERM)
            .with_context(|| format!("failed to stop managed app-server pid {}", record.pid))?;
        let deadline = Instant::now() + START_TIMEOUT;
        while process_group_exists(record.process_group_id)? {
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for managed app-server process group {} to stop",
                    record.process_group_id
                );
            }
            tokio::time::sleep(START_POLL_INTERVAL).await;
        }
        let _ = fs::remove_file(&paths.process);
        Ok(ManagedServerReport {
            status: "stopped".to_string(),
            backend: "codex-tamer".to_string(),
            endpoint: endpoint.display(),
            codex_home,
            codex_bin: Some(record.codex_bin),
            codex_version: Some(record.codex_version),
            pid: Some(record.pid),
        })
    }
}

async fn connect_validated(endpoint: &Endpoint, codex_home: &Path) -> Result<RpcClient> {
    let client = RpcClient::connect(endpoint).await?;
    validate_managed_connection(client.peer_identity(), client.server_info(), codex_home)?;
    Ok(client)
}

async fn connect_existing(
    endpoint: &Endpoint,
    codex_home: &Path,
    reject_reachable_handshake_failure: bool,
    deadline: Instant,
) -> Result<Option<RpcClient>> {
    let was_reachable =
        reject_reachable_handshake_failure && endpoint_accepts_connection(endpoint).await;
    let client = match run_before_start_deadline(
        deadline,
        RpcClient::connect(endpoint),
        "managed app-server handshake",
    )
    .await
    {
        Ok(client) => client,
        Err(err) => {
            if was_reachable
                || (reject_reachable_handshake_failure
                    && endpoint_accepts_connection(endpoint).await)
            {
                return Err(err)
                    .context("managed app-server endpoint is reachable but its handshake failed");
            }
            return Ok(None);
        }
    };
    validate_managed_connection(client.peer_identity(), client.server_info(), codex_home)?;
    Ok(Some(client))
}

fn validate_managed_connection(
    peer: Option<PeerIdentity>,
    info: &InitializeInfo,
    codex_home: &Path,
) -> Result<()> {
    #[cfg(unix)]
    validate_managed_peer_uid(peer)?;
    #[cfg(not(unix))]
    let _ = peer;
    validate_managed_identity(info, codex_home)
}

#[cfg(unix)]
async fn endpoint_accepts_connection(endpoint: &Endpoint) -> bool {
    let Endpoint::Unix { path } = endpoint else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            LISTENER_PROBE_TIMEOUT,
            tokio::net::UnixStream::connect(path)
        )
        .await,
        Ok(Ok(_))
    )
}

#[cfg(not(unix))]
async fn endpoint_accepts_connection(_endpoint: &Endpoint) -> bool {
    false
}

async fn wait_for_server(
    endpoint: &Endpoint,
    codex_home: &Path,
    record: &ProcessRecord,
) -> Result<RpcClient> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let attempt = run_before_start_deadline(
            deadline,
            connect_validated(endpoint, codex_home),
            "managed app-server readiness handshake",
        )
        .await;
        let error = match attempt {
            Ok(client) => match validate_peer_ownership(&client, record) {
                Ok(()) => return Ok(client),
                Err(error) => error,
            },
            Err(err) => err,
        };
        if Instant::now() >= deadline {
            return Err(error);
        }
        tokio::time::sleep(START_POLL_INTERVAL).await;
    }
}

async fn run_before_start_deadline<T>(
    deadline: Instant,
    future: impl std::future::Future<Output = Result<T>>,
    operation: &str,
) -> Result<T> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("timed out during {operation}");
    }
    tokio::time::timeout(remaining, future)
        .await
        .map_err(|_| anyhow!("timed out during {operation}"))?
}

fn reused_report(
    endpoint: &Endpoint,
    codex_home: &Path,
    client: &RpcClient,
) -> Result<ManagedServerReport> {
    let paths = ManagedPaths::new(endpoint)?;
    let record = read_active_process_record(&paths.process)?;
    Ok(match record {
        Some(record)
            if normalized_path(&record.codex_home) == normalized_path(codex_home)
                && record.endpoint == endpoint.display() =>
        {
            validate_peer_ownership(client, &record)?;
            ManagedServerReport {
                status: "reused".to_string(),
                backend: "codex-tamer".to_string(),
                endpoint: endpoint.display(),
                codex_home: codex_home.to_path_buf(),
                codex_bin: Some(record.codex_bin),
                codex_version: Some(record.codex_version),
                pid: Some(record.pid),
            }
        }
        _ => ManagedServerReport {
            status: "reused".to_string(),
            backend: "external".to_string(),
            endpoint: endpoint.display(),
            codex_home: codex_home.to_path_buf(),
            codex_bin: None,
            codex_version: None,
            pid: None,
        },
    })
}

impl ManagedPaths {
    fn new(endpoint: &Endpoint) -> Result<Self> {
        let Endpoint::Unix { path } = endpoint else {
            bail!("managed app-server requires a Unix socket endpoint");
        };
        let directory = path
            .parent()
            .ok_or_else(|| anyhow!("managed app-server socket has no parent directory"))?
            .to_path_buf();
        Ok(Self {
            lock: directory.join(START_LOCK_FILE),
            process: directory.join(PROCESS_FILE),
            stderr_log: directory.join(STDERR_LOG_FILE),
            directory,
        })
    }

    #[cfg(unix)]
    fn prepare(&self) -> Result<()> {
        let root = self
            .directory
            .parent()
            .ok_or_else(|| anyhow!("managed runtime directory has no parent"))?;
        ensure_private_directory(root)?;
        ensure_private_directory(&self.directory)
    }
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to create private directory `{}`", path.display())
            });
        }
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect private directory `{}`", path.display()))?;
    let expected_uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir()
        || metadata.uid() != expected_uid
        || metadata.mode() & 0o777 != 0o700
    {
        bail!(
            "managed runtime directory `{}` must be owned by uid {expected_uid} with mode 0700",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn open_private_file(path: &Path, truncate: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .with_context(|| format!("failed to open private file `{}`", path.display()))?;
    validate_private_file(&file, path)?;
    Ok(file)
}

#[cfg(unix)]
fn validate_private_file(file: &File, path: &Path) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect private file `{}`", path.display()))?;
    let expected_uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != expected_uid || metadata.mode() & 0o077 != 0 {
        bail!(
            "managed runtime file `{}` must be owned by uid {expected_uid} and private",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
async fn acquire_start_lock(file: &File) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != ErrorKind::WouldBlock {
            return Err(err).context("failed to acquire managed app-server start lock");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for managed app-server start lock");
        }
        tokio::time::sleep(START_POLL_INTERVAL).await;
    }
}

#[cfg(target_os = "linux")]
fn read_process_start_time(pid: u32) -> Result<String> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let stat = fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read managed app-server identity `{}`",
            path.display()
        )
    })?;
    let command_end = stat.rfind(')').ok_or_else(|| {
        anyhow!(
            "managed app-server identity `{}` is malformed",
            path.display()
        )
    })?;
    let start_time = stat[command_end + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| {
            anyhow!(
                "managed app-server identity `{}` has no start time",
                path.display()
            )
        })?;
    let start_time = start_time.parse::<u64>().with_context(|| {
        format!(
            "managed app-server identity `{}` has an invalid start time",
            path.display()
        )
    })?;
    Ok(format!("linux-procfs:{start_time}"))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn read_process_start_time(pid: u32) -> Result<String> {
    let ps = [Path::new("/bin/ps"), Path::new("/usr/bin/ps")]
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| Path::new("ps"));
    let output = Command::new(ps)
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .env("TZ", "UTC")
        .output()
        .context("failed to invoke ps for managed app-server")?;
    if !output.status.success() {
        bail!("failed to read start time for managed app-server pid {pid}");
    }
    let start_time = String::from_utf8(output.stdout)
        .context("managed app-server process start time was not UTF-8")?;
    let start_time = start_time.trim();
    if start_time.is_empty() {
        bail!("managed app-server pid {pid} has no start time");
    }
    Ok(format!("ps-utc:{start_time}"))
}

#[cfg(unix)]
fn read_process_group(pid: u32) -> Result<u32> {
    let raw_pid = libc::pid_t::try_from(pid)
        .with_context(|| format!("managed app-server pid {pid} is out of range"))?;
    let process_group = unsafe { libc::getpgid(raw_pid) };
    if process_group == -1 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to inspect process group for pid {pid}"));
    }
    u32::try_from(process_group)
        .with_context(|| format!("managed app-server process group {process_group} is invalid"))
}

#[cfg(target_os = "linux")]
fn current_boot_id() -> Result<Option<String>> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("failed to read Linux boot identity")?;
    let boot_id = boot_id.trim();
    if boot_id.is_empty() {
        bail!("Linux boot identity is empty");
    }
    Ok(Some(boot_id.to_string()))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn current_boot_id() -> Result<Option<String>> {
    Ok(None)
}

fn write_process_record(path: &Path, record: &ProcessRecord) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(&temp)
        .with_context(|| format!("failed to create process record `{}`", temp.display()))?;
    let bytes = serde_json::to_vec(record).context("failed to serialize process record")?;
    if let Err(err) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(err).context("failed to write managed app-server process record");
    }
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(err)
            .with_context(|| format!("failed to publish process record `{}`", path.display()));
    }
    Ok(())
}

fn cleanup_spawn_failure(child: &mut Child, error: anyhow::Error) -> anyhow::Error {
    match terminate_and_reap(child) {
        Ok(()) => error,
        Err(cleanup_error) => error.context(format!(
            "also failed to terminate and reap spawned app-server: {cleanup_error:#}"
        )),
    }
}

#[cfg(unix)]
fn terminate_and_reap(child: &mut Child) -> Result<()> {
    signal_process_group(child.id(), libc::SIGKILL)
        .context("failed to terminate spawned app-server process group")?;
    child.wait().context("failed to reap spawned app-server")?;
    Ok(())
}

#[cfg(not(unix))]
fn terminate_and_reap(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("failed to inspect spawned app-server")?
        .is_some()
    {
        return Ok(());
    }
    child
        .kill()
        .context("failed to terminate spawned app-server")?;
    child.wait().context("failed to reap spawned app-server")?;
    Ok(())
}

#[cfg(unix)]
fn signal_managed_process_group(record: &ProcessRecord, signal: libc::c_int) -> Result<()> {
    if process_exists(record.pid)? {
        let actual_group = read_process_group(record.pid)?;
        if actual_group != record.process_group_id {
            bail!(
                "managed app-server pid {} belongs to unexpected process group {actual_group}",
                record.pid
            );
        }
    }
    signal_process_group(record.process_group_id, signal)
}

#[cfg(unix)]
fn signal_process_group(process_group: u32, signal: libc::c_int) -> Result<()> {
    let process_group = libc::pid_t::try_from(process_group).with_context(|| {
        format!("managed app-server process group {process_group} is out of range")
    })?;
    let result = unsafe { libc::kill(-process_group, signal) };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err)
                .with_context(|| format!("failed to signal process group {process_group}"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn process_group_exists(process_group: u32) -> Result<bool> {
    let process_group = libc::pid_t::try_from(process_group).with_context(|| {
        format!("managed app-server process group {process_group} is out of range")
    })?;
    let result = unsafe { libc::kill(-process_group, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).with_context(|| format!("failed to inspect process group {process_group}")),
    }
}

fn read_active_process_record(path: &Path) -> Result<Option<ProcessRecord>> {
    #[cfg(unix)]
    let text = {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW);
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed to open process record `{}`", path.display())
                });
            }
        };
        validate_private_file(&file, path)?;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .with_context(|| format!("failed to read process record `{}`", path.display()))?;
        text
    };
    #[cfg(not(unix))]
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read process record `{}`", path.display()));
        }
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("invalid process record `{}`", path.display()))?;
    if value.get("schemaVersion").is_none() {
        bail!(
            "legacy process record `{}` cannot be safely validated; preserving it",
            path.display()
        );
    }
    let record: ProcessRecord = serde_json::from_value(value)
        .with_context(|| format!("invalid process record `{}`", path.display()))?;
    if process_matches_record(&record)? {
        Ok(Some(record))
    } else {
        let _ = fs::remove_file(path);
        Ok(None)
    }
}

#[cfg(unix)]
fn process_matches_record(record: &ProcessRecord) -> Result<bool> {
    if record.schema_version != PROCESS_RECORD_SCHEMA_VERSION {
        bail!(
            "unsupported managed process record schema {}; expected {}",
            record.schema_version,
            PROCESS_RECORD_SCHEMA_VERSION
        );
    }
    if record.boot_id != current_boot_id()? {
        return Ok(false);
    }
    if !process_group_exists(record.process_group_id)? {
        return Ok(false);
    }
    if !process_exists(record.pid)? {
        return Ok(true);
    }
    Ok(read_process_group(record.pid)? == record.process_group_id
        && read_process_start_time(record.pid)? == record.process_start_time)
}

#[cfg(unix)]
fn process_exists(pid: u32) -> Result<bool> {
    let raw_pid = libc::pid_t::try_from(pid)
        .with_context(|| format!("managed app-server pid {pid} is out of range"))?;
    let result = unsafe { libc::kill(raw_pid, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).with_context(|| format!("failed to inspect process {pid}")),
    }
}

#[cfg(not(unix))]
fn process_matches_record(_record: &ProcessRecord) -> Result<bool> {
    Ok(false)
}

fn read_log_tail(path: &Path) -> Result<String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read `{}`", path.display()));
        }
    };
    let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
    Ok(String::from_utf8_lossy(&bytes[start..]).trim().to_string())
}

fn server_version_from_user_agent(user_agent: &str) -> Result<&str> {
    let product = user_agent
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("app-server initialize response has an empty userAgent"))?;
    let (name, version) = product
        .rsplit_once('/')
        .ok_or_else(|| anyhow!("app-server userAgent `{user_agent}` has no version"))?;
    if name != CLIENT_NAME {
        bail!(
            "app-server product `{name}` is incompatible; expected managed client product `{CLIENT_NAME}`"
        );
    }
    if version != SUPPORTED_CODEX_VERSION {
        bail!(
            "app-server version `{version}` is incompatible; expected reviewed Codex {SUPPORTED_CODEX_VERSION}"
        );
    }
    Ok(version)
}

fn validate_managed_identity(info: &InitializeInfo, codex_home: &Path) -> Result<()> {
    let _ = server_version_from_user_agent(&info.user_agent)?;
    if normalized_path(&info.codex_home) != normalized_path(codex_home) {
        bail!(
            "managed app-server reported CODEX_HOME `{}`, expected `{}`",
            info.codex_home.display(),
            codex_home.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn validate_peer_ownership(client: &RpcClient, record: &ProcessRecord) -> Result<()> {
    let peer = client
        .peer_identity()
        .ok_or_else(|| anyhow!("managed Unix listener did not expose peer credentials"))?;
    validate_peer_identity(peer, record)
}

#[cfg(unix)]
fn validate_managed_peer_uid(peer: Option<PeerIdentity>) -> Result<PeerIdentity> {
    let peer =
        peer.ok_or_else(|| anyhow!("managed Unix listener did not expose peer credentials"))?;
    let expected_uid = unsafe { libc::geteuid() };
    if peer.uid != expected_uid {
        bail!(
            "managed app-server peer uid {} does not match current uid {expected_uid}",
            peer.uid
        );
    }
    Ok(peer)
}

#[cfg(unix)]
fn validate_peer_identity(peer: PeerIdentity, record: &ProcessRecord) -> Result<()> {
    let peer = validate_managed_peer_uid(Some(peer))?;
    let peer_pid = peer
        .pid
        .ok_or_else(|| anyhow!("managed app-server peer did not expose a process id"))?;
    let peer_group = read_process_group(peer_pid)?;
    if peer_group != record.process_group_id {
        bail!(
            "managed app-server peer pid {peer_pid} belongs to process group {peer_group}, expected {}",
            record.process_group_id
        );
    }
    if record.boot_id != current_boot_id()? {
        bail!("managed app-server process record belongs to another system boot");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_peer_ownership(_client: &RpcClient, _record: &ProcessRecord) -> Result<()> {
    bail!("managed app-server ownership verification is supported only on Unix")
}

#[cfg(unix)]
fn leader_matches_record(record: &ProcessRecord) -> Result<bool> {
    if record.schema_version != PROCESS_RECORD_SCHEMA_VERSION
        || record.boot_id != current_boot_id()?
        || !process_exists(record.pid)?
    {
        return Ok(false);
    }
    Ok(read_process_group(record.pid)? == record.process_group_id
        && read_process_start_time(record.pid)? == record.process_start_time)
}

fn normalized_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn wait_for_recorded_pid(path: &Path, label: &str) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match fs::read_to_string(path) {
                Ok(contents) => match contents.parse::<u32>() {
                    Ok(pid) => return pid,
                    Err(_) if Instant::now() < deadline => {}
                    Err(error) => panic!("{label} did not write a numeric pid: {error}"),
                },
                Err(error) if error.kind() == ErrorKind::NotFound && Instant::now() < deadline => {}
                Err(error) => panic!("failed to read {label} pid: {error}"),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn assert_process_exits(pid: u32, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(pid).unwrap() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_exists(pid).unwrap(),
            "{label} pid {pid} survived cleanup"
        );
    }

    fn info(home: &str, user_agent: &str) -> InitializeInfo {
        InitializeInfo {
            user_agent: user_agent.to_string(),
            codex_home: PathBuf::from(home),
            platform_family: "unix".to_string(),
            platform_os: "linux".to_string(),
        }
    }

    #[test]
    fn managed_identity_requires_the_selected_home_and_reviewed_server_version() {
        validate_managed_identity(
            &info(
                "/tmp/codex-a",
                "codex-tamer/0.146.0 (Ubuntu 24.4.0; x86_64) xterm-256color (codex-tamer; 0.3.1)",
            ),
            Path::new("/tmp/codex-a"),
        )
        .unwrap();

        let wrong_home = validate_managed_identity(
            &info("/tmp/codex-b", "codex-tamer/0.146.0 (Linux; x86_64)"),
            Path::new("/tmp/codex-a"),
        )
        .unwrap_err();
        assert!(wrong_home.to_string().contains("/tmp/codex-b"));

        let wrong_version = validate_managed_identity(
            &info("/tmp/codex-a", "codex-tamer/0.147.0 (Linux; x86_64)"),
            Path::new("/tmp/codex-a"),
        )
        .unwrap_err();
        assert!(wrong_version.to_string().contains(SUPPORTED_CODEX_VERSION));
    }

    #[test]
    fn app_server_user_agent_version_must_be_structured() {
        assert_eq!(
            server_version_from_user_agent("codex-tamer/0.146.0 (Linux; x86_64)").unwrap(),
            "0.146.0"
        );
        assert!(server_version_from_user_agent("mock-codex").is_err());
        assert!(server_version_from_user_agent("codex_cli_rs/0.146.0").is_err());
        assert!(server_version_from_user_agent("codex_vscode/0.146.0").is_err());
        assert!(server_version_from_user_agent("codex/0.146.0-dev").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn reused_managed_connection_requires_a_same_uid_peer() {
        let expected_uid = unsafe { libc::geteuid() };
        let peer = PeerIdentity {
            pid: Some(std::process::id()),
            uid: expected_uid.saturating_add(1),
            gid: unsafe { libc::getegid() },
        };

        let error = validate_managed_connection(
            Some(peer),
            &info("/tmp/codex-a", "codex-tamer/0.146.0 (Linux; x86_64)"),
            Path::new("/tmp/codex-a"),
        )
        .unwrap_err();

        assert!(error.to_string().contains(&expected_uid.to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn private_runtime_directory_rejects_broad_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let private = temp.path().join("private");
        ensure_private_directory(&private).unwrap();
        assert_eq!(
            fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o700
        );

        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_directory(&private).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn process_record_round_trips_without_losing_identity() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("process.json");
        let record = ProcessRecord {
            schema_version: PROCESS_RECORD_SCHEMA_VERSION,
            pid: std::process::id(),
            process_group_id: read_process_group(std::process::id()).unwrap(),
            process_start_time: read_process_start_time(std::process::id()).unwrap(),
            boot_id: current_boot_id().unwrap(),
            codex_bin: PathBuf::from("/opt/codex"),
            codex_version: SUPPORTED_CODEX_VERSION.to_string(),
            codex_home: PathBuf::from("/tmp/codex-home"),
            endpoint: "unix:///tmp/codex.sock".to_string(),
        };
        write_process_record(&path, &record).unwrap();
        assert_eq!(read_active_process_record(&path).unwrap(), Some(record));
    }

    #[cfg(unix)]
    #[test]
    fn process_record_rejects_non_private_files() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("process.json");
        fs::write(&path, b"{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let error = read_active_process_record(&path).unwrap_err();
        assert!(error.to_string().contains("must be owned by uid"));
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_spawn_failure_terminates_a_ready_process_group() {
        use std::os::unix::process::CommandExt;

        let temp = TempDir::new().unwrap();
        let leader_path = temp.path().join("leader.pid");
        let descendant_path = temp.path().join("descendant.pid");
        let script = r#"
printf '%s' "$$" > "$CODEX_TAMER_TEST_LEADER_PID"
/bin/sleep 30 &
descendant=$!
printf '%s' "$descendant" > "$CODEX_TAMER_TEST_DESCENDANT_PID"
wait "$descendant"
"#;
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .env("CODEX_TAMER_TEST_LEADER_PID", &leader_path)
            .env("CODEX_TAMER_TEST_DESCENDANT_PID", &descendant_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let leader = wait_for_recorded_pid(&leader_path, "leader");
        let descendant = wait_for_recorded_pid(&descendant_path, "descendant");
        assert_eq!(leader, child.id());
        assert!(process_exists(leader).unwrap());
        assert!(process_exists(descendant).unwrap());

        let error = cleanup_spawn_failure(&mut child, anyhow!("forced process record failure"));

        assert!(error.to_string().contains("forced process record failure"));
        assert_process_exits(leader, "leader");
        assert_process_exits(descendant, "descendant");
    }

    #[cfg(unix)]
    #[test]
    fn stale_process_record_is_removed_only_after_identity_mismatch() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("process.json");
        let record = ProcessRecord {
            schema_version: PROCESS_RECORD_SCHEMA_VERSION,
            pid: std::process::id(),
            process_group_id: read_process_group(std::process::id()).unwrap(),
            process_start_time: "confirmed-stale-identity".to_string(),
            boot_id: current_boot_id().unwrap(),
            codex_bin: PathBuf::from("/opt/codex"),
            codex_version: SUPPORTED_CODEX_VERSION.to_string(),
            codex_home: PathBuf::from("/tmp/codex-home"),
            endpoint: "unix:///tmp/codex.sock".to_string(),
        };
        write_process_record(&path, &record).unwrap();

        assert_eq!(read_active_process_record(&path).unwrap(), None);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn listener_peer_must_belong_to_the_recorded_process_group() {
        let actual_group = read_process_group(std::process::id()).unwrap();
        let record = ProcessRecord {
            schema_version: PROCESS_RECORD_SCHEMA_VERSION,
            pid: std::process::id(),
            process_group_id: actual_group.saturating_add(1),
            process_start_time: read_process_start_time(std::process::id()).unwrap(),
            boot_id: current_boot_id().unwrap(),
            codex_bin: PathBuf::from("/opt/codex"),
            codex_version: SUPPORTED_CODEX_VERSION.to_string(),
            codex_home: PathBuf::from("/tmp/codex-home"),
            endpoint: "unix:///tmp/codex.sock".to_string(),
        };
        let peer = PeerIdentity {
            pid: Some(std::process::id()),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        };

        let error = validate_peer_identity(peer, &record).unwrap_err();
        assert!(error.to_string().contains("expected"));
        assert!(error.to_string().contains(&actual_group.to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn managed_listener_peer_must_belong_to_the_current_uid() {
        let expected_uid = unsafe { libc::geteuid() };
        let peer = PeerIdentity {
            pid: Some(std::process::id()),
            uid: expected_uid.saturating_add(1),
            gid: unsafe { libc::getegid() },
        };

        let error = validate_managed_peer_uid(Some(peer)).unwrap_err();

        assert!(error.to_string().contains(&expected_uid.to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_process_record_is_preserved_when_identity_cannot_be_verified() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("process.json");
        let legacy = serde_json::json!({
            "pid": std::process::id(),
            "processStartTime": "Sat Aug  8 16:32:35 2026",
            "codexBin": "/opt/codex",
            "codexVersion": SUPPORTED_CODEX_VERSION,
            "codexHome": "/tmp/codex-home",
            "endpoint": "unix:///tmp/codex.sock"
        });
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        fs::set_permissions(
            &path,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )
        .unwrap();

        let error = read_active_process_record(&path).unwrap_err();
        assert!(error.to_string().contains("legacy process record"));
        assert!(path.exists(), "unverifiable ownership evidence was deleted");
    }

    #[test]
    fn log_tail_handles_missing_files_and_bounds_output() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("stderr.log");
        assert_eq!(read_log_tail(&path).unwrap(), "");

        let mut content = vec![b'a'; LOG_TAIL_BYTES + 10];
        content.extend_from_slice(b" final message \n");
        fs::write(&path, content).unwrap();
        let tail = read_log_tail(&path).unwrap();
        assert!(tail.len() <= LOG_TAIL_BYTES);
        assert!(tail.ends_with("final message"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_waits_for_the_managed_lifecycle_lock() {
        let temp = TempDir::new().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        let codex_home = fs::canonicalize(codex_home).unwrap();
        let endpoint = Endpoint::Unix {
            path: temp.path().join("runtime/server/app-server.sock"),
        };
        let paths = ManagedPaths::new(&endpoint).unwrap();
        paths.prepare().unwrap();
        let lock = open_private_file(&paths.lock, false).unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);

        let mut config = AppConfig::default();
        config.managed.codex_home = Some(codex_home);
        let result =
            tokio::time::timeout(Duration::from_millis(250), stop(&config, &endpoint)).await;

        assert!(
            result.is_err(),
            "stop completed while another lifecycle operation held the lock"
        );
    }

    #[tokio::test]
    async fn startup_deadline_bounds_a_stalled_operation() {
        let deadline = Instant::now() + Duration::from_millis(25);
        let error = run_before_start_deadline(
            deadline,
            std::future::pending::<Result<()>>(),
            "test operation",
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("timed out during test operation")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_listener_handshake_respects_the_startup_deadline() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("app-server.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let endpoint = Endpoint::Unix { path: socket };
        let codex_home = temp.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        let accepted = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(stream);
        });
        let deadline = Instant::now() + Duration::from_millis(50);

        let error = match connect_existing(&endpoint, &codex_home, true, deadline).await {
            Ok(_) => panic!("stalled listener completed before the startup deadline"),
            Err(error) => error,
        };

        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("timed out during managed app-server handshake"),
            "unexpected error: {rendered}"
        );
        accepted.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn active_process_record_prevents_a_duplicate_start() {
        let temp = TempDir::new().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        let codex_home = fs::canonicalize(codex_home).unwrap();
        let endpoint = Endpoint::Unix {
            path: temp.path().join("runtime/server/app-server.sock"),
        };
        let paths = ManagedPaths::new(&endpoint).unwrap();
        paths.prepare().unwrap();
        let record = ProcessRecord {
            schema_version: PROCESS_RECORD_SCHEMA_VERSION,
            pid: std::process::id(),
            process_group_id: read_process_group(std::process::id()).unwrap(),
            process_start_time: read_process_start_time(std::process::id()).unwrap(),
            boot_id: current_boot_id().unwrap(),
            codex_bin: PathBuf::from("/missing/codex"),
            codex_version: SUPPORTED_CODEX_VERSION.to_string(),
            codex_home: codex_home.clone(),
            endpoint: endpoint.display(),
        };
        write_process_record(&paths.process, &record).unwrap();

        let mut config = AppConfig::default();
        config.managed.codex_home = Some(codex_home);
        config.managed.codex = Some(record.codex_bin.clone());
        let error = match connect_or_start(&config, &endpoint).await {
            Ok(_) => panic!("active process record allowed a duplicate start"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("is still running"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            read_active_process_record(&paths.process).unwrap(),
            Some(record)
        );
    }
}
