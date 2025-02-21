use std::io::Cursor;

use quinn_proto::{Connection, Dir, StreamId};
use url::Url;
use web_transport_proto::{ConnectError, ConnectRequest};

use crate::webtransport::WebTransportError;

#[derive(Default)]
pub struct Connect {
    stream_id: Option<StreamId>,
}

#[allow(dead_code)]
impl Connect {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(
        &mut self,
        connection: &mut Connection,
        recv_buf: &mut Vec<u8>,
    ) -> Result<Option<(Url, StreamId)>, WebTransportError> {
        if self.stream_id.is_none() {
            self.stream_id = connection.streams().accept(Dir::Bi); // For response
        }

        if let Some(recv_id) = self.stream_id {
            // We don't close on error since we'll just nuke the connection
            let mut recv_stream = connection.recv_stream(recv_id);
            let recv_chunk = recv_stream
                .read(true)
                .map_err(|_| WebTransportError::UnexpectedEnd)?
                .next(usize::MAX)?
                .ok_or(WebTransportError::UnexpectedEnd)?;

            recv_buf.extend_from_slice(&recv_chunk.bytes);

            return match ConnectRequest::decode(&mut Cursor::new(&recv_buf)) {
                Err(ConnectError::UnexpectedEnd) => Ok(None),  // Keep trying
                Err(e) => Err(e.into()),

                // Got what we wanted here
                Ok(connect) => {
                    // The stream stays open for the duration of the connection
                    Ok(Some((connect.url, recv_id)))
                }
            };
        }

        Ok(None) // Keep trying
    }
}
