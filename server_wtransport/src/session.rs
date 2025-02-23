use std::io::Cursor;
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use http::StatusCode;
use quinn_proto::coding::Codec;
use quinn_proto::{Connection, SendDatagramError, VarInt};

use crate::outbound::Outbound;
use crate::webtransport::{Request, RequestState, WebTransportError};

/// The maximum of datagrams a Server will produce via `poll_transmit`
const MAX_DATAGRAMS: usize = 10;

/// The maximum number of transmit loop iterations for a single connection.
const MAX_TRANSMIT_OPS: usize = 3;

pub struct Session {
    pub(crate) inner: Connection,
    pub(crate) request: Request,
}

impl Session {
    pub fn recv_datagram(&mut self) -> Result<Option<Bytes>, WebTransportError> {
        let Some(mut bytes) = self.inner.datagrams().recv() else {
            return Ok(None);
        };
        let Some(completed) = self.request.completed() else {
            Err(WebTransportError::WebTransportNotConnected)?
        };

        let mut cursor = Cursor::new(&bytes);
        if VarInt::decode(&mut cursor)? != completed.session_id {
            return Err(WebTransportError::UnexpectedSessionId);
        }
        Ok(Some(bytes.split_off(cursor.position() as usize)))
    }

    pub fn send_datagram(
        &mut self,
        mut fill: impl FnMut(&mut [u8]) -> usize,
    ) -> Result<(), WebTransportError> {
        let Some(completed) = self.request.completed() else {
            Err(WebTransportError::WebTransportNotConnected)?
        };
        let Some(max_size) = self.inner.datagrams().max_size() else {
            Err(SendDatagramError::UnsupportedByPeer)?
        };

        let bytes = {
            let header = completed.datagram_header.as_ref();
            let mut buf = BytesMut::zeroed(max_size);
            buf[..header.len()].copy_from_slice(header);
            let written = fill(&mut buf[header.len()..]);
            buf.split_to(header.len() + written).into()
        };

        Ok(self.inner.datagrams().send(bytes, true)?)
    }

    pub(crate) fn handle_process(
        &mut self,
        now: Instant,
        buf: &mut Vec<u8>,
        outbound: &mut Outbound,
    ) {
        // Update the webtransport connection request state machine
        if self.inner.is_drained() == false {
            'wt: loop {
                match self.request.update(&mut self.inner) {
                    Ok(RequestState::ConnectData(url)) => {
                        println!("got connect data: {:?}", url);
                        _ = self.request.respond(StatusCode::OK);
                    }
                    Ok(RequestState::ResponseSent(id)) => {
                        // We've completed the connection
                        println!("got stream id: {:?}", id);
                    }
                    Ok(RequestState::Completed) | Ok(RequestState::Waiting) => {
                        // Nothing to do here
                        break 'wt;
                    }
                    Err(e) => {
                        println!("got error: {:?}", e);
                        break 'wt;
                    }
                }
            }
        }

        let mut transmit_ops = 0;

        loop {
            if let Some(transmit) = self.inner.poll_transmit(now, MAX_DATAGRAMS, buf) {
                outbound.push(transmit, buf);
                transmit_ops += 1;
            } else {
                // Nothing (left) to transmit, but still check timeouts
                transmit_ops = MAX_TRANSMIT_OPS;
            }

            // Do this after every transmit (as transmits affect timers), and at least once
            self.inner.handle_timeout(now);

            if transmit_ops >= MAX_TRANSMIT_OPS {
                break;
            }
        }
    }
}

// TODO
// pub fn close_recv_stream(recv_stream: &mut RecvStream) {
//     _ = recv_stream.stop(0u32.into()); // Ignore ClosedStream errors
// }
//
// pub fn close_send_stream(send_stream: &mut SendStream) {
//     match send_stream.finish() {
//         Ok(()) => (), // Everything worked fine
//         Err(FinishError::Stopped(reason)) => _ = send_stream.reset(reason),
//         Err(FinishError::ClosedStream) => (), // Already closed
//     }
// }
