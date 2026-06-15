//! zaz - A modern file-watching task runner and process manager.

use anyhow::{bail, Result};
use clap::Parser;
use std::ffi::OsString;
use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::sync::mpsc;
use zaz::cli::{Cli, Commands};
use zaz_config::{load_user_config, TuiStylePreference, UserConfig};
use zaz_daemon::{
    resolve_socket, socket_path_for_config, ApiRequest, ApiResponse, Client, DaemonError, Engine,
    EngineCommand, Server,
};
use zaz_mcp::McpError;
use zaz_process::{DaemonLauncher, LaunchHandle};

/// Effective TUI options after merging CLI flags with user config.
#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub style: TuiStylePreference,
    pub no_autostart: bool,
    pub stop_on_exit: bool,
    pub disable_animations: bool,
}

impl TuiOptions {
    /// Create TUI options by merging CLI flags with user config.
    /// CLI flags take precedence over user config.
    fn from_cli_and_user_config(cli: &Cli, user_config: &UserConfig) -> Self {
        let style = if cli.full {
            TuiStylePreference::Full
        } else if cli.multi_pane {
            TuiStylePreference::MultiPane
        } else {
            user_config.tui_style.unwrap_or(TuiStylePreference::Full)
        };

        Self {
            style,
            no_autostart: cli.no_autostart || user_config.no_autostart,
            stop_on_exit: cli.stop_on_exit,
            disable_animations: user_config.disable_animations,
        }
    }
}

/// Initialize tracing with optional file logging.
///
/// Returns a guard that must be kept alive for the duration of the program
/// when file logging is enabled (to ensure logs are flushed).
fn init_tracing(
    debug: bool,
    is_tui_mode: bool,
    log_file: Option<&Path>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;

    let filter = if debug { "debug,globset=info" } else { "info" };
    let env_filter = tracing_subscriber::EnvFilter::new(filter);

    match (is_tui_mode, log_file) {
        // TUI mode with file logging: log to file only
        (true, Some(path)) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("Failed to open log file");
            let (non_blocking, guard) = tracing_appender::non_blocking(file);
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(non_blocking)
                .with_ansi(false)
                .init();
            Some(guard)
        }
        // Non-TUI mode with file logging: log to both console and file
        (false, Some(path)) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("Failed to open log file");
            let (non_blocking, guard) = tracing_appender::non_blocking(file);

            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false);

            let console_layer = tracing_subscriber::fmt::layer().with_target(false);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(console_layer)
                .with(file_layer)
                .init();
            Some(guard)
        }
        // Non-TUI mode without file logging: console only
        (false, None) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_target(false)
                .init();
            None
        }
        // Otherwise, no logging
        (true, None) => None,
    }
}

/// Initialize tracing for `zaz mcp` mode.
///
/// The MCP server uses stdout as its JSON-RPC channel, so log output must
/// never touch it. Console logs are written to stderr; an optional file
/// receives logs alongside.
fn init_tracing_stderr(
    debug: bool,
    log_file: Option<&Path>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;

    let filter = if debug { "debug,globset=info" } else { "info" };
    let env_filter = tracing_subscriber::EnvFilter::new(filter);

    match log_file {
        Some(path) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("Failed to open log file");
            let (non_blocking, guard) = tracing_appender::non_blocking(file);

            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false);

            let stderr_layer = tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(stderr_layer)
                .with(file_layer)
                .init();
            Some(guard)
        }
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .with_target(false)
                .init();
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DebugLogKind {
    Tui,
    Daemon,
    DaemonOutput,
}

impl DebugLogKind {
    fn filename(self) -> &'static str {
        match self {
            Self::Tui => "tui-debug.log",
            Self::Daemon => "daemon-debug.log",
            Self::DaemonOutput => "daemon-output.log",
        }
    }
}

const DEBUG_LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
const DEBUG_LOG_ROTATE_KEEP: usize = 5;
const DEBUG_LOG_DIR_BUDGET_BYTES: u64 = 200 * 1024 * 1024;

fn user_state_dir() -> PathBuf {
    if let Ok(xdg_state_home) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg_state_home).join("zaz");
    }

    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/state/zaz");
    }

    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from(format!("/tmp/zaz-{}", user))
}

fn ensure_log_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    Ok(())
}

fn default_debug_log_path(kind: DebugLogKind) -> Result<PathBuf> {
    let path = user_state_dir().join(kind.filename());
    ensure_log_parent_dir(&path)?;
    Ok(path)
}

fn derive_daemon_log_path(path: &Path) -> PathBuf {
    match path.extension() {
        Some(ext) => {
            let mut new_ext = OsString::from("daemon.");
            new_ext.push(ext);
            path.with_extension(new_ext)
        }
        None => path.with_extension("daemon.log"),
    }
}

fn derive_daemon_output_log_path(path: &Path) -> PathBuf {
    match path.extension() {
        Some(ext) => {
            let mut new_ext = OsString::from("daemon-output.");
            new_ext.push(ext);
            path.with_extension(new_ext)
        }
        None => path.with_extension("daemon-output.log"),
    }
}

fn resolve_tui_log_file(debug: bool, log_file: Option<&Path>) -> Result<Option<PathBuf>> {
    let path = match log_file {
        Some(path) => path.to_path_buf(),
        None if debug => default_debug_log_path(DebugLogKind::Tui)?,
        None => return Ok(None),
    };
    ensure_log_parent_dir(&path)?;
    Ok(Some(path))
}

fn resolve_autostart_daemon_log_file(
    debug: bool,
    log_file: Option<&Path>,
) -> Result<Option<PathBuf>> {
    if !debug {
        return Ok(None);
    }

    let path = match log_file {
        Some(path) => derive_daemon_log_path(path),
        None => default_debug_log_path(DebugLogKind::Daemon)?,
    };
    ensure_log_parent_dir(&path)?;
    Ok(Some(path))
}

fn resolve_autostart_daemon_output_log(log_file: Option<&Path>) -> Result<PathBuf> {
    let path = match log_file {
        Some(path) => derive_daemon_output_log_path(path),
        None => default_debug_log_path(DebugLogKind::DaemonOutput)?,
    };
    ensure_log_parent_dir(&path)?;
    Ok(path)
}

fn rotated_log_path(path: &Path, generation: usize) -> PathBuf {
    let file_name = path
        .file_name()
        .expect("log path should have a file name")
        .to_string_lossy();
    path.with_file_name(format!("{file_name}.{generation}"))
}

fn rotate_log_file(path: &Path) -> Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    if metadata.len() < DEBUG_LOG_ROTATE_BYTES {
        return Ok(());
    }

    let oldest = rotated_log_path(path, DEBUG_LOG_ROTATE_KEEP);
    if oldest.exists() {
        std::fs::remove_file(&oldest)?;
    }

    for generation in (1..DEBUG_LOG_ROTATE_KEEP).rev() {
        let src = rotated_log_path(path, generation);
        if src.exists() {
            std::fs::rename(&src, rotated_log_path(path, generation + 1))?;
        }
    }

    std::fs::rename(path, rotated_log_path(path, 1))?;
    Ok(())
}

