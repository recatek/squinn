use quinn_proto::{Connection, Dir, StreamId};
use std::io::Cursor;
use thiserror::Error;
use web_transport_proto::{Settings as ProtoSettings, SettingsError as ProtoSettingsError};

#[derive(Error, Debug, Clone)]
pub enum SettingsError {
    #[error("quic stream was closed early")]
    UnexpectedEnd,
    #[error("webtransport is not supported")]
    WebTransportUnsupported,

    #[error("read error: {0}")]
    ReadError(#[from] quinn_proto::ReadError),
    #[error("write error: {0}")]
    WriteError(#[from] quinn_proto::WriteError),
    #[error("protocol error: {0}")]
    ProtoError(#[from] web_transport_proto::SettingsError),
}

pub struct Settings {
    send_done: bool,
    send_id: Option<StreamId>,
    send_bytes: usize, // How much of our settings data we've successfully sent

    recv_done: bool,
    recv_id: Option<StreamId>,
    recv_data: Vec<u8>,

    settings_data: &'static [u8],
}

// https://github.com/kixelated/web-transport-rs/blob/main/web-transport-quinn/src/settings.rs#L27
impl Settings {
    pub fn new(settings_data: &'static [u8]) -> Self {
        Settings {
            send_done: false,
            send_id: None,
            send_bytes: 0,

            recv_done: false,
            recv_id: None,
            recv_data: Vec::new(), // TODO: Add a capacity here

            settings_data,
        }
    }

    pub fn init_settings_data() -> &'static [u8] {
        let mut settings = ProtoSettings::default();
        settings.enable_webtransport(1);
        let mut buf = Vec::<u8>::new();
        settings.encode(&mut buf);
        Box::leak(buf.into_boxed_slice()).as_ref()
    }

    pub fn update(&mut self, connection: &mut Connection) -> Result<bool, SettingsError> {
        if self.send_done == false {
            self.send_done |= self.try_send(connection)?;
        }

        if self.recv_done == false {
            self.recv_done |= self.try_recv(connection)?;
        }

        Ok(self.send_done && self.recv_done)
    }

    fn try_send(&mut self, connection: &mut Connection) -> Result<bool, SettingsError> {
        if self.send_id.is_none() {
            self.send_id = connection.streams().open(Dir::Uni);
        }

        if let Some(send_id) = self.send_id {
            let sent = connection
                .send_stream(send_id)
                .write(&self.settings_data[self.send_bytes..])?;
            self.send_bytes += sent;

            if self.send_bytes >= self.settings_data.len() {
                return Ok(true);
            }
        }

        Ok(false) // Keep trying
    }

    fn try_recv(&mut self, connection: &mut Connection) -> Result<bool, SettingsError> {
        if self.recv_id.is_none() {
            self.recv_id = connection.streams().accept(Dir::Uni);
        }

        if let Some(recv_id) = self.recv_id {
            let mut recv_stream = connection.recv_stream(recv_id);
            let mut chunks = recv_stream
                .read(true)
                .map_err(|_| SettingsError::UnexpectedEnd)?;
            let chunk = chunks
                .next(usize::MAX)?
                .ok_or(SettingsError::UnexpectedEnd)?;
            self.recv_data.extend_from_slice(&chunk.bytes);

            return match ProtoSettings::decode(&mut Cursor::new(&self.recv_data)) {
                Ok(settings) => match settings.supports_webtransport() {
                    1 => Ok(true), // We're done!
                    _ => Err(SettingsError::WebTransportUnsupported),
                },
                Err(ProtoSettingsError::UnexpectedEnd) => Ok(false), // Keep reading
                Err(e) => Err(e.into()),
            };
        }

        Ok(false) // Keep trying
    }
}
