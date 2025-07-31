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
    PacketRouter::start_processing(usb_manager, outgoing_packet_queue_receiver).await;
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

enum PacketFramerConnectionState {
    WaitVersionResponse,
    Handshaking,
    Established { heartbeat: Heartbeater },
}

struct PacketRouterStateConnected {
    framer: PacketFramer,
    connection_state: PacketFramerConnectionState,
}

enum PacketRouterState {
    NotConnected,
    Connected(PacketRouterStateConnected),
}

struct PacketRouter {
    usb_manager: UsbManager,
    state: PacketRouterState,
    encryption_manager: EncryptionManager,
}

struct PacketFramer {
    pending_frame: Option<AAPFrame>,
    encrypted_connection_manager: EncryptedConnectionManager,
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

impl PacketRouter {
    const PING_INTERVAL_MS: u64 = 4000;
    const PING_MAX_RTT_MS: u64 = 800;

    pub async fn start_processing(
        usb_manager: UsbManager,
        mut outgoing_packet_queue: Receiver<Packet>,
    ) {
        let mut packet_framer = PacketRouter {
            encryption_manager: EncryptionManager::new(),
            usb_manager,
            state: PacketRouterState::NotConnected,
        };

        loop {
            let incoming_event = packet_framer.usb_manager.recv_frame().await;
            packet_framer.process_incoming_event(incoming_event).await;
            if matches!(incoming_event, usb::IncomingEvent::Connected) {
                packet_framer.process_connected_event().await;
            }
        }
        packet_framer.usb_manager.terminate();
    }

    async fn process_incoming_event(&mut self, incoming_event: IncomingEvent) {
        match incoming_event {
            usb::IncomingEvent::Connected => {}
            usb::IncomingEvent::Closed => {
                self.state = PacketRouterState::NotConnected;
                info!("closed")
            }
            usb::IncomingEvent::Frame(aap_frame) => self.process_incoming_frame(aap_frame).await,
        }
    }

    async fn process_connected_event(&mut self) {
        assert!(matches!(self.state, PacketRouterState::NotConnected));
        let connected_state = PacketRouterStateConnected {
            connection_state: PacketFramerConnectionState::WaitVersionResponse,
            framer: PacketFramer::new(self.encryption_manager.new_connection()),
        };

        self.state = PacketRouterState::Connected(connected_state);
        self.send_packet(build_version_request_packet()).await;
    }
}
impl PacketRouterStateConnected {
    async fn start(
        &mut self,
        outgoing_packet_queue: &mut Receiver<Packet>,
        usb_manager: &mut UsbManager,
    ) {
        loop {
            select! {
                outgoing_packet = outgoing_packet_queue.recv() => {
                    if let Some(outgoing_packet) = outgoing_packet {
                        self.send_packet(outgoing_packet).await
                    } else {
                        break;
                    }
                },
                incoming_event = usb_manager.recv_frame() => match incoming_event {
                    IncomingEvent::Connected => todo!(),
                    IncomingEvent::Closed => return ,
                    IncomingEvent::Frame(frame) => self.process_incoming_frame(frame).await,
                },
                _ = Self::wait_for_timeout(&packet_framer.heartbeat), if packet_framer.heartbeat.is_some() => packet_framer.handle_heartbeat_timer().await,
            }
        }
    }
    async fn process_incoming_frame(&mut self, frame: AAPFrame) {
        let packet = self.framer.process_incoming_frame(frame);
        if let Some(packet) = packet {
            self.dispatch_incoming_packet(packet).await;
        }
    }

    async fn wait_for_timeout(heartbeat: &Option<Heartbeater>) {
        time::sleep_until(heartbeat.as_ref().unwrap().next_timeout).await
    }

    async fn handle_heartbeat_timer(&mut self) {
        let Heartbeater {
            next_timeout,
            state,
            counter,
        } = if let PacketFramerConnectionState::Established { heartbeat } =
            &mut self.connection_state
        {
            heartbeat
        } else {
            panic!();
        };

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
        self.send_packet(ping_packet).await;
    }

    fn restart_heartbeat_timer(&mut self) {
        if let PacketFramerConnectionState::Established { heartbeat } = &mut self.connection_state {
            match heartbeat.state {
                HeartbeatState::WaitingUntilSendingNextPingRequest => {
                    heartbeat.state = HeartbeatState::WaitingUntilSendingNextPingRequest;
                    heartbeat.next_timeout = Self::next_ping_request_instant()
                }
                HeartbeatState::WaitingForPingResponse => {}
            };
        } else {
            panic!();
        }
    }

