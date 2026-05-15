use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("tls: {0}")]
    Tls(String),

    #[error("usb: {0}")]
    Usb(String),
}
