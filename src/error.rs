use thiserror::Error;

/// Errors that can abort a whole run. Per-page failures are recorded and
/// reported rather than propagated (see `pipeline::PageOutcome`).
#[derive(Debug, Error)]
pub enum Error {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A URL that came from user input or page content, carrying where it
    /// came from. Separate from `Url`, which is the parser's own error and is
    /// what `?` produces.
    #[error("invalid url: {0}")]
    BadUrl(String),

    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),

    #[error("html rewrite failed: {0}")]
    Rewrite(String),

    #[error("markdown conversion failed: {0}")]
    Convert(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
