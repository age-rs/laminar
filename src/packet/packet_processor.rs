use std::io::{Cursor, Read, Write};
use std::net::SocketAddr;

use net::{SocketState, NetworkConfig};
use packet::{header, Packet};
use sequence_buffer::{SequenceBuffer, ReassemblyData};
use self::header::{FragmentHeader, PacketHeader, HeaderReader};

use error::{NetworkError, Result};

/// A wrapper for processing data.
pub struct PacketProcessor {
    /// buffer for temporarily fragment storage
    reassembly_buffer: SequenceBuffer<ReassemblyData>,
    config: NetworkConfig,
}

impl PacketProcessor {
    pub fn new(config: &NetworkConfig) -> Self
    {
        PacketProcessor { reassembly_buffer: SequenceBuffer::with_capacity(config.fragment_reassembly_buffer_size), config: config.clone() }
    }

    /// Process data and return the resulting packet
    pub fn process_data(
        &mut self,
        packet: Vec<u8>,
        addr: SocketAddr,
        socket_state: &mut SocketState,
    ) -> Result<Option<Packet>> {
        let prefix_byte = packet[0];
        let mut cursor = Cursor::new(packet);

        let received_bytes = if prefix_byte & 1 == 0 {
            // a normal packet starts by a header whose first bit is always 0.
            self.handle_normal_packet(&mut cursor, &addr, socket_state)
        } else {
            self.handle_fragment(&mut cursor)
        };

        return match received_bytes {
            Ok(Some(payload)) => Ok(Some(Packet::sequenced_unordered(addr, payload, ))),
            Ok(None) => Ok(None),
            Err(_) => Err(NetworkError::ReceiveFailed.into()),
        };
    }

    /// Extract fragments from data.
    fn handle_fragment(&mut self, cursor: &mut Cursor<Vec<u8>>) -> Result<Option<Vec<u8>>> {
        // read fragment packet
        let fragment_header = FragmentHeader::read(cursor)?;

        self.create_fragment_if_not_exists(&fragment_header);

        let num_fragments_received;
        let num_fragments_total;
        let sequence;
        let total_buffer;

        {
            // get entry of previous received fragments
            let reassembly_data = match self.reassembly_buffer.get_mut(fragment_header.sequence()) {
                Some(val) => val,
                None => return Err(NetworkError::InvalidFragmentHeader.into()),
            };

            // Got the data
            if reassembly_data.num_fragments_total != fragment_header.fragment_count() {
                return Err(NetworkError::InvalidFragmentHeader.into());
            }

            if reassembly_data.fragments_received[usize::from(fragment_header.id())] {
                return Err(NetworkError::InvalidFragmentHeader.into());
            }

            // increase number of received fragments and set the specific fragment to received.
            reassembly_data.num_fragments_received += 1;
            reassembly_data.fragments_received[usize::from(fragment_header.id())] = true;

            // read payload after fragment header
            let mut payload = Vec::new();
            cursor.read_to_end(&mut payload)?;

            // add the payload from the fragment to the buffer whe have in cache
            reassembly_data.buffer.write(payload.as_slice());

            num_fragments_received = reassembly_data.num_fragments_received;
            num_fragments_total = reassembly_data.num_fragments_total;
            sequence = reassembly_data.sequence as u16;
            total_buffer = reassembly_data.buffer.clone();
        }

        // if whe received all fragments then remove entry and return the total received bytes.
        if num_fragments_received == num_fragments_total {
            let sequence = sequence as u16;
            self.reassembly_buffer.remove(sequence);

            return Ok(Some(total_buffer));
        }

        return Ok(None);
    }

    /// Extract header and data from normal packet.
    fn handle_normal_packet(
        &mut self,
        cursor: &mut Cursor<Vec<u8>>,
        addr: &SocketAddr,
        socket_state: &mut SocketState,
    ) -> Result<Option<Vec<u8>>> {
        let packet_header = PacketHeader::read(cursor);

        match packet_header {
            Ok(header) => {
                socket_state.process_received(*addr, &header)?;

                let mut payload = Vec::new();
                cursor.read_to_end(&mut payload)?;

                Ok(Some(payload))
            }
            Err(_) => Err(NetworkError::HeaderParsingFailed.into()),
        }
    }

