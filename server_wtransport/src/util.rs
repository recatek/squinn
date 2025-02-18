use quinn_proto::EcnCodepoint as ProtoEcnCodepoint;
use quinn_udp::EcnCodepoint as UdpEcnCodepoint;

#[inline]
pub fn proto_ecn(ecn: UdpEcnCodepoint) -> ProtoEcnCodepoint {
    match ecn {
        UdpEcnCodepoint::Ect0 => ProtoEcnCodepoint::Ect0,
        UdpEcnCodepoint::Ect1 => ProtoEcnCodepoint::Ect1,
        UdpEcnCodepoint::Ce => ProtoEcnCodepoint::Ce,
    }
}

#[inline]
pub fn udp_ecn(ecn: ProtoEcnCodepoint) -> UdpEcnCodepoint {
    match ecn {
        ProtoEcnCodepoint::Ect0 => UdpEcnCodepoint::Ect0,
        ProtoEcnCodepoint::Ect1 => UdpEcnCodepoint::Ect1,
        ProtoEcnCodepoint::Ce => UdpEcnCodepoint::Ce,
    }
}
