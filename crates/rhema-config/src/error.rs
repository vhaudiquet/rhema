//! The single error type every config operation wraps.

/// Why a config operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Neither `HOME` nor `USERPROFILE` is set, so the user home, and
    /// every convention that hangs off it, the config base included,
    /// cannot be located.
    #[error("cannot locate the home directory: HOME and USERPROFILE are both unset")]
    NoHome,
    /// Reading or writing the file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The YAML file is not valid or not the expected shape.
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    /// A foreign [`crate::store::Store`] refused the section: its own parse or
    /// format error, carried verbatim.
    #[error("settings store: {0}")]
    Store(String),
}