fn rotated_generation_for(path: &Path, active_name: &str) -> Option<usize> {
    let file_name = path.file_name()?.to_str()?;
    let suffix = file_name.strip_prefix(active_name)?.strip_prefix('.')?;
    suffix.parse().ok()
}

fn prune_debug_log_dirs_with_budget(paths: &[PathBuf], budget_bytes: u64) -> Result<()> {
    use std::collections::{BTreeMap, HashSet};
    use std::time::SystemTime;

    let mut by_dir: BTreeMap<PathBuf, HashSet<String>> = BTreeMap::new();
    for path in paths {
        let Some(parent) = path.parent() else {
            continue;
        };
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        by_dir
            .entry(parent.to_path_buf())
            .or_default()
            .insert(file_name.to_string());
    }

    for (dir, active_names) in by_dir {
        let mut total_bytes = 0u64;
        let mut rotated_files = Vec::new();

        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = entry.metadata()?;
            total_bytes = total_bytes.saturating_add(metadata.len());

            if !metadata.is_file() {
                continue;
            }

            for active_name in &active_names {
                if let Some(generation) = rotated_generation_for(&path, active_name) {
                    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    rotated_files.push((modified, generation, metadata.len(), path.clone()));
                    break;
                }
            }
        }

        if total_bytes <= budget_bytes {
            continue;
        }

        rotated_files
            .sort_by_key(|(modified, generation, _, path)| (*modified, *generation, path.clone()));

        for (_, _, size, path) in rotated_files {
            if total_bytes <= budget_bytes {
                break;
            }
            std::fs::remove_file(&path)?;
            total_bytes = total_bytes.saturating_sub(size);
        }
    }

    Ok(())
}

fn prune_debug_log_dirs(paths: &[PathBuf]) -> Result<()> {
    prune_debug_log_dirs_with_budget(paths, DEBUG_LOG_DIR_BUDGET_BYTES)
}

fn prepare_log_files(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        ensure_log_parent_dir(path)?;
        rotate_log_file(path)?;
    }
    prune_debug_log_dirs(paths)?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let exit_code = match try_main().await {
        Ok(()) => 0,
        Err(err) if err.downcast_ref::<StatusNotRunning>().is_some() => 3,
        Err(err) => {
            report_error(&err);
            1
        }
    };

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

/// Walk an `anyhow::Error` chain and return the first structured recovery hint
/// it carries, if any. Recognized error types: [`DaemonError`], [`McpError`],
/// [`NoDaemon`], and `zaz_config::ValidationError`.
fn error_hint(err: &anyhow::Error) -> Option<String> {
    for cause in err.chain() {
        if let Some(de) = cause.downcast_ref::<DaemonError>() {
            if let Some(h) = de.hint() {
                return Some(h.to_string());
            }
        }
        if let Some(me) = cause.downcast_ref::<McpError>() {
            if let Some(h) = me.hint() {
                return Some(h.to_string());
            }
        }
        if let Some(nd) = cause.downcast_ref::<NoDaemon>() {
            return Some(nd.hint().to_string());
        }
    }
    None
}

/// Print an operator-facing error to stderr in the same shape as
/// `check_config`'s validation printer: an `Error: <message>` line, and a
/// `       hint: <recovery>` line indented underneath when the underlying
/// error type provides one. Labels are styled when stderr is a TTY and emitted
/// as plain text otherwise so the output remains substring-searchable for
/// scripted consumers.
fn report_error(err: &anyhow::Error) {
    use std::io::IsTerminal;
    use yansi::Paint;

    let hint = error_hint(err);
    if std::io::stderr().is_terminal() {
        eprintln!("{}: {}", "Error".red().bold(), err);
        if let Some(hint) = hint {
            eprintln!("       {}: {}", "hint".cyan().bold(), hint);
        }
    } else {
        eprintln!("Error: {err}");
        if let Some(hint) = hint {
            eprintln!("       hint: {hint}");
        }
    }
}

async fn try_main() -> Result<()> {
    let cli = Cli::parse();
    let current_dir = std::env::current_dir()?;
    let _log_guard;
    let explicit_log_paths = cli.log_file.iter().cloned().collect::<Vec<PathBuf>>();

    match cli.command {
        Some(Commands::Task) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            let config_path = find_config(&cli.config)?;
            run_tasks(&config_path).await
        }
        Some(Commands::Daemon { quiet }) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            let config_path = find_config(&cli.config)?;
            let socket_path =
                resolve_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            run_daemon(&config_path, &socket_path, quiet).await
        }
        Some(Commands::Start) => {
            let config_path = find_config(&cli.config)?;
            let socket_path =
                resolve_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            let daemon_log_file =
                resolve_autostart_daemon_log_file(cli.debug, cli.log_file.as_deref())?;
            let daemon_output_log = resolve_autostart_daemon_output_log(cli.log_file.as_deref())?;

            let mut log_paths = explicit_log_paths.clone();
            if let Some(path) = &daemon_log_file {
                log_paths.push(path.clone());
            }
            log_paths.push(daemon_output_log.clone());
            prepare_log_files(&log_paths)?;

            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());

            start_daemon_command(
                &config_path,
                &socket_path,
                cli.debug,
                daemon_log_file.as_deref(),
                &daemon_output_log,
            )
            .await
        }
        Some(Commands::Status) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            let socket_path =
                resolve_control_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            show_status(&socket_path).await
        }
        Some(Commands::Restart { group }) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            let socket_path =
                resolve_control_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            restart(&socket_path, group).await
        }
        Some(Commands::Stop) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            let socket_path =
                resolve_control_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            stop_daemon(&socket_path).await
        }
        Some(Commands::Reload) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            let socket_path =
                resolve_control_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            reload_config(&socket_path).await
        }
        Some(Commands::Check { config, json }) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            let config_path = find_config(&config.or(cli.config.clone()))?;
            check_config(&config_path, json)
        }
        Some(Commands::Ignores) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            show_ignores()
        }
        Some(Commands::Mcp { autostart }) => {
            let socket_path =
                resolve_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;

            if autostart {
                let config_path = find_config(&cli.config)?;
                let daemon_log_file =
                    resolve_autostart_daemon_log_file(cli.debug, cli.log_file.as_deref())?;
                let daemon_output_log =
                    resolve_autostart_daemon_output_log(cli.log_file.as_deref())?;

                let mut log_paths = explicit_log_paths.clone();
                if let Some(path) = &daemon_log_file {
                    log_paths.push(path.clone());
                }
                log_paths.push(daemon_output_log.clone());
                prepare_log_files(&log_paths)?;

                _log_guard = init_tracing_stderr(cli.debug, cli.log_file.as_deref());

                let check_timeout = Duration::from_secs(1);
                if !matches!(
                    check_daemon_availability(&socket_path, check_timeout).await,
                    DaemonAvailability::Running
                ) {
                    tracing::info!(
                        socket = %socket_path.display(),
                        daemon_output_log = %daemon_output_log.display(),
                        "auto-starting daemon for MCP session"
                    );
                    let mut handle = start_daemon_via_launcher(
                        &config_path,
                        &socket_path,
                        cli.debug,
                        daemon_log_file.as_deref(),
                        &daemon_output_log,
                    )?;
                    match wait_for_daemon_ready(
                        &socket_path,
                        &mut handle,
                        20,
                        Duration::from_millis(100),
                    )
                    .await?
                    {
                        DaemonReadyOutcome::Ready => {}
                        DaemonReadyOutcome::Crashed(status) => bail!(
                            "daemon exited before becoming ready (status: {}); see {} for details",
                            status,
                            daemon_output_log.display()
                        ),
                        DaemonReadyOutcome::Timeout => bail!(
                            "daemon did not become ready within 2s; see {} for details",
                            daemon_output_log.display()
                        ),
                    }
                }
            } else {
                prepare_log_files(&explicit_log_paths)?;
                _log_guard = init_tracing_stderr(cli.debug, cli.log_file.as_deref());
            }

            zaz_mcp::run(zaz_mcp::McpRunOptions {
                cwd: current_dir.clone(),
                socket_path,
                explicit_config: cli.config.clone(),
            })
            .await
            .map_err(anyhow::Error::from)
        }
        Some(Commands::Completions { shell }) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            generate_completions(shell)
        }
        Some(Commands::Man { command }) => {
            prepare_log_files(&explicit_log_paths)?;
            _log_guard = init_tracing(cli.debug, false, cli.log_file.as_deref());
            generate_man(command.as_deref())
        }
        None => {
            let config_path = find_config(&cli.config)?;
            let socket_path =
                resolve_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            let tui_log_file = resolve_tui_log_file(cli.debug, cli.log_file.as_deref())?;
            let daemon_log_file =
                resolve_autostart_daemon_log_file(cli.debug, cli.log_file.as_deref())?;
            let daemon_output_log = resolve_autostart_daemon_output_log(cli.log_file.as_deref())?;
            let mut log_paths = Vec::new();
            if let Some(path) = &tui_log_file {
                log_paths.push(path.clone());
            }
            if let Some(path) = &daemon_log_file {
                log_paths.push(path.clone());
            }
            log_paths.push(daemon_output_log.clone());
            prepare_log_files(&log_paths)?;
            _log_guard = init_tracing(cli.debug, true, tui_log_file.as_deref());
            let user_config = load_user_config();
            let tui_options = TuiOptions::from_cli_and_user_config(&cli, &user_config);
            run_tui(
                &config_path,
                &socket_path,
                &tui_options,
                cli.debug,
                daemon_log_file.as_deref(),
                &daemon_output_log,
            )
            .await
        }
    }
}

