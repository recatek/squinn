use bytes::Bytes;
use quinn_proto::Transmit;

pub(crate) struct Outbound(pub(crate) Vec<(Transmit, Bytes)>);

impl Outbound {
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    /// Pushes a `Transmit` to the outbound buffer, splitting it if necessary.
    pub(crate) fn push(&mut self, transmit: Transmit, buf: &mut Vec<u8>) {
        let mut buffer = Bytes::copy_from_slice(&buf[..transmit.size]);

        match transmit.segment_size {
            // No separate segments -- just push the whole thing
            None => self.0.push((transmit, buffer)),

            // Multiple segments, so split and push them as sub-transmits
            Some(segment_size) => {
                while buffer.is_empty() == false {
                    let end = segment_size.min(buffer.len());
                    let contents = buffer.split_to(end);

                    self.0.push((
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
            }
        };

        buf.clear();
    }
}

impl Default for Outbound {
    fn default() -> Self {
        Self::new()
    }
}
