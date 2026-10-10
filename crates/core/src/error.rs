use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("capture source unavailable: {0}")]
    Unavailable(String),
    #[error("capture stream closed")]
    Closed,
    #[error("resize not supported by this capture source")]
    ResizeUnsupported,
}

#[derive(Debug, Error)]
pub enum EncodeError {
    #[error("unsupported frame: {0}")]
    Unsupported(String),
    #[error("encoder failure: {0}")]
    Backend(String),
}

#[derive(Debug, Error)]
pub enum InputError {
    #[error("input backend failure: {0}")]
    Backend(String),
}

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard backend failure: {0}")]
    Backend(String),
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("audio backend failure: {0}")]
    Backend(String),
    #[error("audio stream closed")]
    Closed,
}

#[derive(Debug, Error)]
pub enum LoginError {
    /// Credentials refused (wrong password, unknown or forbidden account): deliberately without detail.
    #[error("login denied")]
    Denied,
    #[error("login backend failure: {0}")]
    Backend(String),
}
