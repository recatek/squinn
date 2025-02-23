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
            // There's no point at which we close this stream. If we error, we'll nuke the whole
            // connection. If we succeed, the stream stays open for the whole connection lifetime.
            self.stream_id = connection.streams().accept(Dir::Bi); // Bidirectional for response
        }

        if let Some(recv_id) = self.stream_id {
            let mut recv_stream = connection.recv_stream(recv_id);
            let recv_chunk = recv_stream
                .read(true)
                .map_err(|_| WebTransportError::UnexpectedEnd)?
                .next(usize::MAX)?
                .ok_or(WebTransportError::UnexpectedEnd)?;

            recv_buf.extend_from_slice(&recv_chunk.bytes);

            return match ConnectRequest::decode(&mut Cursor::new(&recv_buf)) {
                Err(ConnectError::UnexpectedEnd) => Ok(None), // Keep trying
                Err(e) => Err(e.into()),

                Ok(connect) => Ok(Some((connect.url, recv_id))),
            };
        }

        Ok(None) // Keep trying
    }
}
