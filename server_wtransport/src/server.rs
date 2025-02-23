use std::collections::HashMap;
use std::io::IoSliceMut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use http::StatusCode;
use quinn_proto::crypto::rustls::QuicServerConfig;
use quinn_proto::{
    AckFrequencyConfig, Connection, ConnectionError, ConnectionHandle, DatagramEvent, Datagrams,
    Endpoint, EndpointConfig, EndpointEvent, IdleTimeout, Incoming, ServerConfig, Transmit, VarInt,
};
use quinn_udp::RecvMeta;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::util;

use crate::webtransport::{Request, RequestState};

/// The HTTP/3 ALPN is required when negotiating a QUIC connection.
pub const ALPN: &[u8] = b"h3";

/// The maximum of datagrams a Server will produce via `poll_transmit`
const MAX_DATAGRAMS: usize = 10;

/// The maximum number of transmit loop iterations for a single connection.
const MAX_TRANSMIT_OPS: usize = 3;

/// Maximum ack delay (adjust for longer periods between recv calls).
const MAX_ACK_DELAY: Duration = Duration::from_millis(50);

/// Max idle timeout for clients.
const MAX_IDLE_TIMEOUT_MS: VarInt = VarInt::from_u32(10_000);

#[cfg(target_os = "linux")]
const BATCH_COUNT: usize = 32;
#[cfg(target_os = "windows")]
const BATCH_COUNT: usize = 1;

pub struct Server {
    endpoint: Endpoint,
    outbound: Vec<(Transmit, Bytes)>,
    connections: HashMap<ConnectionHandle, ConnectionWrapper>,
    endpoint_events: Vec<(ConnectionHandle, EndpointEvent)>,

    buf: Vec<u8>, // Reusable byte buffer to save on allocations
}

struct ConnectionWrapper {
    inner: Connection,
    request: Request,
}