#[derive(Debug)]
struct StatusNotRunning;

impl fmt::Display for StatusNotRunning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("daemon not running")
    }
}

impl std::error::Error for StatusNotRunning {}

/// Operator-facing error raised when a daemon-API verb cannot reach the daemon
/// because no socket is listening. Carries the socket path that was tried and
/// exposes a structured recovery hint for the top-level error printer.
#[derive(Debug, Clone)]
struct NoDaemon {
    socket_path: PathBuf,
}

impl NoDaemon {
    fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
        }
    }

    fn hint(&self) -> &'static str {
        "start a daemon with `zaz start`, or set --socket <PATH> if the daemon is running on a different socket"
    }
}

impl fmt::Display for NoDaemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no daemon running at {}", self.socket_path.display())
    }
}

impl std::error::Error for NoDaemon {}

/// Connect to the daemon at `socket_path`. On connection failure, returns a
/// `NoDaemon` error wrapped via `anyhow::Error` so the top-level printer can
/// surface its recovery hint.
async fn connect_or_no_daemon(socket_path: &Path) -> Result<Client> {
    Client::connect(socket_path)
        .await
        .map_err(|_| anyhow::Error::from(NoDaemon::new(socket_path)))
}

/// Render the daemon's response to a control-plane verb: print the success
/// message on the `Ok` arm, fail with `"{verb} failed: {message}"` on the
/// `Error` arm, and fail with `"{verb} returned unexpected response"` for any
/// other shape. `default_ok` is used when `ApiResponse::Ok` carries no message.
fn handle_daemon_response(response: ApiResponse, verb: &str, default_ok: &str) -> Result<()> {
    match response {
        ApiResponse::Ok { message } => {
            println!("{}", message.unwrap_or_else(|| default_ok.to_string()));
            Ok(())
        }
        ApiResponse::Error { message } => {
            bail!("{verb} failed: {message}");
        }
        _ => {
            bail!("{verb} returned unexpected response");
        }
    }
}

/// Find the configuration file.
fn find_config(explicit: &Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if path.exists() {
            return Ok(path.clone());
        }
        anyhow::bail!("config file not found: {}", path.display());
    }

    // Try to discover config file
    match zaz_config::discover() {
        Ok((path, _)) => Ok(path),
        Err(e) => anyhow::bail!("no config file found: {}", e),
    }
}

fn resolve_control_command_socket(
    explicit_config: &Option<PathBuf>,
    explicit_socket: Option<PathBuf>,
    start_dir: &Path,
) -> Result<PathBuf> {
    resolve_command_socket(explicit_config, explicit_socket, start_dir)
}