    /// if fragment does not exist we need to insert a new entry
    fn create_fragment_if_not_exists(&mut self, fragment_header: &FragmentHeader) -> Result<()> {
        if !self.reassembly_buffer.exists(fragment_header.sequence()) {
            if fragment_header.id() == 0 {
                match fragment_header.packet_header() {
                    Some(header) => {
                        let reassembly_data = ReassemblyData::new(
                            fragment_header.sequence(),
                            header.ack_seq(),
                            header.ack_field(),
                            fragment_header.fragment_count(),
                            fragment_header.size() as usize,
                            (9 + self.config.fragment_size) as usize,
                        );

                        self.reassembly_buffer
                            .insert(reassembly_data.clone(), fragment_header.sequence());
                    }
                    None => return Err(NetworkError::InvalidFragmentHeader.into()),
                }
            } else {
                return Err(NetworkError::InvalidFragmentHeader.into());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::PacketProcessor;
    use net::{NetworkConfig, SocketState};
    use packet::Packet;
    use total_fragments_needed;

    /// Tests if a packet will be processed right.
    ///
    /// 1. first create test Packet
    /// 2. process it with `pre_process_packet` so we have valid raw data
    /// 3. then assert that the Packet we've gotten contains the right data.
    #[test]
    fn process_normal_packet_test() {
        let config = NetworkConfig::default();
        let mut packet_processor = PacketProcessor::new(&config);

        let test_data: Vec<u8> = vec![1, 2, 3, 4, 5];

        // first setup packet data
        let packet = Packet::sequenced_unordered("127.0.0.1:12345".parse().unwrap(), test_data.clone());

        let mut socket_sate = SocketState::new(&config).unwrap();
        let mut result = socket_sate.pre_process_packet(packet, &config).unwrap();

        let packet_data = result.1.parts();

        assert_eq!(packet_data.len(), 1);

        for part in packet_data {
            let packet: Packet = packet_processor
                .process_data(part, result.0, &mut socket_sate)
                .unwrap()
                .unwrap(); /* unwrap should not fail and if it would test failed :) */
            assert_eq!(packet.payload(), test_data.as_slice());
            assert_eq!(packet.addr(), "127.0.0.1:12345".parse().unwrap());
        }
    }

    /// Tests if an fragmented packet will be reassembled and processed right.
    /// 1. first create an test Packet
    /// 2. process it with `pre_process_packet` so we have valid raw data
    /// 3. then assert that the Packet we've gotten from contains the right data.
    #[test]
    fn process_fragment_packet_test() {
        let config = NetworkConfig::default();
        let mut packet_processor = PacketProcessor::new(&config);

        let test_data: Vec<u8> = vec![1; 4000];

        // first setup packet data
        let packet = Packet::sequenced_unordered("127.0.0.1:12345".parse().unwrap(), test_data.clone());

        let mut socket_sate = SocketState::new(&config).unwrap();
        let mut result = socket_sate.pre_process_packet(packet, &config).unwrap();

        let packet_data = result.1.parts();

        let expected_fragments =
            total_fragments_needed(test_data.len() as u16, config.fragment_size) as usize;
        assert_eq!(packet_data.len(), expected_fragments);

        let mut is_packet_reassembled = false;

        for part in packet_data {
            let result: Option<Packet> = packet_processor
                .process_data(part, result.0, &mut socket_sate)
                .unwrap(); /* unwrap should not fail and if it would test failed :) */

            if let Some(packet) = result {
                assert_eq!(packet.payload(), test_data.as_slice());
                assert_eq!(packet.addr(), "127.0.0.1:12345".parse().unwrap());
                is_packet_reassembled = true;
            }
        }

        assert!(is_packet_reassembled);
    }
}
