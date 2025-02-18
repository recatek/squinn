use quinn_proto::{ReadError, WriteError};
use thiserror::Error;
use web_transport_proto::{ConnectError, SettingsError};

#[derive(Error, Debug, Clone)]
pub enum WebTransportError {
    #[error("quic stream was closed early")]
    UnexpectedEnd,
    #[error("webtransport is not supported")]
    WebTransportUnsupported,
    #[error("connect already finished")]
    ConnectAlreadyFinished,
    #[error("not ready to respond")]
    NotReadyToRespond,

    #[error("read error: {0}")]
    ReadError(#[from] ReadError),
    #[error("write error: {0}")]
    WriteError(#[from] WriteError),
    #[error("settings error: {0}")]
    SettingsError(#[from] SettingsError),
    #[error("connect error: {0}")]
    ConnectError(#[from] ConnectError),
}