fn resolve_command_socket(
    explicit_config: &Option<PathBuf>,
    explicit_socket: Option<PathBuf>,
    start_dir: &Path,
) -> Result<PathBuf> {
    if let Some(socket_path) = explicit_socket {
        return Ok(socket_path);
    }

    if explicit_config.is_some() {
        let config_path = find_config(explicit_config)?;
        return Ok(socket_path_for_config(&config_path));
    }

    Ok(resolve_socket(None, start_dir)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonAvailability {
    Running,
    StaleSocket,
    Unreachable,
}

async fn run_tasks(config_path: &Path) -> Result<()> {
    tracing::info!(config = %config_path.display(), "running task commands");

    let mut engine = Engine::new_task_only(config_path)?;
    engine.startup().await?;
    let success = engine.wait_for_tasks().await;

    // Shutdown still performs normal cleanup, even though task-only mode never starts daemons.
    engine.shutdown().await?;

    if !success {
        bail!("one or more tasks failed");
    }

    tracing::info!("all tasks completed");
    Ok(())
}

async fn run_daemon(config_path: &Path, socket_path: &Path, quiet: bool) -> Result<()> {
    // Check if a daemon is already running by trying to connect and send a request
    let check_timeout = Duration::from_secs(1);
    match tokio::time::timeout(check_timeout, Client::connect(socket_path)).await {
        Ok(Ok(mut client)) => {
            // Try to actually communicate with the daemon
            match tokio::time::timeout(check_timeout, client.request(&ApiRequest::Status)).await {
                Ok(Ok(_)) => {
                    anyhow::bail!(
                        "daemon already running (socket {} is active)",
                        socket_path.display()
                    );
                }
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "stale socket (request failed), will replace");
                }
                Err(_) => {
                    tracing::debug!("stale socket (request timed out), will replace");
                }
            }
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "no existing daemon (connect failed)");
        }
        Err(_) => {
            tracing::debug!("no existing daemon (connect timed out)");
        }
    }

    tracing::info!(config = %config_path.display(), "starting daemon");

    let mut engine = Engine::with_options(config_path, !quiet)?;
    let (command_tx, mut command_rx) = mpsc::channel::<EngineCommand>(32);
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    // Start API server
    let server = Server::bind(socket_path, command_tx, shutdown_tx).await?;
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            tracing::error!(error = %e, "server error");
        }
    });

    // Run initial startup, but don't exit on failure
    if let Err(e) = engine.startup().await {
        tracing::error!(error = %e, "initial startup failed, waiting for file changes to retry");
    }

    tracing::info!("watching for file changes...");
    let mut shutdown_requested = false;
    loop {
        tokio::select! {
            // Handle Ctrl+C
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received shutdown signal");
                break;
            }

            // Handle API commands
            Some(cmd) = command_rx.recv() => {
                // Drain log channel before handling request (ensures GetLogs returns fresh data)
                engine.process_incoming_logs();

                let response = engine.handle_request(cmd.request).await;
                let _ = cmd.response_tx.send(response);
            }

            // Tear down once a Shutdown response has been written to its client
            _ = shutdown_rx.recv() => {
                shutdown_requested = true;
                break;
            }

            // Poll for file changes and check daemons
            _ = async {
                // Process incoming logs from PTY readers
                engine.process_incoming_logs();

                if let Err(e) = engine.poll().await {
                    tracing::error!(error = %e, "poll error");
                }
                if let Err(e) = engine.check_daemons().await {
                    tracing::error!(error = %e, "daemon check error");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            } => {}
        }
    }

    // Cleanup
    server_handle.abort();
    engine.shutdown().await?;

    if shutdown_requested {
        tracing::info!("shutdown complete");
    }

    Ok(())
}

async fn start_daemon_command(
    config_path: &Path,
    socket_path: &Path,
    debug: bool,
    daemon_log_file: Option<&Path>,
    daemon_output_log: &Path,
) -> Result<()> {
    let check_timeout = Duration::from_secs(1);
    if matches!(
        check_daemon_availability(socket_path, check_timeout).await,
        DaemonAvailability::Running
    ) {
        println!("daemon already running (socket {})", socket_path.display());
        return Ok(());
    }

    tracing::info!(
        config = %config_path.display(),
        socket = %socket_path.display(),
        daemon_output_log = %daemon_output_log.display(),
        "starting daemon in background"
    );

    let mut handle = start_daemon_via_launcher(
        config_path,
        socket_path,
        debug,
        daemon_log_file,
        daemon_output_log,
    )?;

    match wait_for_daemon_ready(socket_path, &mut handle, 20, Duration::from_millis(100)).await? {
        DaemonReadyOutcome::Ready => {
            println!("daemon started (pid {})", handle.id());
            Ok(())
        }
        DaemonReadyOutcome::Crashed(status) => bail!(
            "daemon exited before becoming ready (status: {}); see {} for details",
            status,
            daemon_output_log.display()
        ),
        DaemonReadyOutcome::Timeout => bail!(
            "daemon did not become ready within 2s; see {} for details",
            daemon_output_log.display()
        ),
    }
}

async fn show_status(socket_path: &Path) -> Result<()> {
    let mut client = match Client::connect(socket_path).await {
        Ok(c) => c,
        Err(_) => {
            // Status's "no daemon" path bypasses the top-level error printer so
            // the message lands on stdout (per ZAZ-005 query-command semantics)
            // and the process exits 3 via `StatusNotRunning`. Render the same
            // bare message + indented hint shape the printer produces for
            // stderr-routed errors.
            let no_daemon = NoDaemon::new(socket_path);
            println!("{}", no_daemon);
            println!("       hint: {}", no_daemon.hint());
            return Err(StatusNotRunning.into());
        }
    };

    let response = client.request(&ApiRequest::Status).await?;
    match response {
        ApiResponse::Status { state } => {
            println!("Daemon Status: {:?}", state.status);
            println!("Groups:");
            for (name, group) in &state.groups {
                println!("  {} ({:?})", name, group.status);
                for task in &group.tasks {
                    let duration = task
                        .duration_ms
                        .map(|d| format!(" ({}ms)", d))
                        .unwrap_or_default();
                    println!("    [task] {} - {:?}{}", task.name, task.status, duration);
                }
                for daemon in &group.daemons {
                    let pid = daemon
                        .pid
                        .map(|p| format!(" (pid {})", p))
                        .unwrap_or_default();
                    println!("    [daemon] {} - {:?}{}", daemon.name, daemon.status, pid);
                }
            }
            if let Some(ts) = state.last_change {
                println!("Last change: {} ms ago", now_ms() - ts);
            }
            Ok(())
        }
        other => handle_daemon_response(other, "status request", ""),
    }
}

async fn restart(socket_path: &Path, group: Option<String>) -> Result<()> {
    let mut client = connect_or_no_daemon(socket_path).await?;

    let request = match group {
        Some(name) => ApiRequest::RestartGroup { name },
        None => ApiRequest::RestartAll,
    };

    let response = client.request(&request).await?;
    handle_daemon_response(response, "restart", "OK")
}

async fn stop_daemon(socket_path: &Path) -> Result<()> {
    let mut client = match Client::connect(socket_path).await {
        Ok(c) => c,
        Err(_) => {
            // Idempotent: a stop that finds no daemon exits 0.
            println!("No daemon running");
            return Ok(());
        }
    };

    let response = client.request(&ApiRequest::Shutdown).await?;
    handle_daemon_response(response, "stop", "Shutdown initiated")
}

async fn reload_config(socket_path: &Path) -> Result<()> {
    let mut client = connect_or_no_daemon(socket_path).await?;
    let response = client.request(&ApiRequest::ReloadConfig).await?;
    handle_daemon_response(response, "reload", "Configuration reloaded")
}

