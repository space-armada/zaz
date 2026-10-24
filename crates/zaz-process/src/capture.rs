//! Run a shell command once and capture its raw output.
//!
//! This is the evaluator's spawn path. The command runs in its own process group with stdin set
//! to null. Both pipes are read as raw bytes to end of file under one deadline that covers spawn
//! to exit and end of output. Each stream has a cap.
//!
//! It does not share [`crate::StreamingRun`]. That reader splits lines, stops silently at invalid
//! UTF-8, and has no deadline or kill.

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::Signal;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::SignalHandler;

/// How long a group has to exit after SIGTERM before it is sent SIGKILL.
pub const TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// How often the exit of the leader is checked while the pipes are quiet.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

const READ_CHUNK: usize = 16 * 1024;

/// A command to capture.
#[derive(Debug)]
pub struct CaptureSpec<'a> {
    /// The shell that runs `command` with `-c`.
    pub shell: &'a str,
    /// The command string.
    pub command: &'a str,
    /// The directory the command runs in.
    pub working_dir: &'a Path,
    /// The complete environment. Nothing is inherited.
    pub env: &'a HashMap<OsString, OsString>,
    /// The deadline for the whole run, from spawn to exit and end of output on both pipes.
    pub timeout: Duration,
    /// The most stdout bytes kept before the run fails.
    pub max_stdout: usize,
    /// The most stderr bytes kept before the run fails.
    pub max_stderr: usize,
}

/// A command that ran to exit with both pipes closed.
#[derive(Debug)]
pub struct Captured {
    /// How the leader exited.
    pub status: ExitStatus,
    /// Everything the command wrote to stdout.
    pub stdout: Vec<u8>,
    /// Everything the command wrote to stderr.
    pub stderr: Vec<u8>,
}

/// Why a capture produced no output.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The shell could not be started.
    #[error("cannot start the shell: {0}")]
    Spawn(#[source] io::Error),

    /// A pipe could not be read.
    #[error("cannot read the command's output: {0}")]
    Read(#[source] io::Error),

    /// The command was still running at the deadline. Its group was terminated.
    #[error("still running after {}", format_duration(.after))]
    Timeout { after: Duration },

    /// The leader exited but a descendant held a pipe open to the deadline.
    #[error(
        "exited {} ago, but a background process still held its output open at the deadline",
        format_duration(.exited_ago)
    )]
    HeldPipe { exited_ago: Duration },

    /// Stdout grew past its cap.
    #[error("wrote more than {limit} bytes to stdout")]
    StdoutTooLarge { limit: usize },

    /// Stderr grew past its cap.
    #[error("wrote more than {limit} bytes to stderr")]
    StderrTooLarge { limit: usize },

    /// The run was cancelled through its [`CancelHandle`].
    #[error("cancelled")]
    Cancelled,
}

fn format_duration(duration: &Duration) -> String {
    if duration.as_secs() >= 10 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

#[derive(Debug, Default)]
struct CancelState {
    cancelled: bool,
    pgid: Option<i32>,
}

/// Cancels the capture it is passed to.
///
/// One handle can be passed to a series of captures. Cancelling signals the group of the one that
/// is running and makes every later one fail at once with [`CaptureError::Cancelled`].
#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    state: Arc<Mutex<CancelState>>,
}

impl CancelHandle {
    /// A handle that has not been cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel the running capture.
    ///
    /// The group is sent SIGTERM. The capture then escalates to SIGKILL after
    /// [`TERMINATE_GRACE`] and returns [`CaptureError::Cancelled`].
    pub fn cancel(&self) {
        let mut state = self.lock();
        state.cancelled = true;

        if let Some(pgid) = state.pgid {
            let _ = SignalHandler::send_to_group(pgid, Signal::SIGTERM);
        }
    }

    /// Whether no captured command's process group is left to signal.
    ///
    /// After [`CancelHandle::cancel`] this turns true once the running command's group has been
    /// signalled to the end and its leader reaped. A caller about to exit waits for it so a
    /// command that ignores SIGTERM does not outlive the caller.
    pub fn is_idle(&self) -> bool {
        self.lock().pgid.is_none()
    }

