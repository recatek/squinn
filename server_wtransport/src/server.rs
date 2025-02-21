use std::collections::{HashMap, VecDeque};
use std::io::IoSliceMut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use http::StatusCode;
use quinn_proto::crypto::rustls::QuicServerConfig;
use quinn_proto::{
    AckFrequencyConfig, Connection, ConnectionError, ConnectionEvent, ConnectionHandle,
    DatagramEvent, Datagrams, Endpoint, EndpointConfig, EndpointEvent, FinishError, IdleTimeout,
    Incoming, RecvStream, SendStream, ServerConfig, Transmit, VarInt,
};
use quinn_udp::RecvMeta;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::util;

use crate::webtransport::{Request, RequestState};

/// The HTTP/3 ALPN is required when negotiating a QUIC connection.
pub const ALPN: &[u8] = b"h3";

/// The maximum of datagrams a Server will produce via `poll_transmit`
const MAX_DATAGRAMS: usize = 10;

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
    outbound: VecDeque<(Transmit, Bytes)>,
    connections: HashMap<ConnectionHandle, ConnectionWrapper>,
    connection_events: HashMap<ConnectionHandle, VecDeque<ConnectionEvent>>,
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
            outbound: VecDeque::new(),
            connections: HashMap::new(),
            connection_events: HashMap::new(),
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
                self.connection_events
                    .entry(connection_handle)
                    .or_default()
                    .push_back(event);
            }
            Some(DatagramEvent::Response(transmit)) => {
                let send_buf = &self.buf[..transmit.size];
                self.outbound.extend(split_transmit(transmit, send_buf));
            }
            _ => {} // There may be no event to handle
        }
    }

    pub fn handle_process(&mut self, now: Instant) {
        let max = MAX_DATAGRAMS;

        self.prepare_response_buf();

        for (connection_handle, connection) in &mut self.connections {
            // TODO: Quinn does this after transmit (in a ping-pong between the two), see below...
            connection.inner.handle_timeout(now);

            for (_, mut events) in self.connection_events.drain() {
                for event in events.drain(..) {
                    connection.inner.handle_event(event);
                }
            }

            if connection.inner.is_drained() == false {
                'wt: loop {
                    println!("webtransport...");
                    match connection.request.update(&mut connection.inner) {
                        Ok(RequestState::ConnectData(url)) => {
                            println!("got connect data: {:?}", url);
                            _ = connection.request.respond(StatusCode::OK);
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

            while let Some(event) = connection.inner.poll_endpoint_events() {
                self.endpoint_events.push((*connection_handle, event));
            }

            // TODO: Transmit may reset timers, and timers may cause more to reset.
            //       Look at the drive_timer function in quinn's connection.rs for more info.
            while let Some(transmit) = connection.inner.poll_transmit(now, max, &mut self.buf) {
                let send_buf = &self.buf[..transmit.size];
                self.outbound.extend(split_transmit(transmit, send_buf));

                self.buf.clear(); // Need to clear here to reuse the buffer
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
                    let size = transmit.size;
                    self.outbound
                        .extend(split_transmit(transmit, &self.buf[..size]));
                }

                Err(error.cause)
            }
        }
    }

    fn prepare_response_buf(&mut self) {
        self.buf.clear();

        let max_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        if self.buf.capacity() < max_size {
            self.buf.reserve(max_size - self.buf.capacity());
        }
    }
}

fn split_transmit(transmit: Transmit, buffer: &[u8]) -> Vec<(Transmit, Bytes)> {
    let mut buffer = Bytes::copy_from_slice(buffer);
    let segment_size = match transmit.segment_size {
        Some(segment_size) => segment_size,
        _ => return vec![(transmit, buffer)],
    };

    let mut transmits = Vec::new();
    while buffer.is_empty() == false {
        let end = segment_size.min(buffer.len());
        let contents = buffer.split_to(end);

        transmits.push((
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

    transmits
}

// TODO: Wrap in connection?
pub fn close_recv_stream(recv_stream: &mut RecvStream) {
    _ = recv_stream.stop(0u32.into()); // Ignore ClosedStream errors
}

// TODO: Wrap in connection?
pub fn close_send_stream(send_stream: &mut SendStream) {
    match send_stream.finish() {
        Ok(()) => (), // Everything worked fine
        Err(FinishError::Stopped(reason)) => _ = send_stream.reset(reason),
        Err(FinishError::ClosedStream) => (), // Already closed
    }
}
