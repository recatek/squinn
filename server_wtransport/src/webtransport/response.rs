use quinn_proto::{Connection, StreamId};
use crate::webtransport::WebTransportError;

pub struct ResponseState {
    send_id: StreamId,
    send_bytes: usize,
}

impl ResponseState {
    pub fn new(send_id: StreamId) -> ResponseState {
        Self {
            send_id,
            send_bytes: 0,
        }
    }

    pub fn update(&mut self, connection: &mut Connection, data_buf: &mut Vec<u8>) -> Result<bool, WebTransportError> {
        todo!()
    }
}
