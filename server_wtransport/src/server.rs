use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use quinn_proto::crypto::rustls::QuicServerConfig;
use quinn_proto::{
    AckFrequencyConfig, ConnectionError, ConnectionHandle, DatagramEvent, Endpoint, EndpointConfig,
    EndpointEvent, IdleTimeout, Incoming, ServerConfig, Transmit, VarInt,
};
use quinn_udp::RecvMeta;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::outbound::Outbound;
use crate::session::Session;
use crate::util;
use crate::webtransport::Request;

/// The HTTP/3 ALPN is required when negotiating a QUIC connection.
pub const ALPN: &[u8] = b"h3";

/// Maximum ack delay (adjust for longer periods between recv calls).
const MAX_ACK_DELAY: Duration = Duration::from_millis(50);

/// Max idle timeout for clients.
const MAX_IDLE_TIMEOUT_MS: VarInt = VarInt::from_u32(10_000);

pub struct Server {
    endpoint: Endpoint,
    outbound: Outbound,
    connections: HashMap<ConnectionHandle, Session>,
    endpoint_events: Vec<(ConnectionHandle, EndpointEvent)>,

    buf: Vec<u8>, // Reusable byte buffer to save on allocations
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
            outbound: Outbound::new(),
            connections: HashMap::new(),
            endpoint_events: Vec::new(),
            buf: Vec::new(),
        };

        Ok(server)
    }

    pub fn get_max_udp_payload_size(&self) -> u64 {
        self.endpoint.config().get_max_udp_payload_size()
    }

    pub fn handle_recv(&mut self, now: Instant, data: BytesMut, meta: &RecvMeta) {
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
                self.outbound.push(transmit, &mut self.buf);
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

    pub fn sessions_mut(&mut self) -> impl Iterator<Item = (&ConnectionHandle, &mut Session)> {
        self.connections.iter_mut()
    }

    pub fn outgoing(&mut self) -> impl Iterator<Item = (Transmit, Bytes)> + '_ {
        self.outbound.0.drain(..)
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
                    Session {
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
                    self.outbound.push(transmit, &mut self.buf);
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
