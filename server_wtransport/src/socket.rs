use std::io::{ErrorKind, IoSliceMut, Result as IoResult};
use std::net::SocketAddr;

use bytes::{Bytes, BytesMut};
use mio::event::Source;
use mio::net::UdpSocket;
use mio::{Interest, Registry, Token};
use quinn_proto::Transmit as ProtoTransmit;
use quinn_udp::{RecvMeta, Transmit as UdpTransmit, UdpSocketState};

use crate::util;

#[cfg(target_os = "linux")]
const BATCH_COUNT: usize = 32;
#[cfg(target_os = "windows")]
const BATCH_COUNT: usize = 1;

pub(crate) struct Socket<'a> {
    sock_mio: UdpSocket,
    sock_quic: UdpSocketState,

    iovs: [IoSliceMut<'a>; BATCH_COUNT],
    metas: [RecvMeta; BATCH_COUNT],
}

impl Socket<'_> {
    pub fn new(addr: SocketAddr, max_udp_payload_size: usize) -> Self {
        let sock_mio = UdpSocket::bind(addr).unwrap();
        let sock_quic = UdpSocketState::new((&sock_mio).into()).unwrap();

        let sock_ref = (&sock_mio).into();
        #[cfg(target_os = "windows")]
        sock_quic.set_gro(sock_ref, true).unwrap();
        #[cfg(target_os = "linux")]
        sock_quic.set_recv_timestamping(sock_ref, true).unwrap();

        let chunk_size = sock_quic.gro_segments() * max_udp_payload_size.min(u16::MAX.into());
        let total_bytes = chunk_size * BATCH_COUNT;

        let buf = Box::leak(vec![0; total_bytes].into_boxed_slice());
        let mut chunks = buf.chunks_mut(chunk_size).map(IoSliceMut::new);

        let iovs = std::array::from_fn(|_| chunks.next().unwrap());
        let metas = [RecvMeta::default(); BATCH_COUNT];

        Self {
            sock_mio,
            sock_quic,
            iovs,
            metas,
        }
    }

    pub fn recv_all(&mut self, mut f: impl FnMut(BytesMut, &RecvMeta)) -> IoResult<()> {
        loop {
            let recv = || {
                self.sock_quic.recv(
                    (&self.sock_mio).into(),
                    &mut self.iovs, //.
                    &mut self.metas,
                )
            };

            match self.sock_mio.try_io(recv) {
                Ok(count) => {
                    for (meta, buf) in self.metas.iter_mut().zip(self.iovs.iter()).take(count) {
                        // Windows doesn't have packet timestamping, so fake it
                        #[cfg(target_os = "windows")]
                        time::stamp_meta(meta);

                        debug_assert!(meta.timestamp.is_some());

                        // Split up any datagrams merged by the GRO process
                        let mut data: BytesMut = buf[0..meta.len].into();
                        while data.is_empty() == false {
                            f(data.split_to(meta.stride.min(data.len())), meta)
                        }
                    }
                }

                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    pub fn try_send(&mut self, transmit: ProtoTransmit, buffer: Bytes) -> IoResult<()> {
        self.sock_mio.try_io(|| {
            self.sock_quic.try_send(
                (&self.sock_mio).into(),
                &UdpTransmit {
                    destination: transmit.destination,
                    ecn: transmit.ecn.map(util::udp_ecn),
                    contents: &buffer,
                    segment_size: transmit.segment_size,
                    src_ip: transmit.src_ip,
                },
            )
        })
    }
}

impl Source for Socket<'_> {
    fn register(
        &mut self, //.
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> IoResult<()> {
        self.sock_mio.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> IoResult<()> {
        self.sock_mio.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> IoResult<()> {
        self.sock_mio.deregister(registry)
    }
}

#[cfg(target_os = "windows")]
mod time {
    use std::sync::LazyLock;
    use std::time::Instant;

    use super::*;

    static START: LazyLock<Instant> = LazyLock::new(|| Instant::now());

    pub(super) fn stamp_meta(meta: &mut RecvMeta) {
        if meta.timestamp.is_none() {
            meta.timestamp = Some(Instant::now().saturating_duration_since(*START));
        }
    }
}
