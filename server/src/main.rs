use std::collections::{HashMap, VecDeque};
use std::io;
use std::str;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use mio::{net::UdpSocket, Events, Interest, Poll, Token};
use quinn_proto::*;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

/// The maximum of datagrams a Server will produce via `poll_transmit`
const MAX_DATAGRAMS: usize = 10;

pub struct Server {
    endpoint: Endpoint,
    outbound: VecDeque<(Transmit, Bytes)>,
    connections: HashMap<ConnectionHandle, Connection>,
    connection_events: HashMap<ConnectionHandle, VecDeque<ConnectionEvent>>,
    endpoint_events: Vec<(ConnectionHandle, EndpointEvent)>,

    byte_buf: Vec<u8>, // Reusable byte buffer to save on allocations
}

fn main() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53423);

    let mut recv_buf = [0_u8; 8192];
    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(64);

    let mut socket = UdpSocket::bind(addr).unwrap();

    poll.registry()
        .register(&mut socket, Token(0), Interest::READABLE)
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
        byte_buf: Vec::new(),
    };

    loop {
        poll.poll(&mut events, server.compute_next_timeout()).unwrap();

        let now = Instant::now();

        while !events.is_empty() {
            match socket.recv_from(&mut recv_buf) {
                Ok((len, remote)) => {
                    println!("recv: {}B", len);
                    server.handle_recv(now, remote, BytesMut::from(&recv_buf[..len]))
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => (),
                Err(e) => panic!("recv_from err: {:?}", e),
            };
        }

        server.handle_process(now);

        // Get all the datagrams and do stuff with them
        for (connection_handle, mut datagrams) in server.incoming() {
            while let Some(bytes) = datagrams.recv() {
                println!(
                    "received datagram '{}' from {:?}",
                    str::from_utf8(&bytes).unwrap(),
                    connection_handle
                );
            }
        }

        // Send all the outgoing traffic -- TODO: Could be sendmmsg
        for (packet, buffer) in server.outgoing() {
            println!("send: {}B", buffer.len());
            if let Err(e) = socket.send_to(&buffer, packet.destination) {
                println!("send_to err: {:?}", e);
            }
        }
    }
}

impl Server {
    pub fn handle_recv(&mut self, now: Instant, remote: SocketAddr, data: BytesMut) {
        self.prepare_byte_buf();

        // REVIEW: What do I do with the local IP and ECN arguments here?
        let event = self
            .endpoint
            .handle(now, remote, None, None, data, &mut self.byte_buf);

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
                let send_buf = &self.byte_buf[..transmit.size];
                self.outbound.extend(split_transmit(transmit, send_buf));
            }
            _ => {} // There may be no event to handle
        }
    }

    pub fn handle_process(&mut self, now: Instant) {
        let max = MAX_DATAGRAMS;

        self.prepare_byte_buf();

        self.connections.retain(|connection_handle, connection| {
            connection.handle_timeout(now);

            for (_, mut events) in self.connection_events.drain() {
                for event in events.drain(..) {
                    connection.handle_event(event);
                }
            }

            while let Some(event) = connection.poll_endpoint_events() {
                self.endpoint_events.push((*connection_handle, event));
            }

            while let Some(transmit) = connection.poll_transmit(now, max, &mut self.byte_buf) {
                let send_buf = &self.byte_buf[..transmit.size];
                self.outbound.extend(split_transmit(transmit, send_buf));

                self.byte_buf.clear(); // Need to clear here to reuse the buffer
            }

            if connection.is_drained() {
                println!("connection {:?} drained", connection_handle);
            }

            // REVIEW: Is this the correct criterion to forget a connection?
            connection.is_drained() == false
        });

        for (connection_handle, event) in self.endpoint_events.drain(..) {
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
        self.byte_buf.clear();

        match self
            .endpoint
            .accept(incoming, now, &mut self.byte_buf, None)
        {
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
                        .extend(split_transmit(transmit, &self.byte_buf[..size]));
                }

                Err(error.cause)
            }
        }
    }

    fn prepare_byte_buf(&mut self) {
        self.byte_buf.clear();

        let max_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        if self.byte_buf.capacity() < max_size {
            self.byte_buf.reserve(max_size - self.byte_buf.capacity());
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

    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    transport_config.max_concurrent_uni_streams(0_u8.into());
    transport_config.max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(10_000))));

    Ok((server_config, cert_der))
}
