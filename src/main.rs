use std::vec;

use byteorder::{BigEndian, ByteOrder};
use futures::channel;
use tokio::{select, sync::mpsc, sync::mpsc::Receiver};

use crate::{
    encryption::{EncryptedConnectionManager, EncryptionManager},
    frame::{AAPFrame, AAPFrameFragmmentation, AAPFrameType},
    usb::{IncomingEvent, UsbManager},
};

mod encryption;
mod frame;
mod usb;

#[tokio::main]
async fn main() -> Result<(), ()> {
    println!("Hello, world!");
    let usb_manager = UsbManager::start();

    let (outgoing_packet_queue_sender, outgoing_packet_queue_receiver) = mpsc::channel(2);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        drop(outgoing_packet_queue_sender);
    });
    PacketFramer::start_processing(usb_manager, outgoing_packet_queue_receiver).await;
    Ok(())
}

#[derive(Debug)]
pub(crate) struct Packet {
    pub channel_id: u8,
    pub r#type: AAPFrameType,
    pub encrypted: bool,
    pub message_id_and_payload: Vec<u8>,
}

impl Packet {
    fn new_from_parts(
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

    fn message_id(&self) -> u16 {
        BigEndian::read_u16(&self.message_id_and_payload[..2])
    }

    fn payload(&self) -> &[u8] {
        &self.message_id_and_payload[2..]
    }

    fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.message_id_and_payload[2..]
    }
}

#[derive(Debug, Clone, Copy)]
enum PacketFramerConnectionState {
    WaitVersionResponse,
    Handshaking,
    Established,
}

struct PacketFramerStateConnected {
    encrypted_connection_manager: EncryptedConnectionManager,
    pending_frame: Option<AAPFrame>,
    connection_state: PacketFramerConnectionState,
}

enum PacketFramerState {
    NotConnected,
    Connected(PacketFramerStateConnected),
}

struct PacketFramer {
    usb_manager: UsbManager,
    encryption_manager: EncryptionManager,
    state: PacketFramerState,
}

impl PacketFramer {
    const MAX_FRAME_PAYLOAD_LENGTH: usize = 0x4000;

    pub async fn start_processing(
        usb_manager: UsbManager,
        mut outgoing_packet_queue: Receiver<Packet>,
    ) {
        let mut packet_framer = PacketFramer {
            encryption_manager: EncryptionManager::new(),
            usb_manager,
            state: PacketFramerState::NotConnected,
        };

        loop {
            select! {
                outgoing_packet = outgoing_packet_queue.recv() => {
                    if let Some(outgoing_packet) = outgoing_packet {
                        packet_framer.process_outgoing_packet(outgoing_packet).await
                    } else {
                        break;
                    }
                },
                incoming_event = packet_framer.usb_manager.recv_frame() => packet_framer.process_incoming_event(incoming_event).await,
            }
        }
        packet_framer.usb_manager.terminate();
    }

    pub async fn process_outgoing_packet(&mut self, packet: Packet) {
        let connected_state = if let PacketFramerState::Connected(connected_state) = &mut self.state
        {
            connected_state
        } else {
            panic!("Not connected");
        };
        if packet.message_id_and_payload.len() <= Self::MAX_FRAME_PAYLOAD_LENGTH {
            let may_be_encrypted_payload = if packet.encrypted {
                assert!(matches!(
                    connected_state.connection_state,
                    PacketFramerConnectionState::Established
                ));
                connected_state
                    .encrypted_connection_manager
                    .encrypt_message(&packet.message_id_and_payload)
            } else {
                packet.message_id_and_payload
            };

            self.usb_manager
                .send_frame(AAPFrame {
                    channel_id: packet.channel_id,
                    encrypted: packet.encrypted,
                    frag_info: frame::AAPFrameFragmmentation::Unfragmented,
                    r#type: packet.r#type,
                    total_payload_length: may_be_encrypted_payload.len(),
                    payload: may_be_encrypted_payload,
                })
                .await
                .unwrap();
        } else {
            let total_payload_length = packet.message_id_and_payload.len();
            let payload_fragments_count = (total_payload_length + Self::MAX_FRAME_PAYLOAD_LENGTH
                - 1)
                / Self::MAX_FRAME_PAYLOAD_LENGTH;
            for (index, fragment) in packet
                .message_id_and_payload
                .chunks(Self::MAX_FRAME_PAYLOAD_LENGTH)
                .enumerate()
            {
                let may_be_encrypted_fragment_payload = if packet.encrypted {
                    assert!(matches!(
                        connected_state.connection_state,
                        PacketFramerConnectionState::Established
                    ));
                    connected_state
                        .encrypted_connection_manager
                        .encrypt_message(fragment)
                } else {
                    fragment.to_vec()
                };

                let frag_info = match index {
                    0 => frame::AAPFrameFragmmentation::First,
                    _ if index == payload_fragments_count - 1 => {
                        frame::AAPFrameFragmmentation::Last
                    }
                    _ => frame::AAPFrameFragmmentation::Continuation,
                };
                self.usb_manager
                    .send_frame(AAPFrame {
                        channel_id: packet.channel_id,
                        encrypted: packet.encrypted,
                        frag_info,
                        r#type: packet.r#type,
                        total_payload_length,
                        // XXX could be incorrect if payload expands during encryption
                        payload: may_be_encrypted_fragment_payload,
                    })
                    .await
                    .unwrap();
            }
        }
    }