fn check_config(config_path: &Path, json_output: bool) -> Result<()> {
    use serde::Serialize;
    use zaz_config::{ConfigError, ValidationErrors};

    #[derive(Serialize)]
    struct CheckResult {
        valid: bool,
        path: String,
        errors: Vec<CheckError>,
    }

    #[derive(Serialize)]
    struct CheckError {
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        column: Option<usize>,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
        code: String,
    }

    // Helper to convert validation errors to CheckErrors
    fn validation_errors_to_check_errors(errors: &ValidationErrors) -> Vec<CheckError> {
        errors
            .iter()
            .map(|e| CheckError {
                line: e.span.as_ref().map(|s| s.line),
                column: e.span.as_ref().map(|s| s.column),
                message: e.kind.to_string(),
                hint: e.hint.clone(),
                code: e.code().to_string(),
            })
            .collect()
    }

    fn check_failed(message: &str) -> Result<()> {
        anyhow::bail!(message.to_string())
    }

    let result = zaz_config::load(config_path);
    match result {
        Ok(_config) => {
            if json_output {
                let result = CheckResult {
                    valid: true,
                    path: config_path.display().to_string(),
                    errors: vec![],
                };

                println!("{}", serde_json::to_string(&result)?);
                return Ok(());
            }

            use yansi::Paint;
            println!("{}: {}", config_path.display(), "OK".green().bold());
            Ok(())
        }

        Err(ConfigError::Validation(ref validation_errors)) => {
            use yansi::Paint;

            let error_count = validation_errors.len();

            if json_output {
                let result = CheckResult {
                    valid: false,
                    path: config_path.display().to_string(),
                    errors: validation_errors_to_check_errors(validation_errors),
                };

                println!("{}", serde_json::to_string(&result)?);
                return check_failed("configuration validation failed");
            }

            // Pretty print each error with colors
            for error in validation_errors.iter() {
                // Format: "path:line:column: error: message" or "path: error: message"
                if let Some(span) = &error.span {
                    eprint!(
                        "{}:{}:{}: ",
                        config_path.display().bold(),
                        span.line.cyan(),
                        span.column.cyan()
                    );
                } else {
                    eprint!("{}: ", config_path.display().bold());
                }
                eprintln!("{}: {}", "error".red().bold(), error.kind);

                if let Some(hint) = &error.hint {
                    eprintln!("               {}: {}", "hint".cyan().bold(), hint);
                }
                eprintln!();
            }

            let plural = if error_count == 1 { "error" } else { "errors" };
            eprintln!(
                "{} {} {} in {}",
                "Found".red().bold(),
                error_count.red().bold(),
                plural.red().bold(),
                config_path.display().bold()
            );
            check_failed("configuration validation failed")
        }

        Err(e) => {
            use yansi::Paint;

            // Non-validation errors (parse errors, IO errors, etc.)
            if json_output {
                let result = CheckResult {
                    valid: false,
                    path: config_path.display().to_string(),
                    errors: vec![CheckError {
                        line: None,
                        column: None,
                        message: e.to_string(),
                        hint: None,
                        code: "parse_error".to_string(),
                    }],
                };

                println!("{}", serde_json::to_string(&result)?);
                return check_failed("configuration parse failed");
            }

            eprintln!(
                "{}: {}: {}",
                config_path.display().bold(),
                "error".red().bold(),
                e
            );
            eprintln!(
                "\n{} in {}",
                "Found 1 error".red().bold(),
                config_path.display().bold()
            );
            check_failed("configuration parse failed")
        }
    }
}

fn show_ignores() -> Result<()> {
    println!("Default ignore patterns:");
    for pattern in zaz_watch::default_ignores() {
        println!("  {}", pattern);
    }

    Ok(())
}

fn generate_completions(shell: clap_complete::Shell) -> Result<()> {
    use clap::CommandFactory;

    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
    Ok(())
}

fn generate_man(command: Option<&str>) -> Result<()> {
    use clap::CommandFactory;

    let root = Cli::command();
    let bin_name = root.get_name().to_string();
    let (cmd, title) = match command {
        None => (root, bin_name),
        Some(name) => {
            let title = format!("{bin_name}-{name}");
            let Some(sub) = root.find_subcommand(name) else {
                bail!("unknown subcommand: {name}");
            };
            (sub.clone(), title)
        }
    };

    clap_mangen::Man::new(cmd)
        .title(title.to_uppercase())
        .render(&mut std::io::stdout())?;
    Ok(())
}

async fn is_daemon_responsive(socket_path: &Path, timeout: Duration) -> bool {
    matches!(
        check_daemon_availability(socket_path, timeout).await,
        DaemonAvailability::Running
    )
}

async fn check_daemon_availability(socket_path: &Path, timeout: Duration) -> DaemonAvailability {
    match tokio::time::timeout(timeout, Client::connect(socket_path)).await {
        Ok(Ok(mut client)) => {
            match tokio::time::timeout(timeout, client.request(&ApiRequest::Status)).await {
                Ok(Ok(_)) => DaemonAvailability::Running,
                Ok(Err(e)) => {
                    tracing::debug!(
                        socket = %socket_path.display(),
                        error = %e,
                        "socket accepted connection but daemon status request failed"
                    );
                    DaemonAvailability::StaleSocket
                }
                Err(_) => {
                    tracing::debug!(
                        socket = %socket_path.display(),
                        timeout_ms = timeout.as_millis(),
                        "socket accepted connection but daemon status request timed out"
                    );
                    DaemonAvailability::StaleSocket
                }
            }
        }
        Ok(Err(e)) => {
            tracing::debug!(
                socket = %socket_path.display(),
                error = %e,
                "failed to connect to daemon socket"
            );
            DaemonAvailability::Unreachable
        }
        Err(_) => {
            tracing::debug!(
                socket = %socket_path.display(),
                timeout_ms = timeout.as_millis(),
                "timed out connecting to daemon socket"
            );
            DaemonAvailability::Unreachable
        }
    }
}

