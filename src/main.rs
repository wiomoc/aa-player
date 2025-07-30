use std::{
    time::{Duration, SystemTime},
    vec,
};

use byteorder::{BigEndian, ByteOrder};
use log::{debug, error, info, trace};
use prost::Message;
use simple_logger::SimpleLogger;
use tokio::{
    select,
    sync::mpsc::{self, Receiver},
    time::{self, Instant, Sleep},
};

use crate::{
    encryption::{EncryptedConnectionManager, EncryptionManager},
    frame::{AAPFrame, AAPFrameFragmmentation, AAPFrameType},
    protos::{SensorSourceService, input_source_service::TouchScreen},
    usb::{IncomingEvent, UsbManager},
};

mod encryption;
mod frame;
mod usb;

mod protos {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

#[tokio::main]
async fn main() -> Result<(), ()> {
    SimpleLogger::new().init().unwrap();
    info!("starting");
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
    heartbeat: Option<Heartbeater>,
}

enum HeartbeatState {
    WaitingUntilSendingNextPingRequest,
    WaitingForPingResponse,
}

struct Heartbeater {
    state: HeartbeatState,
    counter: u32,
    next_timeout: Instant,
}

impl PacketFramer {
    const MAX_FRAME_PAYLOAD_LENGTH: usize = 0x4000;
    const PING_INTERVAL_MS: u64 = 4000;
    const PING_MAX_RTT_MS: u64 = 800;

    pub async fn start_processing(
        usb_manager: UsbManager,
        mut outgoing_packet_queue: Receiver<Packet>,
    ) {
        let mut packet_framer = PacketFramer {
            encryption_manager: EncryptionManager::new(),
            usb_manager,
            state: PacketFramerState::NotConnected,
            heartbeat: None,
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
                _ = Self::wait_for_timeout(&packet_framer.heartbeat), if packet_framer.heartbeat.is_some() => packet_framer.handle_heartbeat_timer().await,
            }
        }
        packet_framer.usb_manager.terminate();
    }

    async fn wait_for_timeout(heartbeat: &Option<Heartbeater>) {
        time::sleep_until(heartbeat.as_ref().unwrap().next_timeout).await
    }


    async fn handle_heartbeat_timer(&mut self) {
        if let PacketFramerState::Connected(PacketFramerStateConnected {
            connection_state, ..
        }) = &self.state
        {
            assert!(matches!(
                connection_state,
                PacketFramerConnectionState::Established
            ));
        } else {
            panic!();
        }

        let Heartbeater {
            next_timeout,
            state,
            counter,
        } = self.heartbeat.as_mut().unwrap();
        let ping_packet = match state {
            HeartbeatState::WaitingUntilSendingNextPingRequest => {
                let ping_packet = build_ping_request_packet(counter.to_be_bytes().to_vec());
                *state = HeartbeatState::WaitingForPingResponse;
                *next_timeout = Instant::now()
                    .checked_add(Duration::from_millis(Self::PING_MAX_RTT_MS))
                    .unwrap();
                ping_packet
            }
            HeartbeatState::WaitingForPingResponse => {
                error!("Ping timeout");
                self.heartbeat = None;
                return;
            }
        };
        self.process_outgoing_packet(ping_packet).await;
    }

    fn restart_heartbeat_timer(&mut self) {
        if let Some(Heartbeater {
            next_timeout,
            state,
            ..
        }) = self.heartbeat.as_mut()
        {
            match state {
                HeartbeatState::WaitingUntilSendingNextPingRequest => {
                    *state = HeartbeatState::WaitingUntilSendingNextPingRequest;
                    *next_timeout = Self::next_ping_request_instant()
                }
                HeartbeatState::WaitingForPingResponse => {}
            };
        } else {
            self.heartbeat = Some(Heartbeater {
                state: HeartbeatState::WaitingUntilSendingNextPingRequest,
                counter: 0,
                next_timeout: Self::next_ping_request_instant(),
            })
        }
    }

    fn handle_heartbeat_ping_response(&mut self, packet: Packet) {
        let ping_response = protos::PingResponse::decode(packet.payload()).unwrap();
        info!(
            "ping response received: {:?}",
            &ping_response
        );

        let Heartbeater {
            next_timeout,
            state,
            counter,
        } = self.heartbeat.as_mut().unwrap();
        match state {
            HeartbeatState::WaitingForPingResponse => {
                let data = ping_response.data();
                assert!(data.len() == 4);
                let received_counter_value = BigEndian::read_u32(data);
                if(received_counter_value != *counter) {
                    info!("Ignoring unsolicited ping response");
                    return;
                }
                *state = HeartbeatState::WaitingForPingResponse;
                *counter += 1;
                *next_timeout = Self::next_ping_request_instant()
            }
            HeartbeatState::WaitingUntilSendingNextPingRequest => {}
        };
    }

