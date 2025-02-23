mod server;
mod util;
mod webtransport;

use std::fs::File;
use std::io::{BufReader, ErrorKind};
use std::net::SocketAddr;
use std::str;
use std::time::Instant;

use bytes::BytesMut;
use mio::{net::UdpSocket, Events, Interest, Poll, Token};
use quinn_udp::{Transmit, UdpSocketState};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::server::Server;

fn main() {
    //simple_logger::init().unwrap();

    let addr: SocketAddr = "[::]:4443".parse().unwrap();

    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(64);

    let mut sock_mio = UdpSocket::bind(addr).unwrap();
    let sock_quic = UdpSocketState::new((&sock_mio).into()).unwrap();

    let sock_ref = (&sock_mio).into();
    #[cfg(target_os = "windows")]
    sock_quic.set_gro(sock_ref, true).unwrap();
    #[cfg(target_os = "linux")]
    sock_quic.set_recv_timestamping(sock_ref, true).unwrap();

    poll.registry()
        .register(&mut sock_mio, Token(0), Interest::READABLE)
        .unwrap();

    let (certs, key) = read_certs();
    let mut server = Server::new(certs, key).expect("failed to create server");
    let (mut iovs, mut metas) = server.create_buffers(sock_quic.gro_segments());

    println!("listening on {}...", addr);

    loop {
        let now = Instant::now();
        let next_timeout = server
            .compute_next_timeout()
            .map(|t| t.saturating_duration_since(now));
        poll.poll(&mut events, next_timeout).unwrap();

        let now = Instant::now(); // Need to update this after polling
        while events.is_empty() == false {
            match sock_mio.try_io(|| sock_quic.recv((&sock_mio).into(), &mut iovs, &mut metas)) {
                Ok(count) => {
                    for (meta, buf) in metas.iter().zip(iovs.iter()).take(count) {
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
        for (connection_handle, connection) in server.connections_mut() {
            // TODO: Need to get/write the session ID for webtransport!

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
            let transmit = Transmit {
                destination: transmit.destination,
                ecn: transmit.ecn.map(util::udp_ecn),
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

fn read_certs() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let certs = File::open("cert/localhost.crt").expect("failed to open cert file");
    let key = File::open("cert/localhost.key").expect("failed to open key file");

    let certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut BufReader::new(certs))
        .collect::<Result<_, _>>()
        .expect("failed to load certs");
    let key = rustls_pemfile::private_key(&mut BufReader::new(key))
        .expect("failed to load private key")
        .expect("missing private key");

    assert!(!certs.is_empty(), "could not find certificate");
    (certs, key)
}
