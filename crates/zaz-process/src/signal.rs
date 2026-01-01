//! Signal handling utilities.

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use zaz_config::Signal as ConfigSignal;

/// Handles signal operations for processes.
pub struct SignalHandler;

impl SignalHandler {
    /// Convert a config signal to a nix signal.
    pub fn to_nix_signal(signal: ConfigSignal) -> Signal {
        match signal {
            ConfigSignal::Sigterm => Signal::SIGTERM,
            ConfigSignal::Sigint => Signal::SIGINT,
            ConfigSignal::Sighup => Signal::SIGHUP,
            ConfigSignal::Sigkill => Signal::SIGKILL,
            ConfigSignal::Sigquit => Signal::SIGQUIT,
            ConfigSignal::Sigusr1 => Signal::SIGUSR1,
            ConfigSignal::Sigusr2 => Signal::SIGUSR2,
        }
    }

    /// Send a signal to a process.
    pub fn send(pid: i32, sig: Signal) -> Result<(), nix::Error> {
        signal::kill(Pid::from_raw(pid), sig)
    }

    /// Send a signal to a process group (negative PID).
    pub fn send_to_group(pgid: i32, sig: Signal) -> Result<(), nix::Error> {
        signal::kill(Pid::from_raw(-pgid), sig)
    }
}