    fn next_ping_request_instant() -> Instant {
        Instant::now()
            .checked_add(Duration::from_millis(Self::PING_INTERVAL_MS))
            .unwrap()
    }

    async fn process_outgoing_packet(&mut self, packet: Packet) {
        let connected_state = if let PacketFramerState::Connected(connected_state) = &mut self.state
        {
            connected_state
        } else {
            panic!("Not connected");
        };
        debug!("> {:?}", &packet);

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

    async fn dispatch_incoming_packet(&mut self, mut packet: Packet) {
        // todo clean up
        let connected_state = if let PacketFramerState::Connected(connected_state) = &mut self.state
        {
            connected_state
        } else {
            panic!("Not connected");
        };

        debug!("< {:?}", &packet);
        match connected_state.connection_state {
            PacketFramerConnectionState::WaitVersionResponse => {
                assert!(packet.channel_id == CHANNEL_ID_CONTROL);
                assert!(packet.message_id() == MESSAGE_ID_GET_VERSION_RESPONSE);
                connected_state.connection_state = PacketFramerConnectionState::Handshaking;
                let handshake_message = connected_state
                    .encrypted_connection_manager
                    .process_handshake_message(&mut [])
                    .unwrap();
                self.process_outgoing_packet(build_handshake_packet(&handshake_message))
                    .await;
            }
            PacketFramerConnectionState::Handshaking => {
                assert!(packet.channel_id == CHANNEL_ID_CONTROL);
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
                    info!("handshaking done");
                    self.process_outgoing_packet(build_auth_complete_packet())
                        .await;
                }
            }
            PacketFramerConnectionState::Established => {
                info!("received normal packet");

                self.restart_heartbeat_timer();
                self.decode_packet(packet).await;
            }
        }
    }

