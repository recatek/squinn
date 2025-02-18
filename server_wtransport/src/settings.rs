use std::io::Cursor;

use quinn_proto::{Connection, Dir, StreamId};
use thiserror::Error;
use web_transport_proto::{Settings as ProtoSettings, SettingsError as ProtoSettingsError};

/// Pre-encoded settings frame with enable_webtransport set to 1.
/// Validated in debug at runtime with check_settings_encoded().
const SETTINGS_ENCODED: [u8; 31] = [
    0, 4, 28, 171, 96, 55, 67, 1, 128, 255, 210, 119, 1, 8, 1, 51, 1, 192, 0, 0, 0, 198, 113, 112,
    106, 1, 171, 96, 55, 66, 1,
];

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
}

impl Default for Settings {
    fn default() -> Self {
        debug_assert!(check_settings_encoded());

        Settings {
            send_done: false,
            send_id: None,
            send_bytes: 0,

            recv_done: false,
            recv_id: None,
            recv_data: Vec::new(), // TODO: Add a capacity here
        }
    }
}

impl Settings {
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
            let mut send_stream = connection.send_stream(send_id);
            self.send_bytes += send_stream.write(&SETTINGS_ENCODED[self.send_bytes..])?;

            return match self.send_bytes >= SETTINGS_ENCODED.len() {
                true => Ok(true),
                false => Ok(false), // Keep trying
            };
        }

        Ok(false) // Keep trying
    }

    fn try_recv(&mut self, connection: &mut Connection) -> Result<bool, SettingsError> {
        if self.recv_id.is_none() {
            self.recv_id = connection.streams().accept(Dir::Uni);
        }

        if let Some(recv_id) = self.recv_id {
            let mut recv_stream = connection.recv_stream(recv_id);
            let recv_chunk = recv_stream
                .read(true)
                .map_err(|_| SettingsError::UnexpectedEnd)?
                .next(usize::MAX)?
                .ok_or(SettingsError::UnexpectedEnd)?;
            self.recv_data.extend_from_slice(&recv_chunk.bytes);

            return match ProtoSettings::decode(&mut Cursor::new(&self.recv_data)) {
                Ok(settings) => match settings.supports_webtransport() {
                    1 => Ok(true), // We're done!
                    _ => Err(SettingsError::WebTransportUnsupported),
                },
                Err(ProtoSettingsError::UnexpectedEnd) => Ok(false), // Keep trying
                Err(e) => Err(e.into()),
            };
        }

        Ok(false) // Keep trying
    }
}

fn check_settings_encoded() -> bool {
    let mut settings = ProtoSettings::default();
    settings.enable_webtransport(1);

    let mut buf = Vec::<u8>::new();
    settings.encode(&mut buf);

    &buf == &SETTINGS_ENCODED
}