impl Server {
    pub fn new(
        certs: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self, rustls::Error> {
        let mut server_config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(certs, key)?;

        server_config.alpn_protocols = vec![ALPN.to_vec()]; // Must set the proper protocol

        let endpoint_config = EndpointConfig::default();
        let server_config: QuicServerConfig = server_config.try_into().unwrap();
        let mut server_config = ServerConfig::with_crypto(Arc::new(server_config));
        let mut ack_frequency_config = AckFrequencyConfig::default();
        let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();

        ack_frequency_config.max_ack_delay(Some(MAX_ACK_DELAY));
        transport_config.max_idle_timeout(Some(IdleTimeout::from(MAX_IDLE_TIMEOUT_MS)));
        transport_config.allow_spin(true);

        let server = Server {
            endpoint: Endpoint::new(
                Arc::new(endpoint_config),
                Some(Arc::new(server_config)),
                true,
                None,
            ),
            outbound: Vec::new(),
            connections: HashMap::new(),
            endpoint_events: Vec::new(),
            buf: Vec::new(),
        };

        Ok(server)
    }

    pub fn create_buffers(
        &self,
        gro_segments: usize,
    ) -> ([IoSliceMut<'static>; BATCH_COUNT], [RecvMeta; BATCH_COUNT]) {
        let max_udp_payload_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        let chunk_size = gro_segments * max_udp_payload_size.min(u16::MAX.into());
        let total_bytes = chunk_size * BATCH_COUNT;

        let buf = Box::leak(vec![0; total_bytes].into_boxed_slice());
        let mut chunks = buf.chunks_mut(chunk_size).map(IoSliceMut::new);

        let iovs = std::array::from_fn(|_| chunks.next().unwrap());
        let metas = [RecvMeta::default(); BATCH_COUNT];

        (iovs, metas)
    }

    pub fn handle_recv(&mut self, now: Instant, meta: &RecvMeta, data: BytesMut) {
        self.prepare_response_buf();

        let event = self.endpoint.handle(
            now,
            meta.addr,
            meta.dst_ip,
            meta.ecn.map(util::proto_ecn),
            data,
            &mut self.buf,
        );

        match event {
            Some(DatagramEvent::NewConnection(incoming)) => {
                if let Err(e) = self.try_accept(incoming, now) {
                    println!("try_accept err: {:?}", e);
                }
            }
            Some(DatagramEvent::ConnectionEvent(connection_handle, event)) => {
                if let Some(connection) = self.connections.get_mut(&connection_handle) {
                    connection.inner.handle_event(event);
                } else {
                    println!("missing connection: {:?}", connection_handle);
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                push_transmit(transmit, &mut self.buf, &mut self.outbound);
            }
            _ => {} // There may be no event to handle
        }
    }

    pub fn handle_process(&mut self, now: Instant) {
        self.prepare_response_buf();

        for (connection_handle, connection) in &mut self.connections {
            connection.handle_process(now, &mut self.buf, &mut self.outbound);

            while let Some(event) = connection.inner.poll_endpoint_events() {
                self.endpoint_events.push((*connection_handle, event));
            }
        }

        for (connection_handle, event) in self.endpoint_events.drain(..) {
            let is_drained = event.is_drained();

            if is_drained {
                println!("draining: {:?}", connection_handle);
                self.connections.remove(&connection_handle);
            }

            if let Some(event) = self.endpoint.handle_event(connection_handle, event) {
                debug_assert!(!is_drained, "drained event yielded response");
                if let Some(connection) = self.connections.get_mut(&connection_handle) {
                    connection.inner.handle_event(event);
                }
            }
        }
    }

    pub fn connections_mut(
        &mut self,
    ) -> impl Iterator<Item = (&ConnectionHandle, &mut Connection)> {
        self.connections.iter_mut().map(|(h, c)| (h, &mut c.inner))
    }

    pub fn _incoming(&mut self) -> impl Iterator<Item = (ConnectionHandle, Datagrams<'_>)> {
        self.connections
            .iter_mut()
            .map(|(connection_handle, connection)| {
                (*connection_handle, connection.inner.datagrams())
            })
    }

    pub fn outgoing(&mut self) -> impl Iterator<Item = (Transmit, Bytes)> + '_ {
        self.outbound.drain(..)
    }

    pub fn compute_next_timeout(&mut self) -> Option<Instant> {
        let mut min: Option<Instant> = None;

        for (_, connection) in self.connections.iter_mut() {
            if let Some(timeout) = connection.inner.poll_timeout().as_mut() {
                match min.as_mut() {
                    Some(min) => *min = *min.min(timeout),
                    None => min = Some(*timeout),
                }
            }
        }

        min
    }

    fn try_accept(
        &mut self,
        incoming: Incoming,
        now: Instant,
    ) -> Result<ConnectionHandle, ConnectionError> {
        // We don't need the original data and can reuse the buffer here.
        self.buf.clear();

        let result = self.endpoint.accept(incoming, now, &mut self.buf, None);

        match result {
            Ok((connection_handle, connection)) => {
                // Created a new connection -- store it in the hashmap
                self.connections.insert(
                    connection_handle,
                    ConnectionWrapper {
                        inner: connection,
                        request: Request::new(),
                    },
                );

                println!("accepted {:?}", connection_handle);

                Ok(connection_handle)
            }
            Err(error) => {
                // Failed to accept the connection -- possibly transmit back a response
                if let Some(transmit) = error.response {
                    push_transmit(transmit, &mut self.buf, &mut self.outbound);
                }

                Err(error.cause)
            }
        }
    }

    fn prepare_response_buf(&mut self) {
        self.buf.clear();
        self.buf
            .reserve(self.endpoint.config().get_max_udp_payload_size() as usize);
    }
}

impl ConnectionWrapper {
    fn handle_process(
        &mut self,
        now: Instant,
        buf: &mut Vec<u8>,
        outbound: &mut Vec<(Transmit, Bytes)>,
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
                        println!("got stream id: {:?}", id);
                        // TODO
                    }
                    Ok(RequestState::Finished) | Ok(RequestState::Waiting) => {
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
                push_transmit(transmit, buf, outbound);
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

// Pushes a transmit to the outbound buffer, splitting it if necessary
fn push_transmit(transmit: Transmit, buf: &mut Vec<u8>, outbound: &mut Vec<(Transmit, Bytes)>) {
    let mut buffer = Bytes::copy_from_slice(&buf[..transmit.size]);

    match transmit.segment_size {
        // No separate segments -- just push the whole thing
        None => outbound.push((transmit, buffer)),

        // Multiple segments, so split and push them as sub-transmits
        Some(segment_size) => {
            while buffer.is_empty() == false {
                let end = segment_size.min(buffer.len());
                let contents = buffer.split_to(end);

                outbound.push((
                    Transmit {
                        destination: transmit.destination,
                        size: contents.len(),
                        ecn: transmit.ecn,
                        segment_size: None,
                        src_ip: transmit.src_ip,
                    },
                    contents,
                ));
            }
        }
    };

    buf.clear();
}

// // TODO: Wrap in connection?
// pub fn close_recv_stream(recv_stream: &mut RecvStream) {
//     _ = recv_stream.stop(0u32.into()); // Ignore ClosedStream errors
// }
//
// // TODO: Wrap in connection?
// pub fn close_send_stream(send_stream: &mut SendStream) {
//     match send_stream.finish() {
//         Ok(()) => (), // Everything worked fine
//         Err(FinishError::Stopped(reason)) => _ = send_stream.reset(reason),
//         Err(FinishError::ClosedStream) => (), // Already closed
//     }
// }
