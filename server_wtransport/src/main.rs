mod server;
mod util;

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str;
use std::time::{Instant, SystemTime};

use bytes::BytesMut;
use mio::{net::UdpSocket, Events, Interest, Poll, Token};
use quinn_udp::{Transmit, UdpSocketState};

#[cfg(target_os = "linux")]
use quinn_udp::RecvTime;

use crate::server::Server;

fn main() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 53423);

    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(64);

    let mut sock_mio = UdpSocket::bind(addr).unwrap();
    let sock_quic = UdpSocketState::new((&sock_mio).into()).unwrap();

    let sock_ref = (&sock_mio).into();
    #[cfg(target_os = "windows")]
    sock_quic.set_gro(sock_ref, true).unwrap();
    #[cfg(target_os = "linux")]
    sock_quic.set_rx_timestamps(sock_ref, true).unwrap();

    poll.registry()
        .register(&mut sock_mio, Token(0), Interest::READABLE)
        .unwrap();

    let (mut server, _cert) = Server::new().expect("failed to create server");
    let (mut iovs, mut metas) = server.create_buffers(sock_quic.gro_segments());

    loop {
        let next_timeout = server.compute_next_timeout();
        poll.poll(&mut events, next_timeout).unwrap();

        let now = Instant::now();
        let sys_now = SystemTime::now();

        while events.is_empty() == false {
            match sock_mio.try_io(|| sock_quic.recv((&sock_mio).into(), &mut iovs, &mut metas)) {
                Ok(count) => {
                    for (meta, buf) in metas.iter().zip(iovs.iter()).take(count) {
                        // Offset the receipt time by the packet timestamp
                        let now = match meta.timestamp {
                            #[cfg(target_os = "linux")]
                            Some(RecvTime::Realtime(rx)) => match sys_now.duration_since(rx) {
                                Ok(duration) => now - duration,
                                Err(_) => now,
                            },
                            _ => now,
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
        for (connection_handle, connection) in server.connections_mut() {
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
