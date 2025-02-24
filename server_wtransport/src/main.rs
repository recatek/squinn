mod outbound;
mod server;
mod session;
mod socket;
mod util;

mod webtransport;

use std::fs::File;
use std::io::{BufReader, ErrorKind};
use std::net::SocketAddr;
use std::str;
use std::time::Instant;

use mio::{Events, Interest, Poll, Token};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::server::Server;
use crate::socket::Socket;

const TOKEN_RECV: Token = Token(0);

fn main() {
    //simple_logger::init().unwrap();

    let (certs, key) = read_certs();
    let mut server = Server::new(certs, key).expect("failed to create server");

    let addr: SocketAddr = "127.0.0.1:4443".parse().unwrap();
    let mut socket = Socket::new(addr, server.get_max_udp_payload_size() as usize);

    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(64);

    poll.registry()
        .register(&mut socket, TOKEN_RECV, Interest::READABLE)
        .unwrap();

    println!("listening on {}...", addr);

    loop {
        let now = Instant::now();
        let next_timeout = server
            .compute_next_timeout()
            .map(|t| t.saturating_duration_since(now));

        if let Err(err) = poll.poll(&mut events, next_timeout) {
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            panic!("poll error: {}", err);
        }

        let now = Instant::now();
        for event in events.iter() {
            match event.token() {
                TOKEN_RECV => {
                    match socket.recv_all(|bytes, meta| {
                        println!("recv: {}B", bytes.len());
                        server.handle_recv(now, bytes, meta)
                    }) {
                        Ok(()) => {}
                        Err(e) => println!("recv error: {:?}", e),
                    }
                }
                _ => unreachable!(),
            }
        }

        server.handle_process(now);

        // Get all the datagrams and do stuff with them
        for (connection_handle, session) in server.sessions_mut() {
            loop {
                let bytes = match session.recv_datagram() {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => break,

                    Err(e) => {
                        println!("failed to recv datagram: {:?}", e);
                        continue;
                    }
                };

                println!(
                    "echoing datagram '{}' from {:?}",
                    str::from_utf8(&bytes).unwrap(),
                    connection_handle,
                );

                let send = session.send_datagram(|buf| {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    bytes.len()
                });

                if let Err(e) = send {
                    println!("failed to send datagram: {:?}", e);
                    continue;
                }
            }
        }

        // Send all the outgoing traffic
        for (transmit, buffer) in server.outgoing() {
            println!("send: {}B", buffer.len());
            match socket.try_send(transmit, buffer) {
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
