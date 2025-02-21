use std::io::Cursor;
use std::sync::LazyLock;

use quinn_proto::{Connection, Dir, StreamId};
use web_transport_proto::{Settings as SettingsData, SettingsError};

use crate::webtransport::{WebTransportError};
use crate::server::{close_send_stream, close_recv_stream};

// This never changes at runtime, but is nondeterministic due to being HashMap-based.
const SETTINGS_ENCODED: LazyLock<&[u8]> = LazyLock::new(|| encode_settings());

#[derive(Default)]
pub struct Settings {
    send_done: bool,
    recv_done: bool,

    send_id: Option<StreamId>,
    recv_id: Option<StreamId>,

    send_bytes: usize,
}

#[allow(dead_code)]
impl Settings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(
        &mut self,
        connection: &mut Connection,
        recv_buf: &mut Vec<u8>,
    ) -> Result<bool, WebTransportError> {
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
                close_send_stream(&mut send_stream);
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

            return match SettingsData::decode(&mut Cursor::new(&recv_buf)) {
                Err(SettingsError::UnexpectedEnd) => Ok(false), // Keep trying
                Err(e) => Err(e.into()), // No close -- we'll nuke the connection

                // Got what we wanted here
                Ok(settings) => {
                    close_recv_stream(&mut recv_stream);
                    match settings.supports_webtransport() {
                        0 => Err(WebTransportError::WebTransportUnsupported),
                        _ => Ok(true), // We're done!
                    }
                }
            };
        }

        Ok(false) // Keep trying
    }
}

fn encode_settings() -> &'static [u8] {
    let mut settings = SettingsData::default();
    settings.enable_webtransport(1);

    let mut buf = Vec::<u8>::new();
    settings.encode(&mut buf);

    Box::leak(buf.into_boxed_slice())
}
