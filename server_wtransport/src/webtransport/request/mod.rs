mod connect;
mod response;
mod settings;

use http::StatusCode;
use quinn_proto::coding::Codec;
use quinn_proto::{Connection, StreamId, VarInt};
use url::Url;

use crate::webtransport::WebTransportError;

use connect::Connect;
use response::Response;
use settings::Settings;

const DATA_BUFFER_SIZE: usize = 128;

pub enum RequestState {
    Waiting,
    ConnectData(Url),
    ResponseSent(StreamId),
    Completed,
}

pub struct Request {
    data_buf: Vec<u8>,
    inner: RequestInner,
}

enum RequestInner {
    Settings(Settings),
    Connect(Connect),
    Response(Response),
    Completed(Completed),
}

pub struct Completed {
    pub session_id: VarInt,
    pub datagram_header: Box<[u8]>,
}

impl Request {
    pub fn new() -> Self {
        Self {
            data_buf: Vec::with_capacity(DATA_BUFFER_SIZE),
            inner: RequestInner::Settings(Settings::new()),
        }
    }

    pub fn completed(&self) -> Option<&Completed> {
        match self.inner {
            RequestInner::Completed(ref completed) => Some(completed),
            _ => None,
        }
    }

    pub fn respond(&mut self, status: StatusCode) -> Result<(), WebTransportError> {
        match &mut self.inner {
            RequestInner::Response(r) => r.start_response(&mut self.data_buf, status),
            _ => Err(WebTransportError::NotReadyToRespond)?,
        }

        Ok(())
    }

    pub fn update(
        &mut self,
        connection: &mut Connection,
    ) -> Result<RequestState, WebTransportError> {
        if let RequestInner::Completed(_) = self.inner {
            return Ok(RequestState::Completed);
        }

        if let RequestInner::Settings(ref mut state) = self.inner {
            if state.update(connection, &mut self.data_buf)? {
                self.inner = RequestInner::Connect(Connect::new());
                self.data_buf.clear();
            }
        }

        if let RequestInner::Connect(ref mut state) = self.inner {
            if let Some((url, connection_id)) = state.update(connection, &mut self.data_buf)? {
                self.inner = RequestInner::Response(Response::new(connection_id));
                self.data_buf.clear();
                return Ok(RequestState::ConnectData(url));
            }
        }

        if let RequestInner::Response(ref mut state) = self.inner {
            if let Some(session_id) = state.update(connection, &self.data_buf)? {
                self.inner = RequestInner::Completed(Completed::new(session_id));
                self.data_buf.clear();
                return Ok(RequestState::ResponseSent(session_id));
            }
        }

        Ok(RequestState::Waiting)
    }
}

impl Completed {
    fn new(session_id: StreamId) -> Self {
        let mut header = Vec::with_capacity(1);
        let session_id = VarInt::from(session_id);
        session_id.encode(&mut header);
        debug_assert_eq!(header.len(), 1);

        Self {
            session_id,
            datagram_header: header.into_boxed_slice(),
        }
    }
}
