use std::collections::{HashMap, VecDeque};
use std::io::IoSliceMut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use quinn_proto::{
    AckFrequencyConfig, Connection, ConnectionError, ConnectionEvent, ConnectionHandle,
    DatagramEvent, Datagrams, Endpoint, EndpointConfig, EndpointEvent, IdleTimeout, Incoming,
    ServerConfig, Transmit, VarInt,
};
use quinn_udp::RecvMeta;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

use crate::util;

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
    connections: HashMap<ConnectionHandle, Connection>,
    connection_events: HashMap<ConnectionHandle, VecDeque<ConnectionEvent>>,
    endpoint_events: Vec<(ConnectionHandle, EndpointEvent)>,

    response_buf: Vec<u8>, // Reusable byte buffer to save on allocations
}

impl Server {
    pub fn new() -> Result<(Self, CertificateDer<'static>), rustls::Error> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let cert_der_vec = vec![cert_der.clone()];
        let private_key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

        let endpoint_config = EndpointConfig::default();
        let mut server_config = ServerConfig::with_single_cert(cert_der_vec, private_key.into())?;
        let mut ack_frequency_config = AckFrequencyConfig::default();
        let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();

        ack_frequency_config.max_ack_delay(Some(MAX_ACK_DELAY));
        transport_config.max_concurrent_uni_streams(0_u8.into());
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
            response_buf: Vec::new(),
        };

        Ok((server, cert_der))
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
            &mut self.response_buf,
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
                let send_buf = &self.response_buf[..transmit.size];
                self.outbound.extend(split_transmit(transmit, send_buf));
            }
            _ => {} // There may be no event to handle
        }
    }

    pub fn handle_process(&mut self, now: Instant) {
        let max = MAX_DATAGRAMS;

        self.prepare_response_buf();

        for (connection_handle, connection) in &mut self.connections {
            connection.handle_timeout(now);

            for (_, mut events) in self.connection_events.drain() {
                for event in events.drain(..) {
                    connection.handle_event(event);
                }
            }

            while let Some(event) = connection.poll_endpoint_events() {
                self.endpoint_events.push((*connection_handle, event));
            }

            while let Some(transmit) = connection.poll_transmit(now, max, &mut self.response_buf) {
                let send_buf = &self.response_buf[..transmit.size];
                self.outbound.extend(split_transmit(transmit, send_buf));

                self.response_buf.clear(); // Need to clear here to reuse the buffer
            }
        }

        for (connection_handle, event) in self.endpoint_events.drain(..) {
            let is_drained = event.is_drained();

            if is_drained {
                println!("draining: {:?}", connection_handle);
                self.connections.remove(&connection_handle);
            }

            if let Some(event) = self.endpoint.handle_event(connection_handle, event) {
                debug_assert_eq!(is_drained, false, "drained event yielded response");
                if let Some(connection) = self.connections.get_mut(&connection_handle) {
                    connection.handle_event(event);
                }
            }
        }
    }

    pub fn connections_mut(
        &mut self,
    ) -> impl Iterator<Item = (&ConnectionHandle, &mut Connection)> {
        self.connections.iter_mut()
    }

    pub fn incoming(&mut self) -> impl Iterator<Item = (ConnectionHandle, Datagrams<'_>)> {
        self.connections
            .iter_mut()
            .map(|(connection_handle, connection)| (*connection_handle, connection.datagrams()))
    }

    pub fn outgoing(&mut self) -> impl Iterator<Item = (Transmit, Bytes)> + '_ {
        self.outbound.drain(..)
    }

    pub fn compute_next_timeout(&mut self) -> Option<Duration> {
        let mut min: Option<Instant> = None;

        for (_, connection) in self.connections.iter_mut() {
            if let Some(timeout) = connection.poll_timeout().as_mut() {
                match min.as_mut() {
                    Some(min) => *min = *min.min(timeout),
                    None => min = Some(*timeout),
                }
            }
        }

        min.and_then(|min| min.checked_duration_since(Instant::now()))
    }

    fn try_accept(
        &mut self,
        incoming: Incoming,
        now: Instant,
    ) -> Result<ConnectionHandle, ConnectionError> {
        // We don't need the original data and can reuse the buffer here.
        self.response_buf.clear();

        let result = self
            .endpoint
            .accept(incoming, now, &mut self.response_buf, None);

        match result {
            Ok((connection_handle, connection)) => {
                // Created a new connection -- store it in the hashmap
                self.connections.insert(connection_handle, connection);
                println!("accepted {:?}", connection_handle);

                Ok(connection_handle)
            }
            Err(error) => {
                // Failed to accept the connection -- possibly transmit back a response
                if let Some(transmit) = error.response {
                    let size = transmit.size;
                    self.outbound
                        .extend(split_transmit(transmit, &self.response_buf[..size]));
                }

                Err(error.cause)
            }
        }
    }

    fn prepare_response_buf(&mut self) {
        self.response_buf.clear();

        let max_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        if self.response_buf.capacity() < max_size {
            self.response_buf
                .reserve(max_size - self.response_buf.capacity());
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
