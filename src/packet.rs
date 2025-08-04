use byteorder::{BigEndian, ByteOrder};
use log::{debug, trace};
use prost::Message;

use crate::{
    encryption::EncryptedConnectionManager,
    frame::{AAPFrame, AAPFrameFragmmentation, AAPFrameType},
};

#[derive(Debug)]
pub(crate) struct Packet {
    pub channel_id: u8,
    pub r#type: AAPFrameType,
    pub encrypted: bool,
    pub message_id_and_payload: Vec<u8>,
}

impl Packet {
    pub(crate) fn new_from_parts(
        channel_id: u8,
        r#type: AAPFrameType,
        encrypted: bool,
        message_id: u16,
        payload: &[u8],
    ) -> Self {
        let mut message_id_and_payload = Vec::with_capacity(2 + payload.len());
        message_id_and_payload.extend_from_slice(&message_id.to_be_bytes());
        message_id_and_payload.extend_from_slice(payload);

        Self {
            channel_id,
            r#type,
            encrypted,
            message_id_and_payload,
        }
    }

    pub(crate) fn new_from_proto_message(
        channel_id: u8,
        r#type: AAPFrameType,
        encrypted: bool,
        message_id: u16,
        payload: &impl Message,
    ) -> Self {
        let mut message_id_and_payload = Vec::with_capacity(2 + payload.encoded_len());
        message_id_and_payload.extend_from_slice(&message_id.to_be_bytes());
        payload.encode_raw(&mut message_id_and_payload);

        Self {
            channel_id,
            r#type,
            encrypted,
            message_id_and_payload,
        }
    }

    pub(crate) fn message_id(&self) -> u16 {
        BigEndian::read_u16(&self.message_id_and_payload[..2])
    }

    pub(crate) fn payload(&self) -> &[u8] {
        &self.message_id_and_payload[2..]
    }

    pub(crate) fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.message_id_and_payload[2..]
    }
}

pub(crate) struct PacketFramer {
    pending_frame: Option<AAPFrame>,
    encrypted_connection_manager: EncryptedConnectionManager,
}

impl PacketFramer {
    const MAX_FRAME_PAYLOAD_LENGTH: usize = 0x4000;

    pub(crate) fn new(encrypted_connection_manager: EncryptedConnectionManager) -> Self {
        Self {
            pending_frame: None,
            encrypted_connection_manager,
        }
    }

    pub(crate) fn process_handshake_message(&mut self, payload: &mut [u8]) -> Option<Vec<u8>> {
        self.encrypted_connection_manager
            .process_handshake_message(payload)
    }

    pub(crate) fn is_handshaking(&self) -> bool {
        self.encrypted_connection_manager.is_handshaking()
    }

    pub(crate) async fn process_outgoing_packet(
        &mut self,
        packet: Packet,
        frame_sender: impl AsyncFn(AAPFrame),
    ) {
        trace!("> {:?}", &packet);

        if packet.message_id_and_payload.len() <= Self::MAX_FRAME_PAYLOAD_LENGTH {
            let may_be_encrypted_payload = if packet.encrypted {
                assert!(!self.encrypted_connection_manager.is_handshaking());
                self.encrypted_connection_manager
                    .encrypt_message(&packet.message_id_and_payload)
            } else {
                packet.message_id_and_payload
            };

            frame_sender(AAPFrame {
                channel_id: packet.channel_id,
                encrypted: packet.encrypted,
                frag_info: AAPFrameFragmmentation::Unfragmented,
                r#type: packet.r#type,
                total_payload_length: may_be_encrypted_payload.len(),
                payload: may_be_encrypted_payload,
            })
            .await
        } else {
            let total_payload_length = packet.message_id_and_payload.len();
            let payload_fragments_count =
                total_payload_length.div_ceil(Self::MAX_FRAME_PAYLOAD_LENGTH);
            for (index, fragment) in packet
                .message_id_and_payload
                .chunks(Self::MAX_FRAME_PAYLOAD_LENGTH)
                .enumerate()
            {
                let may_be_encrypted_fragment_payload = if packet.encrypted {
                    assert!(!self.encrypted_connection_manager.is_handshaking());

                    self.encrypted_connection_manager.encrypt_message(fragment)
                } else {
                    fragment.to_vec()
                };

                let frag_info = match index {
                    0 => AAPFrameFragmmentation::First,
                    _ if index == payload_fragments_count - 1 => AAPFrameFragmmentation::Last,
                    _ => AAPFrameFragmmentation::Continuation,
                };

                frame_sender(AAPFrame {
                    channel_id: packet.channel_id,
                    encrypted: packet.encrypted,
                    frag_info,
                    r#type: packet.r#type,
                    total_payload_length,
                    // XXX could be incorrect if payload expands during encryption
                    payload: may_be_encrypted_fragment_payload,
                })
                .await;
            }
        }
    }

    pub(crate) fn process_incoming_frame(&mut self, mut frame: AAPFrame) -> Option<Packet> {
        if frame.encrypted {
            let decrypted_payload = self
                .encrypted_connection_manager
                .decrypt_message(&mut frame.payload);
            frame.payload = decrypted_payload;
        }

        let defragmented_frame = match frame.frag_info {
            AAPFrameFragmmentation::First => {
                assert!(self.pending_frame.is_none());
                self.pending_frame = Some(frame);
                return None;
            }
            AAPFrameFragmmentation::Continuation | AAPFrameFragmmentation::Last => {
                let mut pending_frame = self.pending_frame.take().expect("pending frame missing");
                assert!(pending_frame.channel_id == frame.channel_id);
                assert!(pending_frame.encrypted == frame.encrypted);
                assert!(pending_frame.r#type == frame.r#type);

                pending_frame.payload.extend_from_slice(&frame.payload);
                if matches!(frame.frag_info, AAPFrameFragmmentation::Continuation) {
                    self.pending_frame = Some(pending_frame);
                    return None;
                }
                pending_frame
            }
            AAPFrameFragmmentation::Unfragmented => frame,
        };

        Some(Packet {
            channel_id: defragmented_frame.channel_id,
            r#type: defragmented_frame.r#type,
            encrypted: defragmented_frame.encrypted,
            message_id_and_payload: defragmented_frame.payload,
        })
    }
}
