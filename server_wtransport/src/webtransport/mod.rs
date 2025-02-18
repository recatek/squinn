mod connect;
mod error;
mod request;
mod response;
mod settings;

pub use connect::ConnectState;
pub use error::WebTransportError;
pub use response::ResponseState;
pub use settings::SettingsState;

use quinn_proto::{FinishError, SendStream, RecvStream};

fn close_recv_stream(recv_stream: &mut RecvStream) {
    _ = recv_stream.stop(0u32.into()); // Ignore ClosedStream errors
}

fn close_send_stream(send_stream: &mut SendStream) {
    match send_stream.finish() {
        Ok(()) => (), // Everything worked fine
        Err(FinishError::Stopped(reason)) => _ = send_stream.reset(reason),
        Err(FinishError::ClosedStream) => (), // Already closed
    }
}
