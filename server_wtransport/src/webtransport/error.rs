use quinn_proto::coding::UnexpectedEnd;
use quinn_proto::{ReadError, SendDatagramError, WriteError};
use web_transport_proto::{ConnectError, SettingsError};

#[derive(thiserror::Error, Debug, Clone)]
pub enum WebTransportError {
    #[error("quic stream was closed early")]
    UnexpectedEnd,
    #[error("unexpected session id")]
    UnexpectedSessionId,
    #[error("webtransport is not supported")]
    WebTransportUnsupported,
    #[error("webtransport is not connected")]
    WebTransportNotConnected,
    #[error("not ready to respond")]
    NotReadyToRespond,

    #[error("read error: {0}")]
    ReadError(#[from] ReadError),
    #[error("write error: {0}")]
    WriteError(#[from] WriteError),
    #[error("send datagram error: {0}")]
    SendDatagramError(#[from] SendDatagramError),
    #[error("settings error: {0}")]
    SettingsError(#[from] SettingsError),
    #[error("connect error: {0}")]
    ConnectError(#[from] ConnectError),
}

impl From<UnexpectedEnd> for WebTransportError {
    fn from(_: UnexpectedEnd) -> Self {
        WebTransportError::UnexpectedEnd
    }
}