    /// Whether [`CancelHandle::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    fn lock(&self) -> MutexGuard<'_, CancelState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Run `spec` and capture its output.
///
/// On a timeout, a held pipe, an oversized stream, or a cancel, the command's process group is
/// signalled, the leader is reaped, and no output is returned.
pub fn capture(spec: &CaptureSpec<'_>, cancel: &CancelHandle) -> Result<Captured, CaptureError> {
    let started = Instant::now();
    let deadline = started + spec.timeout;

    let mut command = Command::new(spec.shell);
    command
        .arg("-c")
        .arg(spec.command)
        .current_dir(spec.working_dir)
        .env_clear()
        .envs(spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // SAFETY: the closure runs in the forked child before exec and calls only `setpgid`, which is
    // async-signal-safe. It allocates nothing and takes no locks.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                .map_err(io::Error::other)
        });
    }

    let mut running = {
        let mut state = cancel.lock();

        if state.cancelled {
            return Err(CaptureError::Cancelled);
        }

        let child = command.spawn().map_err(CaptureError::Spawn)?;
        state.pgid = Some(child.id() as i32);

        Running::new(child, Arc::clone(&cancel.state))
    };

    match running.drive(spec, deadline, started) {
        Ok(captured) => Ok(captured),
        Err(error) => {
            running.terminate();
            Err(error)
        }
    }
}

struct Pipe {
    reader: Box<dyn ReadFd>,
    data: Vec<u8>,
    open: bool,
}

trait ReadFd: Read + AsFd {}

impl<T: Read + AsFd> ReadFd for T {}

struct Running {
    child: Child,
    pgid: i32,
    state: Arc<Mutex<CancelState>>,
    stdout: Option<Pipe>,
    stderr: Option<Pipe>,
    exited_at: Option<Instant>,
}

impl Running {
    fn new(mut child: Child, state: Arc<Mutex<CancelState>>) -> Self {
        let stdout = child.stdout.take().map(Pipe::new);
        let stderr = child.stderr.take().map(Pipe::new);

        let pgid = child.id() as i32;

        Self {
            child,
            pgid,
            state,
            stdout,
            stderr,
            exited_at: None,
        }
    }

    fn drive(
        &mut self,
        spec: &CaptureSpec<'_>,
        deadline: Instant,
        started: Instant,
    ) -> Result<Captured, CaptureError> {
        loop {
            self.check_cancelled()?;
            self.check_exit()?;

            self.pump(spec)?;

            if self.exited_at.is_some() && self.pipes_closed() {
                let status = self.reap()?;

                return Ok(Captured {
                    status,
                    stdout: self.stdout.take().map(|p| p.data).unwrap_or_default(),
                    stderr: self.stderr.take().map(|p| p.data).unwrap_or_default(),
                });
            }

            let now = Instant::now();

            if now >= deadline {
                return Err(match self.exited_at {
                    None => CaptureError::Timeout {
                        after: now.duration_since(started),
                    },
                    Some(exited_at) => CaptureError::HeldPipe {
                        exited_ago: now.duration_since(exited_at),
                    },
                });
            }

            self.wait_for_output(deadline - now)?;
        }
    }

    fn check_cancelled(&mut self) -> Result<(), CaptureError> {
        let cancelled = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled;

        if cancelled {
            return Err(CaptureError::Cancelled);
        }

        Ok(())
    }

    // The exit is detected without reaping. The zombie leader keeps its process group ID taken
    // until `reap`, so a signal to the group can never reach a recycled ID, however long a
    // descendant holds the pipes after the leader is gone.
    fn check_exit(&mut self) -> Result<(), CaptureError> {
        if self.exited_at.is_none()
            && leader_has_exited(self.child.id()).map_err(CaptureError::Read)?
        {
            self.exited_at = Some(Instant::now());
        }

        Ok(())
    }

    /// Reap the leader and stop `cancel` from signalling its group.
    fn reap(&mut self) -> Result<ExitStatus, CaptureError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pgid = None;

