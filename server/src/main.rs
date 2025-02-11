use std::{
    cmp,
    collections::{HashMap, HashSet, VecDeque},
    env,
    io::{self, Write},
    mem,
    net::{Ipv6Addr, SocketAddr},
    ops::RangeFrom,
    str,
    sync::{Arc, Mutex},
};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mio::{Poll, Events, Token, Interest};
use mio::net::UdpSocket;

use bytes::{Bytes, BytesMut};
use quinn_proto::{
    Connection, ConnectionError, ConnectionEvent, ConnectionHandle, EcnCodepoint, Endpoint,
    Incoming, Transmit,
};

#[derive(Debug, Copy, Clone)]
pub enum IncomingConnectionBehavior {
    Accept,
    Reject,
    Retry,
    Wait,
}

pub struct Server {
    endpoint: Endpoint,
    addr: SocketAddr,
    socket: Option<UdpSocket>,
    timeout: Option<Instant>,
    outbound: VecDeque<(Transmit, Bytes)>,
    delayed: VecDeque<(Transmit, Bytes)>,
    inbound: VecDeque<(Instant, Option<EcnCodepoint>, BytesMut)>,
    accepted: Option<Result<ConnectionHandle, ConnectionError>>,
    connections: HashMap<ConnectionHandle, Connection>,
    conn_events: HashMap<ConnectionHandle, VecDeque<ConnectionEvent>>,
    waiting_incoming: Vec<Incoming>,
}

fn main() {
    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(64);

    let mut socket = UdpSocket::bind("127.0.0.1:54323".parse().unwrap()).unwrap();
    poll.registry().register(&mut socket, Token(0), Interest::READABLE).unwrap();

    loop {
        poll.poll(&mut events, None).unwrap();
    }
}
