use http::StatusCode;
use quinn_proto::{Connection, StreamId};
use web_transport_proto::ConnectResponse;

use crate::webtransport::WebTransportError;

pub struct Response {
    send_id: StreamId,
    send_bytes: usize,
}

impl Response {
    pub fn new(send_id: StreamId) -> Response {
        Self {
            send_id,
            send_bytes: 0,
        }
    }

    pub fn start_response(&mut self, data_buf: &mut Vec<u8>, status: StatusCode) {
        debug_assert!(data_buf.is_empty());
        ConnectResponse { status }.encode(data_buf);
    }

    pub fn update(
        &mut self,
        connection: &mut Connection,
        data_buf: &mut Vec<u8>,
    ) -> Result<Option<StreamId>, WebTransportError> {
        if data_buf.is_empty() == false {
            let mut send_stream = connection.send_stream(self.send_id);
            self.send_bytes += send_stream.write(&data_buf[self.send_bytes..])?;

            if self.send_bytes >= data_buf.len() {
                return Ok(Some(self.send_id));
            }
        }

        Ok(None) // Keep trying
    }
}