        self.child.wait().map_err(CaptureError::Read)
    }

    fn pipes_closed(&self) -> bool {
        !self.stdout.as_ref().is_some_and(|p| p.open)
            && !self.stderr.as_ref().is_some_and(|p| p.open)
    }

    fn pump(&mut self, spec: &CaptureSpec<'_>) -> Result<(), CaptureError> {
        if let Some(pipe) = &mut self.stdout {
            if pipe.read_available(spec.max_stdout)? {
                return Err(CaptureError::StdoutTooLarge {
                    limit: spec.max_stdout,
                });
            }
        }

        if let Some(pipe) = &mut self.stderr {
            if pipe.read_available(spec.max_stderr)? {
                return Err(CaptureError::StderrTooLarge {
                    limit: spec.max_stderr,
                });
            }
        }

        Ok(())
    }

    fn wait_for_output(&self, remaining: Duration) -> Result<(), CaptureError> {
        let wait = remaining.min(POLL_INTERVAL);
        let mut fds = Vec::new();

        for pipe in [&self.stdout, &self.stderr].into_iter().flatten() {
            if pipe.open {
                fds.push(PollFd::new(pipe.reader.as_fd(), PollFlags::POLLIN));
            }
        }

        if fds.is_empty() {
            std::thread::sleep(wait);
            return Ok(());
        }

        let millis = wait.as_millis().clamp(1, u16::MAX as u128) as u16;

        match poll(&mut fds, PollTimeout::from(millis)) {
            Ok(_) | Err(nix::errno::Errno::EINTR) => Ok(()),
            Err(e) => Err(CaptureError::Read(io::Error::from(e))),
        }
    }

    /// Signal the group, then reap the leader.
    ///
    /// The group is sent SIGTERM while the leader is still unreaped, running or a zombie. The
    /// leader and the descendants that hold the pipes then have [`TERMINATE_GRACE`] to finish.
    /// The group is sent SIGKILL after that, and only then is the leader reaped, so the group ID
    /// is held until the last signal is sent.
    fn terminate(&mut self) {
        let _ = SignalHandler::send_to_group(self.pgid, Signal::SIGTERM);

        let grace_ends = Instant::now() + TERMINATE_GRACE;

        while Instant::now() < grace_ends {
            let _ = self.check_exit();
            self.discard_output();

            if self.exited_at.is_some() && self.pipes_closed() {
                break;
            }

            let _ = self.wait_for_output(POLL_INTERVAL);
        }

        let _ = SignalHandler::send_to_group(self.pgid, Signal::SIGKILL);

        let _ = self.reap();
    }

    fn discard_output(&mut self) {
        for pipe in [&mut self.stdout, &mut self.stderr].into_iter().flatten() {
            let _ = pipe.read_available(usize::MAX);
            pipe.data.clear();
        }
    }
}

/// Whether the leader has exited, without reaping it.
fn leader_has_exited(pid: u32) -> io::Result<bool> {
    // SAFETY: `siginfo_t` is plain data that the kernel fills in, and all zeroes is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };

    // SAFETY: `info` is a valid, writable `siginfo_t` for the whole call. `WNOHANG` makes the call
    // return at once and `WNOWAIT` leaves the child waitable, so nothing is reaped.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };

    if result == 0 {
        return Ok(info.si_signo != 0);
    }

    let error = io::Error::last_os_error();

    if error.kind() == io::ErrorKind::Interrupted {
        return Ok(false);
    }

    Err(error)
}

impl Pipe {
    fn new<T: Read + AsFd + 'static>(reader: T) -> Self {
        let flags = fcntl(reader.as_fd().as_raw_fd(), FcntlArg::F_GETFL).unwrap_or(0);
        let _ = fcntl(
            reader.as_fd().as_raw_fd(),
            FcntlArg::F_SETFL(OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK),
        );