    fn start_heartbeat_timer() -> Heartbeater { 
Heartbeater {
                state: HeartbeatState::WaitingUntilSendingNextPingRequest,
                counter: 0,
                next_timeout: Self::next_ping_request_instant(),
            }
    }


    fn handle_heartbeat_ping_response(&mut self, packet: Packet) {
        let ping_response = protos::PingResponse::decode(packet.payload()).unwrap();
        info!("ping response received: {:?}", &ping_response);

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
                if (received_counter_value != *counter) {
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

    async fn dispatch_incoming_packet(&mut self, mut packet: Packet) {
        debug!("< {:?}", &packet);
        match self.connection_state {
            PacketFramerConnectionState::WaitVersionResponse => {
                assert!(packet.channel_id == CHANNEL_ID_CONTROL);
                assert!(packet.message_id() == MESSAGE_ID_GET_VERSION_RESPONSE);
                self.connection_state = PacketFramerConnectionState::Handshaking;
                let handshake_message = self
                    .framer
                    .process_handshake_message(&mut [])
                    .unwrap();
                self.send_packet(build_handshake_packet(&handshake_message))
                    .await;
            }
            PacketFramerConnectionState::Handshaking => {
                assert!(packet.channel_id == CHANNEL_ID_CONTROL);
                assert!(packet.message_id() == MESSAGE_ID_HANDSHAKE);
                if let Some(handshake_message) = self
                    .framer
                    .process_handshake_message(packet.payload_mut())
                {
                    self.send_packet(build_handshake_packet(&handshake_message))
                        .await;
                } else if !self.framer
                    .is_handshaking()
                {
                    let heartbeat = Heartbeater { state: (), counter: (), next_timeout: () }
                    self.connection_state = PacketFramerConnectionState::Established { heartbeat};
                    info!("handshaking done");
                    self.send_packet(build_auth_complete_packet()).await;
                }
            }
            PacketFramerConnectionState::Established{..} => {
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

                    self.send_packet(build_service_discovery_response_packet(
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

    async fn send_packet(&mut self, packet: Packet) {
        let connected_state = if let PacketRouterState::Connected(connected_state) = &mut self.state
        {
            connected_state
        } else {
            panic!("Not connected");
        };

        connected_state
            .framer
            .process_outgoing_packet(packet, async |frame| {
                self.usb_manager.send_frame(frame).await.unwrap()
            })
            .await;
    }
}

impl PacketFramer {
    const MAX_FRAME_PAYLOAD_LENGTH: usize = 0x4000;

    fn new(encrypted_connection_manager: EncryptedConnectionManager) -> Self {
        Self {
            pending_frame: None,
            encrypted_connection_manager,
        }
    }

    pub(crate) fn process_handshake_message(&mut self, payload: &mut [u8]) -> Option<Vec<u8>> {
        self.encrypted_connection_manager.process_handshake_message(payload)
    }

    pub(crate) fn is_handshaking(&self) -> bool {
        self.encrypted_connection_manager.is_handshaking()
    }

    async fn process_outgoing_packet(
        &mut self,
        packet: Packet,
        frame_sender: impl AsyncFn(AAPFrame),
    ) {
        debug!("> {:?}", &packet);

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
                frag_info: frame::AAPFrameFragmmentation::Unfragmented,
                r#type: packet.r#type,
                total_payload_length: may_be_encrypted_payload.len(),
                payload: may_be_encrypted_payload,
            })
            .await
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
                    assert!(!self.encrypted_connection_manager.is_handshaking());

                    self.encrypted_connection_manager.encrypt_message(fragment)
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

    fn process_incoming_frame(&mut self, frame: AAPFrame) -> Option<Packet> {
        let frame_complete = matches!(
            frame.frag_info,
            AAPFrameFragmmentation::Last | AAPFrameFragmmentation::Unfragmented
        );

        let mut defragmented_frame = if let Some(mut pending_frame) = self.pending_frame.take() {
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
                self.encrypted_connection_manager
                    .decrypt_message(&mut defragmented_frame.payload)
            } else {
                defragmented_frame.payload
            };
            Some(Packet {
                channel_id: defragmented_frame.channel_id,
                r#type: defragmented_frame.r#type,
                encrypted: defragmented_frame.encrypted,
                message_id_and_payload: payload,
            })
        } else {
            self.pending_frame = Some(defragmented_frame);
            None
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