    async fn decode_packet(&mut self, packet: Packet) {
        match packet.channel_id {
            CHANNEL_ID_CONTROL => match packet.message_id() {
                MESSAGE_ID_SERVICE_DISCOVERY_REQUEST => {
                    let service_discovery_request =
                        protos::ServiceDiscoveryRequest::decode(packet.payload());
                    info!(
                        "service discovery request received: {:?}",
                        &service_discovery_request
                    );

                    let service_discovery_response = protos::ServiceDiscoveryResponse {
                        services: vec![
                            protos::Service {
                                id: 1,
                                sensor_source_service: Some(protos::SensorSourceService {
                                    sensors: vec![],
                                    location_characterization: None,
                                    supported_fuel_types: vec![protos::FuelType::Electric as i32],
                                    supported_ev_connector_types: vec![
                                        protos::EvConnectorType::Mennekes as i32,
                                    ],
                                }),
                                ..Default::default()
                            },
                            protos::Service {
                                id: 2,
                                media_sink_service: Some(protos::MediaSinkService {
                                    available_type: Some(
                                        protos::MediaCodecType::MediaCodecVideoH264Bp as i32,
                                    ),
                                    video_configs: vec![protos::VideoConfiguration {
                                        codec_resolution: Some(
                                            protos::VideoCodecResolutionType::Video1280x720 as i32,
                                        ),
                                        frame_rate: Some(
                                            protos::VideoFrameRateType::VideoFps60 as i32,
                                        ),
                                        width_margin: Some(0),
                                        height_margin: Some(0),
                                        density: Some(300),
                                        video_codec_type: Some(
                                            protos::MediaCodecType::MediaCodecVideoH264Bp as i32,
                                        ),
                                        ..Default::default()
                                    }],
                                    available_while_in_call: Some(true),
                                    display_id: Some(0),
                                    display_type: Some(protos::DisplayType::Main as i32),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                            protos::Service {
                                id: 3,
                                input_source_service: Some(protos::InputSourceService {
                                    touchscreen: vec![protos::input_source_service::TouchScreen {
                                        width: 1280,
                                        height: 720,
                                        r#type: Some(protos::TouchScreenType::Capacitive as i32),
                                        is_secondary: Some(false),
                                    }],
                                    display_id: Some(0),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                            protos::Service {
                                id: 4,
                                media_sink_service: Some(protos::MediaSinkService {
                                    available_type: Some(
                                        protos::MediaCodecType::MediaCodecAudioPcm as i32,
                                    ),
                                    audio_type: Some(
                                        protos::AudioStreamType::AudioStreamSystemAudio as i32,
                                    ),
                                    audio_configs: vec![protos::AudioConfiguration {
                                        sampling_rate: 16000,
                                        number_of_bits: 16,
                                        number_of_channels: 1,
                                    }],

                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                            /*protos::Service {
                                id: 5,
                                media_sink_service: Some(protos::MediaSinkService {
                                    available_type: Some(
                                        protos::MediaCodecType::MediaCodecAudioPcm as i32,
                                    ),
                                    audio_type: Some(
                                        protos::AudioStreamType::AudioStreamMedia as i32,
                                    ),
                                    audio_configs: vec![protos::AudioConfiguration {
                                        sampling_rate: 48000,
                                        number_of_bits: 16,
                                        number_of_channels: 2,
                                    }],

                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                            protos::Service {
                                id: 6,
                                media_sink_service: Some(protos::MediaSinkService {
                                    available_type: Some(
                                        protos::MediaCodecType::MediaCodecAudioPcm as i32,
                                    ),
                                    audio_type: Some(
                                        protos::AudioStreamType::AudioStreamTelephony as i32,
                                    ),
                                    audio_configs: vec![protos::AudioConfiguration {
                                        sampling_rate: 16000,
                                        number_of_bits: 16,
                                        number_of_channels: 1,
                                    }],

                                    ..Default::default()
                                }),
                                ..Default::default()
                            },*/
                        ],
                        driver_position: Some(protos::DriverPosition::Center as i32),
                        display_name: Some("aa_player".to_string()),
                        connection_configuration: Some(protos::ConnectionConfiguration {
                            ping_configuration: Some(protos::PingConfiguration {
                                interval_ms: Some(3000),
                                timeout_ms: Some(1000),
                                high_latency_threshold_ms: Some(100000),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        headunit_info: Some(protos::HeadUnitInfo {
                            make: Some("wiomoc".to_string()),
                            model: Some("aa_player".to_string()),
                            year: Some("2025".to_string()),
                            vehicle_id: Some("12345".to_string()),
                            head_unit_make: Some("wiomoc".to_string()),
                            head_unit_model: Some("aa_player".to_string()),
                            head_unit_software_build: Some("42".to_string()),
                            head_unit_software_version: Some("1.0".to_string()),
                        }),
                        ..Default::default()
                    };

                    self.process_outgoing_packet(build_service_discovery_response_packet(
                        service_discovery_response,
                    ))
                    .await;
                }
                MESSAGE_ID_PING_RESPONSE => {
                    self.handle_heartbeat_ping_response(packet);
                }
                message_id @ _ => info!(
                    "received packet on control channel with unknown message_id {}",
                    message_id
                ),
            },
            channel_id @ _ => info!("received packet on unknown channel {}", channel_id),
        }
    }

    async fn process_incoming_event(&mut self, incoming_event: IncomingEvent) {
        match incoming_event {
            usb::IncomingEvent::Connected => {
                self.process_connected_event().await;
            }
            usb::IncomingEvent::Closed => {
                self.state = PacketFramerState::NotConnected;
                info!("closed")
            }
            usb::IncomingEvent::Message(aap_frame) => self.process_incoming_frame(aap_frame).await,
        }
    }

    async fn process_connected_event(&mut self) {
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

    async fn process_incoming_frame(&mut self, frame: AAPFrame) {
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

const CHANNEL_ID_CONTROL: u8 = 0;
const MESSAGE_ID_GET_VERSION_REQUEST: u16 = 0x01;
const MESSAGE_ID_GET_VERSION_RESPONSE: u16 = 0x02;
const MESSAGE_ID_HANDSHAKE: u16 = 0x03;
const MESSAGE_ID_AUTH_COMPLETE: u16 = 0x04;
const MESSAGE_ID_SERVICE_DISCOVERY_REQUEST: u16 = 0x05;
const MESSAGE_ID_SERVICE_DISCOVERY_RESPONSE: u16 = 0x06;
const MESSAGE_ID_PING_REQUEST: u16 = 0x0B;
const MESSAGE_ID_PING_RESPONSE: u16 = 0x0C;

fn build_version_request_packet() -> Packet {
    Packet::new_from_parts(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_GET_VERSION_REQUEST,
        &[0, 1, 0, 1],
    )
}

fn build_handshake_packet(payload: &[u8]) -> Packet {
    Packet::new_from_parts(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_HANDSHAKE,
        payload,
    )
}

fn build_auth_complete_packet() -> Packet {
    let payload = protos::AuthResponse {
        status: protos::MessageStatus::StatusSuccess as i32,
    }
    .encode_to_vec();
    Packet::new_from_parts(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_AUTH_COMPLETE,
        &payload,
    )
}

fn build_service_discovery_response_packet(message: protos::ServiceDiscoveryResponse) -> Packet {
    let payload = message.encode_to_vec();
    Packet::new_from_parts(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        true,
        MESSAGE_ID_SERVICE_DISCOVERY_RESPONSE,
        &payload,
    )
}

fn build_ping_request_packet(data: Vec<u8>) -> Packet {
    let payload = protos::PingRequest {
        timestamp: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
        bug_report: Some(false),
        data: Some(data),
    }
    .encode_to_vec();
    Packet::new_from_parts(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_PING_REQUEST,
        &payload,
    )
}
