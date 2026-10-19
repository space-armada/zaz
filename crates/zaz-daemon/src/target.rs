//! The daemon a command is addressing, carrying the provenance of every path in it.

use crate::server::socket_path_for_config;
use std::path::{Path, PathBuf};

/// How a config path reached the resolver.
///
/// Operator-facing messages annotate the values zaz derived and echo the ones the
/// operator typed, so the origin decides whether a message explains where the
/// config came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOrigin {
    /// The operator named the config with `--config`.
    Explicit,

    /// Found in the directory the command ran from.
    CurrentDir,

    /// Found by walking above the directory the command ran from.
    Upward,
}

/// The daemon a command is addressing.
///
/// The two variants are the two ways a socket is arrived at, and they differ in
/// what a message about the daemon can say. An operator who passes `--socket`
/// named the address outright, so there is nothing to explain and no config in
/// play. Every other route derives the socket from a config file, which is the
/// artifact the operator wrote and can open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonTarget {
    /// The operator named the socket with `--socket`. Nothing was derived.
    Explicit { socket: PathBuf },

    /// The socket was derived from a config file.
    ForConfig {
        socket: PathBuf,
        config: PathBuf,
        origin: ConfigOrigin,
    },
}

impl DaemonTarget {
    /// Target for a socket path the operator supplied outright.
    pub fn explicit(socket: PathBuf) -> Self {
        Self::Explicit { socket }
    }

    /// Target whose socket is derived from `config`.
    ///
    /// Derivation runs through [`socket_path_for_config`], so the socket and the
    /// config a target reports can never disagree about which daemon is meant.
    pub fn for_config(config: PathBuf, origin: ConfigOrigin) -> Self {
        let socket = socket_path_for_config(&config);
        Self::ForConfig {
            socket,
            config,
            origin,
        }
    }

    /// The socket to connect to.
    pub fn socket(&self) -> &Path {
        match self {
            Self::Explicit { socket } => socket,
            Self::ForConfig { socket, .. } => socket,
        }
    }

    /// The config the socket was derived from, or `None` for an explicit socket.
    pub fn config(&self) -> Option<&Path> {
        match self {
            Self::Explicit { .. } => None,
            Self::ForConfig { config, .. } => Some(config),
        }
    }

    /// How the config was obtained, or `None` for an explicit socket.
    pub fn config_origin(&self) -> Option<ConfigOrigin> {
        match self {
            Self::Explicit { .. } => None,
            Self::ForConfig { origin, .. } => Some(*origin),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_target_echoes_the_socket_and_carries_no_config() {
        let target = DaemonTarget::explicit(PathBuf::from("/tmp/zaz.sock"));

        assert_eq!(target.socket(), Path::new("/tmp/zaz.sock"));
        assert_eq!(target.config(), None);
        assert_eq!(target.config_origin(), None);
    }

    #[test]
    fn config_target_derives_its_socket_from_the_config() {
        let config = PathBuf::from("/tmp/project/zaz.toml");
        let target = DaemonTarget::for_config(config.clone(), ConfigOrigin::Upward);

        assert_eq!(target.socket(), socket_path_for_config(&config));
        assert_eq!(target.config(), Some(config.as_path()));
        assert_eq!(target.config_origin(), Some(ConfigOrigin::Upward));
    }

    #[test]
    fn config_target_keeps_the_origin_it_was_built_with() {
        let config = PathBuf::from("/tmp/project/zaz.toml");

        for origin in [
            ConfigOrigin::Explicit,
            ConfigOrigin::CurrentDir,
            ConfigOrigin::Upward,
        ] {
            let target = DaemonTarget::for_config(config.clone(), origin);
            assert_eq!(target.config_origin(), Some(origin));
        }
    }
}