        Self {
            reader: Box::new(reader),
            data: Vec::new(),
            open: true,
        }
    }

    /// Read everything that is ready. Returns true when the stream grew past `limit`.
    fn read_available(&mut self, limit: usize) -> Result<bool, CaptureError> {
        let mut chunk = [0u8; READ_CHUNK];

        while self.open {
            match self.reader.read(&mut chunk) {
                Ok(0) => self.open = false,
                Ok(n) => {
                    self.data.extend_from_slice(&chunk[..n]);

                    if self.data.len() > limit {
                        return Ok(true);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(CaptureError::Read(e)),
            }
        }

        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(command: &str, timeout: Duration) -> Result<Captured, CaptureError> {
        run_with(command, timeout, 1024, 1024, &CancelHandle::new())
    }

    fn run_with(
        command: &str,
        timeout: Duration,
        max_stdout: usize,
        max_stderr: usize,
        cancel: &CancelHandle,
    ) -> Result<Captured, CaptureError> {
        let env: HashMap<OsString, OsString> =
            std::env::vars_os().filter(|(k, _)| k == "PATH").collect();

        capture(
            &CaptureSpec {
                shell: "/bin/sh",
                command,
                working_dir: Path::new("/"),
                env: &env,
                timeout,
                max_stdout,
                max_stderr,
            },
            cancel,
        )
    }

    #[test]
    fn output_and_status_are_captured_raw() {
        let captured = run(
            "printf 'a\\377b'; printf err >&2; exit 3",
            Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(captured.stdout, b"a\xffb");
        assert_eq!(captured.stderr, b"err");
        assert_eq!(captured.status.code(), Some(3));
    }

    #[test]
    fn the_environment_is_exactly_the_one_given() {
        let env: HashMap<OsString, OsString> = [
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (OsString::from("ZAZ_ONLY"), OsString::from("yes")),
        ]
        .into_iter()
        .collect();

        let captured = capture(
            &CaptureSpec {
                shell: "/bin/sh",
                command: "echo \"$ZAZ_ONLY:${HOME-unset}\"",
                working_dir: Path::new("/"),
                env: &env,
                timeout: Duration::from_secs(5),
                max_stdout: 1024,
                max_stderr: 1024,
            },
            &CancelHandle::new(),
        )
        .unwrap();

        assert_eq!(captured.stdout, b"yes:unset\n");
    }

    #[test]
    fn a_command_that_outruns_the_deadline_times_out_and_its_group_dies() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("alive");
        let command = format!("(sleep 30; touch {}) & sleep 30", marker.display());

        let started = Instant::now();
        let error = run(&command, Duration::from_millis(300)).unwrap_err();

        assert!(matches!(error, CaptureError::Timeout { .. }), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!marker.exists());
    }

    fn assert_process_dies(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(5);

        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            assert!(Instant::now() < deadline, "process {pid} is still alive");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn held_pipe_descendant(helper: &str) -> (CaptureError, i32) {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("helper.pid");
        let command = format!("{helper} & echo $! > {}; echo '{{}}'", pid_file.display());

        let error = run(&command, Duration::from_millis(400)).unwrap_err();
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        (error, pid)
    }

    #[test]
    fn a_descendant_holding_the_pipe_past_the_deadline_is_a_held_pipe_and_is_killed() {
        let (error, pid) = held_pipe_descendant("sleep 30");

        assert!(matches!(error, CaptureError::HeldPipe { .. }), "{error}");
        assert_process_dies(pid);
    }

    #[test]
    fn a_term_ignoring_descendant_holding_the_pipe_is_killed_after_the_grace() {
        let started = Instant::now();
        let (error, pid) = held_pipe_descendant("sh -c \"trap '' TERM; sleep 30\"");

        assert!(matches!(error, CaptureError::HeldPipe { .. }), "{error}");
        assert!(started.elapsed() >= TERMINATE_GRACE);
        assert_process_dies(pid);
    }

    #[test]
    fn a_leader_that_exited_is_detected_without_being_reaped() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 4"])
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);

        while !leader_has_exited(child.id()).unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            leader_has_exited(child.id()).unwrap(),
            "still waitable after detection"
        );
        assert_eq!(child.wait().unwrap().code(), Some(4));
    }

    #[test]
    fn a_descendant_that_releases_the_pipe_in_time_still_succeeds() {
        let captured = run("(sleep 0.3; echo late) & echo now", Duration::from_secs(5)).unwrap();

        assert_eq!(captured.stdout, b"now\nlate\n");
    }

    #[test]
    fn oversized_streams_fail_with_their_own_error() {
        let out = run_with(
            "yes | head -c 5000",
            Duration::from_secs(5),
            100,
            1024,
            &CancelHandle::new(),
        )
        .unwrap_err();
        assert!(
            matches!(out, CaptureError::StdoutTooLarge { limit: 100 }),
            "{out}"
        );

        let err = run_with(
            "yes >&2",
            Duration::from_secs(5),
            1024,
            100,
            &CancelHandle::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, CaptureError::StderrTooLarge { limit: 100 }),
            "{err}"
        );
    }

    #[test]
    fn a_missing_working_directory_is_a_spawn_error() {
        let env = HashMap::new();
        let error = capture(
            &CaptureSpec {
                shell: "/bin/sh",
                command: "true",
                working_dir: Path::new("/definitely/not/here"),
                env: &env,
                timeout: Duration::from_secs(5),
                max_stdout: 10,
                max_stderr: 10,
            },
            &CancelHandle::new(),
        )
        .unwrap_err();

        assert!(matches!(error, CaptureError::Spawn(_)), "{error}");
    }

    #[test]
    fn cancel_terminates_the_running_group() {
        let cancel = CancelHandle::new();
        let canceller = cancel.clone();

        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            canceller.cancel();
        });

        let started = Instant::now();
        let error = run_with("sleep 30", Duration::from_secs(60), 10, 10, &cancel).unwrap_err();
        thread.join().unwrap();

        assert!(matches!(error, CaptureError::Cancelled), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_cancelled_handle_refuses_later_captures() {
        let cancel = CancelHandle::new();
        cancel.cancel();

        let error = run_with("true", Duration::from_secs(5), 10, 10, &cancel).unwrap_err();

        assert!(matches!(error, CaptureError::Cancelled), "{error}");
    }

    #[test]
    fn a_term_ignoring_command_is_killed_after_the_grace() {
        let started = Instant::now();
        let error = run("trap '' TERM; sleep 30", Duration::from_millis(200)).unwrap_err();

        assert!(matches!(error, CaptureError::Timeout { .. }), "{error}");
        assert!(started.elapsed() >= TERMINATE_GRACE);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