fn resolve_autostart_executable() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_zaz") {
        return Ok(PathBuf::from(path));
    }

    let current_exe = std::env::current_exe()?;
    if let Some(deps_dir) = current_exe.parent() {
        if deps_dir.file_name().and_then(|name| name.to_str()) == Some("deps") {
            if let Some(target_dir) = deps_dir.parent() {
                let candidate = target_dir.join(env!("CARGO_PKG_NAME"));
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    Ok(current_exe)
}

fn build_daemon_args(
    config_path: &Path,
    socket_path: &Path,
    debug: bool,
    log_file: Option<&Path>,
) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = Vec::new();

    args.push(OsString::from("--config"));
    args.push(OsString::from(config_path.to_str().ok_or_else(|| {
        anyhow::anyhow!("config path is not valid utf-8")
    })?));
    args.push(OsString::from("--socket"));
    args.push(OsString::from(socket_path.to_str().ok_or_else(|| {
        anyhow::anyhow!("socket path is not valid utf-8")
    })?));

    if debug {
        args.push(OsString::from("--debug"));
    }

    if let Some(path) = log_file {
        args.push(OsString::from("--log-file"));
        args.push(OsString::from(path.to_str().ok_or_else(|| {
            anyhow::anyhow!("log file path is not valid utf-8")
        })?));
    }

    args.push(OsString::from("daemon"));
    args.push(OsString::from("--quiet"));

    Ok(args)
}

fn start_daemon_via_launcher(
    config_path: &Path,
    socket_path: &Path,
    debug: bool,
    log_file: Option<&Path>,
    output_log: &Path,
) -> Result<LaunchHandle> {
    let exe = resolve_autostart_executable()?;
    let args = build_daemon_args(config_path, socket_path, debug, log_file)?;
    let mut launcher = DaemonLauncher::new(exe, output_log);
    launcher.args(args);
    Ok(launcher.launch()?)
}

#[derive(Debug)]
enum DaemonReadyOutcome {
    Ready,
    Crashed(ExitStatus),
    Timeout,
}

async fn wait_for_daemon_ready(
    socket_path: &Path,
    handle: &mut LaunchHandle,
    attempts: usize,
    delay: Duration,
) -> Result<DaemonReadyOutcome> {
    for _ in 0..attempts {
        tokio::time::sleep(delay).await;
        if let Some(status) = handle.try_wait()? {
            return Ok(DaemonReadyOutcome::Crashed(status));
        }
        if is_daemon_responsive(socket_path, Duration::from_secs(1)).await {
            return Ok(DaemonReadyOutcome::Ready);
        }
    }

    Ok(DaemonReadyOutcome::Timeout)
}

async fn run_tui(
    config_path: &Path,
    socket_path: &Path,
    options: &TuiOptions,
    debug: bool,
    daemon_log_file: Option<&Path>,
    daemon_output_log: &Path,
) -> Result<()> {
    tracing::info!(
        config = %config_path.display(),
        style = ?options.style,
        "starting TUI"
    );
    tracing::debug!(
        config = %config_path.display(),
        socket = %socket_path.display(),
        no_autostart = options.no_autostart,
        stop_on_exit = options.stop_on_exit,
        disable_animations = options.disable_animations,
        daemon_log_file = daemon_log_file.map(|path| path.display().to_string()),
        daemon_output_log = %daemon_output_log.display(),
        "resolved TUI startup options"
    );

    let check_timeout = Duration::from_secs(1);
    let availability = check_daemon_availability(socket_path, check_timeout).await;
    let daemon_running = availability == DaemonAvailability::Running;
    match availability {
        DaemonAvailability::Running => {
            tracing::debug!(
                socket = %socket_path.display(),
                "reusing existing responsive daemon"
            );
        }
        DaemonAvailability::StaleSocket => {
            tracing::debug!(
                socket = %socket_path.display(),
                "daemon socket exists but is not serving requests"
            );
        }
        DaemonAvailability::Unreachable => {
            tracing::debug!(
                socket = %socket_path.display(),
                "no responsive daemon reachable on resolved socket"
            );
        }
    }

    if !daemon_running && !options.no_autostart {
        tracing::info!(
            socket = %socket_path.display(),
            daemon_log_file = daemon_log_file.map(|path| path.display().to_string()),
            daemon_output_log = %daemon_output_log.display(),
            "auto-starting daemon in background"
        );

        match start_daemon_via_launcher(
            config_path,
            socket_path,
            debug,
            daemon_log_file,
            daemon_output_log,
        ) {
            Ok(mut handle) => {
                tracing::debug!(
                    socket = %socket_path.display(),
                    pid = handle.id(),
                    "daemon subprocess spawned, waiting for readiness"
                );
                match wait_for_daemon_ready(
                    socket_path,
                    &mut handle,
                    20,
                    Duration::from_millis(100),
                )
                .await
                {
                    Ok(DaemonReadyOutcome::Ready) => {
                        tracing::debug!(
                            socket = %socket_path.display(),
                            "auto-started daemon became responsive"
                        );
                    }
                    Ok(DaemonReadyOutcome::Crashed(status)) => {
                        tracing::warn!(
                            socket = %socket_path.display(),
                            exit_status = %status,
                            output_log = %daemon_output_log.display(),
                            "auto-started daemon exited before becoming responsive"
                        );
                    }
                    Ok(DaemonReadyOutcome::Timeout) => {
                        tracing::warn!(
                            socket = %socket_path.display(),
                            output_log = %daemon_output_log.display(),
                            "auto-started daemon may not be ready after 2s"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            socket = %socket_path.display(),
                            error = %e,
                            "error while waiting for auto-started daemon"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    socket = %socket_path.display(),
                    error = %e,
                    "failed to auto-start daemon"
                );
            }
        }
    } else if !daemon_running {
        tracing::debug!(
            socket = %socket_path.display(),
            "daemon autostart skipped because no_autostart is enabled"
        );
    }

    // Create TUI app
    use zaz_tui::{App, TuiStyle};
    let style = TuiStyle::from(options.style);
    let user_config = zaz_config::UserConfig {
        no_autostart: options.no_autostart,
        disable_animations: options.disable_animations,
        tui_style: Some(options.style),
        log_colors: zaz_config::LogColorConfig::default(),
        notifications: zaz_config::NotificationConfig::default(),
        log_storage: zaz_config::LogStorageConfig::default(),
    };

    let config_name = config_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "zaz.toml".to_string());

    let mut app = App::new(style, user_config, config_name);
    app.stop_on_exit = options.stop_on_exit;

    // Connect to daemon
    tracing::debug!(socket = %socket_path.display(), "connecting TUI app to daemon");
    if let Err(e) = app.connect(socket_path).await {
        tracing::warn!(
            socket = %socket_path.display(),
            error = %e,
            "failed to connect TUI app to daemon"
        );
    }

    // Run TUI
    let result = app.run();
    match &result {
        Ok(()) => tracing::debug!("TUI exited cleanly"),
        Err(e) => tracing::debug!(error = %e, "TUI exited with error"),
    }
    result?;

    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;
    use zaz_daemon::{socket_path_for_config, ApiRequest, ApiResponse, Client, DaemonError};

    fn parse_cli(args: &[&str]) -> Cli {
        Cli::parse_from(args)
    }

    #[test]
    fn tui_options_stop_on_exit_defaults_false() {
        let cli = parse_cli(&["zaz"]);
        let options = TuiOptions::from_cli_and_user_config(&cli, &UserConfig::default());

        assert!(!options.stop_on_exit);
        assert!(!options.no_autostart);
    }

    #[test]
    fn no_daemon_display_includes_socket_path() {
        let nd = NoDaemon::new(Path::new("/tmp/zaz.sock"));
        assert_eq!(nd.to_string(), "no daemon running at /tmp/zaz.sock");
    }

    #[test]
    fn no_daemon_hint_points_at_zaz_start_and_socket_flag() {
        let nd = NoDaemon::new(Path::new("/tmp/zaz.sock"));
        assert_eq!(
            nd.hint(),
            "start a daemon with `zaz start`, or set --socket <PATH> if the daemon is running on a different socket"
        );
    }

    #[test]
    fn error_hint_surfaces_daemon_error_recovery() {
        let err: anyhow::Error = DaemonError::SocketResolution {
            start_dir: PathBuf::from("/tmp/outside"),
        }
        .into();
        assert_eq!(
            error_hint(&err).as_deref(),
            Some("run this command from a zaz project directory or pass --socket <PATH>")
        );
    }

    #[test]
    fn error_hint_surfaces_no_daemon_recovery() {
        let err: anyhow::Error = NoDaemon::new(Path::new("/tmp/zaz.sock")).into();
        assert_eq!(
            error_hint(&err).as_deref(),
            Some("start a daemon with `zaz start`, or set --socket <PATH> if the daemon is running on a different socket")
        );
    }

    #[test]
    fn error_hint_returns_none_when_no_structured_error_in_chain() {
        let err = anyhow::anyhow!("some plain error");
        assert_eq!(error_hint(&err), None);
    }

    #[test]
    fn handle_daemon_response_ok_prints_message_and_succeeds() {
        let result = handle_daemon_response(
            ApiResponse::Ok {
                message: Some("done".into()),
            },
            "restart",
            "OK",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn handle_daemon_response_error_uses_verb_failed_template() {
        let result = handle_daemon_response(
            ApiResponse::Error {
                message: "group 'foo' not found".into(),
            },
            "restart",
            "OK",
        );
        let err = result.expect_err("error arm should bail");
        assert_eq!(err.to_string(), "restart failed: group 'foo' not found");
    }

    #[test]
    fn handle_daemon_response_unexpected_uses_verb_returned_unexpected_template() {
        let result =
            handle_daemon_response(ApiResponse::EndOfStream, "reload", "Configuration reloaded");
        let err = result.expect_err("unexpected arm should bail");
        assert_eq!(err.to_string(), "reload returned unexpected response");
    }

    #[test]
    fn tui_options_include_cli_stop_on_exit_flag() {
        let cli = parse_cli(&["zaz", "--stop-on-exit"]);
        let options = TuiOptions::from_cli_and_user_config(&cli, &UserConfig::default());

        assert!(options.stop_on_exit);
    }

    #[test]
    fn derive_daemon_log_path_keeps_log_alongside_explicit_tui_log() {
        let path = Path::new("/tmp/zaz.log");
        assert_eq!(
            derive_daemon_log_path(path),
            PathBuf::from("/tmp/zaz.daemon.log")
        );
    }

    #[test]
    fn default_debug_log_path_uses_state_dir_filenames() -> Result<()> {
        let tui_path = default_debug_log_path(DebugLogKind::Tui)?;
        let daemon_path = default_debug_log_path(DebugLogKind::Daemon)?;
        let daemon_output_path = default_debug_log_path(DebugLogKind::DaemonOutput)?;

        assert!(tui_path.ends_with("zaz/tui-debug.log"));
        assert!(daemon_path.ends_with("zaz/daemon-debug.log"));
        assert!(daemon_output_path.ends_with("zaz/daemon-output.log"));
        Ok(())
    }

    #[test]
    fn resolve_tui_log_file_defaults_in_debug_tui_mode() -> Result<()> {
        let path = resolve_tui_log_file(true, None)?.expect("debug log path");
        assert!(path.ends_with("tui-debug.log"));
        Ok(())
    }

    #[test]
    fn resolve_tui_log_file_keeps_explicit_path_without_debug() -> Result<()> {
        let path = resolve_tui_log_file(false, Some(Path::new("/tmp/custom.log")))?
            .expect("explicit log path");
        assert_eq!(path, PathBuf::from("/tmp/custom.log"));
        Ok(())
    }

    #[test]
    fn resolve_autostart_daemon_log_file_uses_sibling_of_explicit_path() -> Result<()> {
        let path = resolve_autostart_daemon_log_file(true, Some(Path::new("/tmp/custom.log")))?
            .expect("daemon log path");
        assert_eq!(path, PathBuf::from("/tmp/custom.daemon.log"));
        Ok(())
    }

    #[test]
    fn derive_daemon_output_log_path_uses_sibling_of_explicit_path() {
        assert_eq!(
            derive_daemon_output_log_path(Path::new("/tmp/zaz.log")),
            PathBuf::from("/tmp/zaz.daemon-output.log")
        );
        assert_eq!(
            derive_daemon_output_log_path(Path::new("/tmp/zaz")),
            PathBuf::from("/tmp/zaz.daemon-output.log")
        );
    }

    #[test]
    fn rotate_log_file_shifts_generations() -> Result<()> {
        let temp = TempDir::new()?;
        let path = temp.path().join("tui-debug.log");
        std::fs::write(&path, vec![b'x'; DEBUG_LOG_ROTATE_BYTES as usize])?;
        std::fs::write(rotated_log_path(&path, 1), "old-1")?;
        std::fs::write(rotated_log_path(&path, 2), "old-2")?;

        rotate_log_file(&path)?;

        assert!(!path.exists());
        assert_eq!(
            std::fs::metadata(rotated_log_path(&path, 1))?.len(),
            DEBUG_LOG_ROTATE_BYTES
        );
        assert_eq!(
            std::fs::read_to_string(rotated_log_path(&path, 2))?,
            "old-1"
        );
        assert_eq!(
            std::fs::read_to_string(rotated_log_path(&path, 3))?,
            "old-2"
        );
        Ok(())
    }

    #[test]
    fn prune_debug_log_dirs_deletes_oldest_rotated_files_first() -> Result<()> {
        let temp = TempDir::new()?;
        let active = temp.path().join("tui-debug.log");
        let rotated_old = rotated_log_path(&active, 1);
        let rotated_new = rotated_log_path(&active, 2);

        std::fs::write(&active, "active")?;
        std::fs::write(&rotated_old, vec![b'a'; 120])?;
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&rotated_new, vec![b'b'; 120])?;

        prune_debug_log_dirs_with_budget(std::slice::from_ref(&active), 200)?;

        assert!(!rotated_old.exists());
        assert!(rotated_new.exists());
        assert!(active.exists());
        Ok(())
    }

    #[test]
    fn build_daemon_args_propagates_debug_only_when_enabled() -> Result<()> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("zaz.toml");
        let socket_path = temp.path().join("daemon.sock");
        let log_path = temp.path().join("daemon.log");
        std::fs::write(&config_path, "")?;

        let args = build_daemon_args(&config_path, &socket_path, true, Some(&log_path))?;
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(args.contains(&"--debug".to_string()));
        let log_path_string = log_path.to_string_lossy().to_string();
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--log-file" && pair[1] == log_path_string));
        assert!(args.windows(2).any(|pair| pair == ["daemon", "--quiet"]));

        let args = build_daemon_args(&config_path, &socket_path, false, None)?;
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(!args.contains(&"--debug".to_string()));
        assert!(!args.contains(&"--log-file".to_string()));
        Ok(())
    }

    #[test]
    fn control_command_socket_uses_explicit_socket() -> Result<()> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let explicit_socket = temp.path().join("explicit.sock");
        let resolved = resolve_control_command_socket(
            &Some(config_path),
            Some(explicit_socket.clone()),
            temp.path(),
        )?;

        assert_eq!(resolved, explicit_socket);
        Ok(())
    }

    #[test]
    fn command_socket_uses_explicit_socket() -> Result<()> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let explicit_socket = temp.path().join("explicit.sock");
        let resolved = resolve_command_socket(
            &Some(config_path),
            Some(explicit_socket.clone()),
            temp.path(),
        )?;

        assert_eq!(resolved, explicit_socket);
        Ok(())
    }

    #[test]
    fn command_socket_uses_explicit_config() -> Result<()> {
        let temp = TempDir::new()?;
        let project_dir = temp.path().join("project");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&project_dir)?;
        std::fs::create_dir_all(&elsewhere)?;

        let config_path = project_dir.join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let resolved = resolve_command_socket(&Some(config_path.clone()), None, &elsewhere)?;

        assert_eq!(resolved, socket_path_for_config(&config_path));
        Ok(())
    }

    #[test]
    fn command_socket_discovers_project_from_start_dir() -> Result<()> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let nested = temp.path().join("a/b/c");
        std::fs::create_dir_all(&nested)?;

        let resolved = resolve_command_socket(&None, None, &nested)?;

        assert_eq!(resolved, socket_path_for_config(&config_path));
        Ok(())
    }

    #[test]
    fn command_socket_errors_outside_project_without_socket() -> Result<()> {
        let temp = TempDir::new()?;
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside)?;

        let err = resolve_command_socket(&None, None, &outside).unwrap_err();
        let daemon_err = err.downcast_ref::<DaemonError>().expect("daemon error");

        match daemon_err {
            DaemonError::SocketResolution { start_dir } => {
                assert_eq!(start_dir, &PathBuf::from(&outside));
            }
            other => panic!("expected socket resolution error, got {:?}", other),
        }

        let message = err.to_string();
        assert!(message.contains("could not resolve daemon socket from"));
        assert_eq!(
            daemon_err.hint(),
            Some("run this command from a zaz project directory or pass --socket <PATH>")
        );
        Ok(())
    }

    #[test]
    fn control_command_socket_uses_explicit_config() -> Result<()> {
        let temp = TempDir::new()?;
        let project_dir = temp.path().join("project");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&project_dir)?;
        std::fs::create_dir_all(&elsewhere)?;

        let config_path = project_dir.join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let resolved =
            resolve_control_command_socket(&Some(config_path.clone()), None, &elsewhere)?;

        assert_eq!(resolved, socket_path_for_config(&config_path));
        Ok(())
    }

    #[test]
    fn control_command_socket_discovers_project_from_start_dir() -> Result<()> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let nested = temp.path().join("a/b/c");
        std::fs::create_dir_all(&nested)?;

        let resolved = resolve_control_command_socket(&None, None, &nested)?;

        assert_eq!(resolved, socket_path_for_config(&config_path));
        Ok(())
    }

    #[test]
    fn control_command_socket_errors_outside_project_without_socket() -> Result<()> {
        let temp = TempDir::new()?;
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside)?;

        let err = resolve_control_command_socket(&None, None, &outside).unwrap_err();
        let daemon_err = err.downcast_ref::<DaemonError>().expect("daemon error");

        match daemon_err {
            DaemonError::SocketResolution { start_dir } => {
                assert_eq!(start_dir, &PathBuf::from(&outside));
            }
            other => panic!("expected socket resolution error, got {:?}", other),
        }

        let message = err.to_string();
        assert!(message.contains("could not resolve daemon socket from"));
        assert_eq!(
            daemon_err.hint(),
            Some("run this command from a zaz project directory or pass --socket <PATH>")
        );
        Ok(())
    }

    #[tokio::test]
    async fn daemon_responsive_reports_false_for_missing_socket() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("missing.sock");

        assert!(!is_daemon_responsive(&socket_path, Duration::from_millis(100)).await);
    }

    #[tokio::test]
    async fn start_daemon_via_launcher_starts_reachable_daemon() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("zaz.toml");
        let socket_path = temp.path().join("daemon.sock");
        let output_log = temp.path().join("daemon-output.log");

        std::fs::write(
            &config_path,
            r#"
[[group]]
name = "backend"
patterns = ["**/*.rs"]

[[group.task]]
name = "noop"
command = "true"
"#,
        )
        .unwrap();

        let mut handle =
            start_daemon_via_launcher(&config_path, &socket_path, false, None, &output_log)
                .unwrap();
        assert!(handle.id() > 0);

        let outcome =
            wait_for_daemon_ready(&socket_path, &mut handle, 50, Duration::from_millis(100))
                .await
                .unwrap();
        match outcome {
            DaemonReadyOutcome::Ready => {}
            other => {
                let log_contents = std::fs::read_to_string(&output_log).unwrap_or_default();
                panic!(
                    "expected Ready, got {:?}; daemon-output.log: {}",
                    other, log_contents
                );
            }
        }
        assert!(is_daemon_responsive(&socket_path, Duration::from_secs(1)).await);

        let mut client = Client::connect(&socket_path).await.unwrap();
        let response = client.request(&ApiRequest::Shutdown).await.unwrap();
        assert!(matches!(response, ApiResponse::Ok { .. }));
    }

    #[tokio::test]
    async fn wait_for_daemon_ready_reports_crash_when_daemon_exits_immediately() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("never-bound.sock");
        let output_log = temp.path().join("daemon-output.log");

        let mut launcher = DaemonLauncher::new("/bin/sh", &output_log);
        launcher.args(["-c", "exit 1"]);
        let mut handle = launcher.launch().unwrap();

        let outcome =
            wait_for_daemon_ready(&socket_path, &mut handle, 50, Duration::from_millis(20))
                .await
                .unwrap();
        match outcome {
            DaemonReadyOutcome::Crashed(status) => assert!(!status.success()),
            other => panic!("expected crash outcome, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn start_daemon_command_launches_responsive_daemon() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("zaz.toml");
        let socket_path = temp.path().join("daemon.sock");
        let output_log = temp.path().join("daemon-output.log");

        std::fs::write(
            &config_path,
            r#"
[[group]]
name = "backend"
patterns = ["**/*.rs"]

[[group.task]]
name = "noop"
command = "true"
"#,
        )
        .unwrap();

        start_daemon_command(&config_path, &socket_path, false, None, &output_log)
            .await
            .unwrap_or_else(|e| {
                let log_contents = std::fs::read_to_string(&output_log).unwrap_or_default();
                panic!("start_daemon_command failed: {e}; daemon-output.log: {log_contents}");
            });

        assert!(is_daemon_responsive(&socket_path, Duration::from_secs(1)).await);

        let mut client = Client::connect(&socket_path).await.unwrap();
        let response = client.request(&ApiRequest::Shutdown).await.unwrap();
        assert!(matches!(response, ApiResponse::Ok { .. }));
    }

    #[tokio::test]
    async fn start_daemon_command_returns_ok_when_daemon_already_running() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;
        use zaz_daemon::DaemonState;

        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("zaz.toml");
        let socket_path = temp.path().join("daemon.sock");
        let output_log = temp.path().join("daemon-output.log");

        // Config exists but is otherwise unused — the early-return path must
        // never spawn a child or touch the daemon binary.
        std::fs::write(&config_path, "").unwrap();

        let listener = UnixListener::bind(&socket_path).unwrap();
        let handler = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();

            let request: ApiRequest = serde_json::from_str(&line).unwrap();
            assert!(matches!(request, ApiRequest::Status));

            let response = ApiResponse::Status {
                state: DaemonState::default(),
            };
            let body = serde_json::to_string(&response).unwrap();
            writer.write_all(body.as_bytes()).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
        });

        start_daemon_command(&config_path, &socket_path, false, None, &output_log)
            .await
            .expect("start_daemon_command should report success when daemon already running");

        handler.await.unwrap();

        // No daemon was launched, so the output log should never have been
        // written to.
        assert!(!output_log.exists());
    }
}
