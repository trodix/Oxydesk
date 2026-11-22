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

/// Failure of an operation on a redirected drive; maps to an errno on the desktop side.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DriveError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    Exists,
    #[error("access denied")]
    Denied,
    #[error("not a directory")]
    NotDirectory,
    #[error("is a directory")]
    IsDirectory,
    #[error("directory not empty")]
    NotEmpty,
    #[error("no space left")]
    NoSpace,
    #[error("invalid argument")]
    Invalid,
    #[error("file in use")]
    Busy,
    #[error("drive unavailable: {0}")]
    Io(String),
}

/// Failure of a print job sent to a redirected printer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PrintError {
    /// The client refused the job or one of its blocks.
    #[error("the client refused the job: {0}")]
    Refused(String),
    #[error("printer unavailable: {0}")]
    Io(String),
}

/// Failure of a call to the smart card subsystem of the client.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SmartcardError {
    /// The client's smart card subsystem answered with this `SCARD_*` code (see `smartcard::code`).
    #[error("smart card error {0:#010x}")]
    Code(u32),
    /// The client is gone or the link failed: no answer will come.
    #[error("smart card link down: {0}")]
    Link(String),
}

impl SmartcardError {
    /// The `SCARD_*` code to give an application: the one the client sent, else `SCARD_E_NO_SERVICE`.
    pub fn scard_code(&self) -> u32 {
        match self {
            Self::Code(c) => *c,
            Self::Link(_) => crate::smartcard::code::E_NO_SERVICE,
        }
    }
}
