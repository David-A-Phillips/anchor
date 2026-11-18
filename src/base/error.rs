//! Crate-wide error type.

use super::llm::ModelError;

/// Convenience alias used across the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Top-level error for Anchor.
///
/// Modules are free to define richer local error enums; they convert into this type either through
/// one of the specific variants or through [`Error::module`], which preserves the source error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem or process I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// SQLite failure.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// JSON (de)serialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// HTTP transport failure.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// A language-model call failed.
    #[error("model error: {0}")]
    Model(#[from] ModelError),

    /// The configuration is missing, malformed or inconsistent.
    #[error("configuration error: {0}")]
    Config(String),

    /// Caller-supplied input is invalid.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// A requested entity does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// A source document could not be parsed.
    #[error("parse error: {0}")]
    Parse(String),

    /// The sync protocol or a `.contextpack` was rejected.
    #[error("sync error: {0}")]
    Sync(String),

    /// A hash, signature or structural integrity check failed.
    #[error("integrity check failed: {0}")]
    Integrity(String),

    /// An error raised by a specific module, with its source preserved.
    #[error("{module}: {source}")]
    Module {
        /// Short module name, e.g. `"ollama"`.
        module: &'static str,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    /// Anything that does not fit the variants above.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Builds an [`Error::Config`].
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// Builds an [`Error::Invalid`].
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    /// Builds an [`Error::NotFound`].
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    /// Builds an [`Error::Parse`].
    pub fn parse(message: impl Into<String>) -> Self {
        Self::Parse(message.into())
    }

    /// Builds an [`Error::Sync`].
    pub fn sync(message: impl Into<String>) -> Self {
        Self::Sync(message.into())
    }

    /// Builds an [`Error::Integrity`].
    pub fn integrity(message: impl Into<String>) -> Self {
        Self::Integrity(message.into())
    }

    /// Builds an [`Error::Other`].
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    /// The message without the category prefix `Display` adds ("not found: ", ...), for wrapping
    /// one error's text in another without repeating the category.
    pub fn detail(&self) -> String {
        match self {
            Self::Config(message)
            | Self::Invalid(message)
            | Self::NotFound(message)
            | Self::Parse(message)
            | Self::Sync(message)
            | Self::Integrity(message)
            | Self::Other(message) => message.clone(),
            other => other.to_string(),
        }
    }

    /// Wraps a module-specific error, keeping it available through `source()`.
    pub fn module(
        module: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self::Module {
            module,
            source: source.into(),
        }
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(err: tokio::task::JoinError) -> Self {
        Self::Other(format!("background task failed: {err}"))
    }
}

impl From<toml::de::Error> for Error {
    fn from(err: toml::de::Error) -> Self {
        Self::Config(err.to_string())
    }
}

impl From<toml::ser::Error> for Error {
    fn from(err: toml::ser::Error) -> Self {
        Self::Config(err.to_string())
    }
}