    pub async fn dispatch_incoming_packet(&mut self, mut packet: Packet) {
        // todo clean up
        let connected_state = if let PacketFramerState::Connected(connected_state) = &mut self.state
        {
            connected_state
        } else {
            panic!("Not connected");
        };

        println!("received {:?}", &packet);
        match connected_state.connection_state {
            PacketFramerConnectionState::WaitVersionResponse => {
                assert!(packet.message_id() == MESSAGE_ID_GET_VERSION_RESPONSE);
                print!("Handshake response {:?}", packet.payload());
                connected_state.connection_state = PacketFramerConnectionState::Handshaking;
                let handshake_message = connected_state
                    .encrypted_connection_manager
                    .process_handshake_message(&mut [])
                    .unwrap();
                self.process_outgoing_packet(build_handshake_packet(&handshake_message))
                    .await;
            }
            PacketFramerConnectionState::Handshaking => {
                assert!(packet.message_id() == MESSAGE_ID_HANDSHAKE);
                if let Some(handshake_message) = connected_state
                    .encrypted_connection_manager
                    .process_handshake_message(packet.payload_mut())
                {
                    self.process_outgoing_packet(build_handshake_packet(&handshake_message))
                        .await;
                } else if !connected_state
                    .encrypted_connection_manager
                    .is_handshaking()
                {
                    connected_state.connection_state = PacketFramerConnectionState::Established;
                    println!("handshaking done");
                }
            }
            PacketFramerConnectionState::Established => {
                println!("received normal packet {:?}", packet);
            }
        }
    }

    pub async fn process_incoming_event(&mut self, incoming_event: IncomingEvent) {
        match incoming_event {
            usb::IncomingEvent::Connected => {
                self.process_connected_event().await;
            }
            usb::IncomingEvent::Closed => {
                self.state = PacketFramerState::NotConnected;
                println!("closed")
            }
            usb::IncomingEvent::Message(aap_frame) => self.process_incoming_frame(aap_frame).await,
        }
    }

    pub async fn process_connected_event(&mut self) {
        assert!(matches!(self.state, PacketFramerState::NotConnected));
        let encrypted_connection_manager = self.encryption_manager.new_connection();
        let connected_state = PacketFramerStateConnected {
            encrypted_connection_manager,
            pending_frame: None,
            connection_state: PacketFramerConnectionState::WaitVersionResponse,
        };

        self.state = PacketFramerState::Connected(connected_state);
        self.process_outgoing_packet(build_version_request_packet())
            .await;
    }

    pub async fn process_incoming_frame(&mut self, frame: AAPFrame) {
        let connected_state = if let PacketFramerState::Connected(connected_state) = &mut self.state
        {
            connected_state
        } else {
            panic!("Not connected");
        };
        let frame_complete = matches!(
            frame.frag_info,
            AAPFrameFragmmentation::Last | AAPFrameFragmmentation::Unfragmented
        );

        let mut defragmented_frame =
            if let Some(mut pending_frame) = connected_state.pending_frame.take() {
                assert!(pending_frame.channel_id == frame.channel_id);
                assert!(pending_frame.encrypted == frame.encrypted);
                assert!(pending_frame.r#type == frame.r#type);
                assert!(matches!(
                    frame.frag_info,
                    AAPFrameFragmmentation::Continuation | AAPFrameFragmmentation::Last
                ));
                pending_frame.payload.extend_from_slice(&frame.payload);
                pending_frame
            } else {
                assert!(!matches!(
                    frame.frag_info,
                    AAPFrameFragmmentation::Continuation | AAPFrameFragmmentation::Last
                ));
                frame
            };

        if frame_complete {
            let payload = if defragmented_frame.encrypted {
                connected_state
                    .encrypted_connection_manager
                    .decrypt_message(&mut defragmented_frame.payload)
            } else {
                defragmented_frame.payload
            };

            self.dispatch_incoming_packet(Packet {
                channel_id: defragmented_frame.channel_id,
                r#type: defragmented_frame.r#type,
                encrypted: defragmented_frame.encrypted,
                message_id_and_payload: payload,
            })
            .await;
        } else {
            connected_state.pending_frame = Some(defragmented_frame);
        }
    }
}

const MESSAGE_ID_GET_VERSION_REQUEST: u16 = 0x01;
const MESSAGE_ID_GET_VERSION_RESPONSE: u16 = 0x02;
const MESSAGE_ID_HANDSHAKE: u16 = 0x03;

fn build_version_request_packet() -> Packet {
    Packet::new_from_parts(
        0,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_GET_VERSION_REQUEST,
        &[0, 1, 0, 1],
    )
}

fn build_handshake_packet(payload: &[u8]) -> Packet {
    Packet::new_from_parts(
        0,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_HANDSHAKE,
        payload,
    )
}
