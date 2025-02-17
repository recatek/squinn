use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use mio::{net::UdpSocket, Events, Interest, Poll, Token};
use quinn_proto::*;
use quinn_udp::{RecvMeta, UdpSocketState};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

/// The maximum of datagrams a Server will produce via `poll_transmit`
const MAX_DATAGRAMS: usize = 10;

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

fn main() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 53423);

    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(64);

    let mut sock_mio = UdpSocket::bind(addr).unwrap();
    let sock_quic = UdpSocketState::new((&sock_mio).into()).unwrap();

    #[cfg(target_os = "windows")]
    sock_quic.set_gro((&sock_mio).into(), true).unwrap();
    #[cfg(target_os = "linux")]
    sock_quic.set_enable_rx_timestamps((&sock_mio).into(), true).unwrap();

    poll.registry()
        .register(&mut sock_mio, Token(0), Interest::READABLE)
        .unwrap();

    let endpoint_config = EndpointConfig::default();
    let (server_config, _cert) = configure_server().expect("failed to configure server");

    let mut server = Server {
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

    let max_udp_payload_size = server.endpoint.config().get_max_udp_payload_size() as usize;
    let (mut iovs, mut metas) = create_buffers(sock_quic.gro_segments(), max_udp_payload_size);

    loop {
        let next_timeout = server.compute_next_timeout();
        poll.poll(&mut events, next_timeout).unwrap();

        let now = Instant::now();
        let sys_now: Duration = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        while events.is_empty() == false {
            match sock_mio.try_io(|| sock_quic.recv((&sock_mio).into(), &mut iovs, &mut metas)) {
                Ok(count) => {
                    for (meta, buf) in metas.iter().zip(iovs.iter()).take(count) {
                        // Offset the receipt time by the packet timestamp
                        let now = match meta.timestamp {
                            Some(rx) => now - sys_now.saturating_sub(rx),
                            None => now,
                        };

                        let mut data: BytesMut = buf[0..meta.len].into();
                        while data.is_empty() == false {
                            let buf = data.split_to(meta.stride.min(data.len()));
                            println!("recv: {}B", buf.len());
                            server.handle_recv(now, meta, buf)
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::ConnectionReset => continue,
                Err(e) => panic!("recv error: {:?}", e),
            }
        }

        server.handle_process(now);

        // Get all the datagrams and do stuff with them
        //for (connection_handle, mut datagrams) in server.incoming() {
        for (connection_handle, connection) in server.connections.iter_mut() {
            let rtt = connection.rtt();
            let tx_bytes = connection.stats().udp_tx.bytes;
            let mut datagrams = connection.datagrams();
            let mut should_ping = false;
            while let Some(bytes) = datagrams.recv() {
                println!(
                    "received datagram '{}' from {:?} (rtt: {}, tx: {})",
                    str::from_utf8(&bytes).unwrap(),
                    connection_handle,
                    rtt.as_millis(),
                    tx_bytes
                );
                should_ping = true;
            }

            if should_ping {
                connection.ping();
            }
        }

        // Send all the outgoing traffic
        for (transmit, buffer) in server.outgoing() {
            let transmit = quinn_udp::Transmit {
                destination: transmit.destination,
                ecn: transmit.ecn.map(udp_ecn),
                contents: &buffer,
                segment_size: transmit.segment_size,
                src_ip: transmit.src_ip,
            };

            println!("send: {}B", buffer.len());
            match sock_mio.try_io(|| sock_quic.try_send((&sock_mio).into(), &transmit)) {
                Ok(()) => (),
                Err(e) => println!("send error: {:?}", e),
            }
        }
    }
}

impl Server {
    pub fn handle_recv(&mut self, now: Instant, meta: &RecvMeta, data: BytesMut) {
        self.prepare_response_buf();

        let event = self.endpoint.handle(
            now,
            meta.addr,
            meta.dst_ip,
            meta.ecn.map(proto_ecn),
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
            if event.is_drained() {
                println!("disconnecting: {:?}", connection_handle);
                self.connections.remove(&connection_handle);
            }

            if let Some(event) = self.endpoint.handle_event(connection_handle, event) {
                if let Some(connection) = self.connections.get_mut(&connection_handle) {
                    connection.handle_event(event);
                }
            }
        }
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

fn configure_server() -> Result<(ServerConfig, CertificateDer<'static>), rustls::Error> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let private_key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

    let mut server_config =
        ServerConfig::with_single_cert(vec![cert_der.clone()], private_key.into())?;

    let mut ack_frequency_config = AckFrequencyConfig::default();
    ack_frequency_config.max_ack_delay(Some(Duration::from_millis(50)));

    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    transport_config.max_concurrent_uni_streams(0_u8.into());
    transport_config.max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(10_000))));
    transport_config.allow_spin(true);

    Ok((server_config, cert_der))
}

pub fn create_buffers(
    gro_segments: usize,
    max_udp_payload_size: usize,
) -> ([IoSliceMut<'static>; BATCH_COUNT], [RecvMeta; BATCH_COUNT]) {
    let chunk_size = gro_segments * max_udp_payload_size.min(u16::MAX.into());
    let total_bytes = chunk_size * BATCH_COUNT;

    let buf = Box::leak(vec![0; total_bytes].into_boxed_slice());
    let mut chunks = buf.chunks_mut(chunk_size).map(IoSliceMut::new);

    let iovs = std::array::from_fn(|_| chunks.next().unwrap());
    let metas = [RecvMeta::default(); BATCH_COUNT];

    (iovs, metas)
}

#[inline]
fn proto_ecn(ecn: quinn_udp::EcnCodepoint) -> EcnCodepoint {
    match ecn {
        quinn_udp::EcnCodepoint::Ect0 => EcnCodepoint::Ect0,
        quinn_udp::EcnCodepoint::Ect1 => EcnCodepoint::Ect1,
        quinn_udp::EcnCodepoint::Ce => EcnCodepoint::Ce,
    }
}

#[inline]
fn udp_ecn(ecn: EcnCodepoint) -> quinn_udp::EcnCodepoint {
    match ecn {
        EcnCodepoint::Ect0 => quinn_udp::EcnCodepoint::Ect0,
        EcnCodepoint::Ect1 => quinn_udp::EcnCodepoint::Ect1,
        EcnCodepoint::Ce => quinn_udp::EcnCodepoint::Ce,
    }
}
