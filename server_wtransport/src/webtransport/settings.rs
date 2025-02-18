use std::io::Cursor;

use quinn_proto::{Connection, Dir, StreamId};
use web_transport_proto::{Settings, SettingsError};

use crate::webtransport::WebTransportError;

/// Pre-encoded settings frame with enable_webtransport set to 1.
/// Validated in debug at runtime with check_settings_encoded().
const SETTINGS_ENCODED: [u8; 31] = [
    0, 4, 28, 171, 96, 55, 67, 1, 128, 255, 210, 119, 1, 8, 1, 51, 1, 192, 0, 0, 0, 198, 113, 112,
    106, 1, 171, 96, 55, 66, 1,
];

#[derive(Default)]
pub struct SettingsState {
    send_done: bool,
    send_id: Option<StreamId>,
    send_bytes: usize,

    recv_done: bool,
    recv_id: Option<StreamId>,
}

#[allow(dead_code)]
impl SettingsState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(
        &mut self,
        connection: &mut Connection,
        recv_buf: &mut Vec<u8>,
    ) -> Result<bool, WebTransportError> {
        debug_assert!(check_settings_encoded());

        if self.send_done == false {
            self.send_done |= self.try_send(connection)?;
        }

        if self.recv_done == false {
            self.recv_done |= self.try_recv(connection, recv_buf)?;
        }

        Ok(self.send_done && self.recv_done)
    }

    fn try_send(&mut self, connection: &mut Connection) -> Result<bool, WebTransportError> {
        debug_assert!(self.send_done == false);

        if self.send_id.is_none() {
            self.send_id = connection.streams().open(Dir::Uni);
        }

        if let Some(send_id) = self.send_id {
            // We don't close on error since we'll just nuke the connection
            let mut send_stream = connection.send_stream(send_id);
            self.send_bytes += send_stream.write(&SETTINGS_ENCODED[self.send_bytes..])?;

            if self.send_bytes >= SETTINGS_ENCODED.len() {
                super::close_send_stream(&mut send_stream);
                return Ok(true);
            }
        }

        Ok(false) // Keep trying
    }

    fn try_recv(
        &mut self,
        connection: &mut Connection,
        recv_buf: &mut Vec<u8>,
    ) -> Result<bool, WebTransportError> {
        debug_assert!(self.recv_done == false);

        if self.recv_id.is_none() {
            self.recv_id = connection.streams().accept(Dir::Uni);
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

            return match Settings::decode(&mut Cursor::new(&recv_buf)) {
                Err(SettingsError::UnexpectedEnd) => Ok(false), // Keep trying
                Err(e) => Err(e.into()), // No close -- we'll nuke the connection

                // Got what we wanted here
                Ok(settings) => {
                    super::close_recv_stream(&mut recv_stream);
                    match settings.supports_webtransport() {
                        0 => Err(WebTransportError::WebTransportUnsupported),
                        _ => Ok(true), // We're done!
                    }
                },
            };
        }

        Ok(false) // Keep trying
    }
}

fn check_settings_encoded() -> bool {
    let mut settings = Settings::default();
    settings.enable_webtransport(1);

    let mut buf = Vec::<u8>::new();
    settings.encode(&mut buf);

    &buf == &SETTINGS_ENCODED
}
