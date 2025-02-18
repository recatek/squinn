use std::io::Cursor;

use quinn_proto::{Connection, Dir, StreamId};
use url::Url;
use web_transport_proto::{ConnectError, ConnectRequest, VarInt};

use crate::webtransport::WebTransportError;

pub struct ConnectData {
    pub url: Url,
    pub session_id: VarInt,
    pub response_id: StreamId,
}

#[derive(Default)]
pub struct ConnectState {
    recv_id: Option<StreamId>,
}

#[allow(dead_code)]
impl ConnectState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(
        &mut self,
        connection: &mut Connection,
        recv_buf: &mut Vec<u8>,
    ) -> Result<Option<ConnectData>, WebTransportError> {
        if self.recv_id.is_none() {
            self.recv_id = connection.streams().accept(Dir::Bi); // For response
        }

        if let Some(recv_id) = self.recv_id {
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
                    super::close_recv_stream(&mut recv_stream); // Send still open
                    Ok(Some(ConnectData {
                        url: connect.url,
                        session_id: VarInt::from_u64(recv_id.index()).unwrap(),
                        response_id: recv_id,
                    }))
                }
            };
        }

        Ok(None) // Keep trying
    }
}
