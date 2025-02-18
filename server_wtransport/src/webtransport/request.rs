use quinn_proto::Connection;

use crate::webtransport::connect::ConnectData;
use crate::webtransport::{ConnectState, ResponseState, SettingsState, WebTransportError};

const DATA_BUFFER_SIZE: usize = 128;

pub enum RequestStatus {
    Waiting,
    ConnectData(ConnectData),
    ResponseSent,
}

enum RequestProgress {
    Settings(SettingsState),
    Connect(ConnectState),
    Response(ResponseState),
}

pub struct RequestState {
    data_buf: Vec<u8>,
    progress: RequestProgress,
}

impl RequestState {
    pub fn update(
        &mut self,
        connection: &mut Connection,
    ) -> Result<RequestStatus, WebTransportError> {
        if let RequestProgress::Settings(ref mut state) = self.progress {
            if state.update(connection, &mut self.data_buf)? {
                self.progress = RequestProgress::Connect(ConnectState::new());
                self.data_buf.clear();
            }
        }

        if let RequestProgress::Connect(ref mut state) = self.progress {
            if let Some(data) = state.update(connection, &mut self.data_buf)? {
                self.progress = RequestProgress::Response(ResponseState::new(data.response_id));
                self.data_buf.clear();
                return Ok(RequestStatus::ConnectData(data));
            }
        }

        if let RequestProgress::Response(ref mut state) = self.progress {
            if state.update(connection, &mut self.data_buf)? {
                return Ok(RequestStatus::ResponseSent);
            }
        }

        Ok(RequestStatus::Waiting)
    }
}
