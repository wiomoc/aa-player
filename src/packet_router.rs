use std::time::Duration;

use byteorder::{BigEndian, ByteOrder};
use log::{debug, error, info};
use prost::Message;
use tokio::{
    select,
    sync::mpsc::Receiver,
    time::{self, Instant},
};

use crate::{
    encryption::EncryptionManager,
    frame::{AAPFrame, AAPFrameType},
    packet::{Packet, PacketFramer},
    packet_types::{
        CHANNEL_ID_CONTROL, MESSAGE_ID_AUDIO_FOCUS_NOTIFICATION_REQUEST,
        MESSAGE_ID_GET_VERSION_RESPONSE, MESSAGE_ID_HANDSHAKE, MESSAGE_ID_OPEN_CHANNEL_REQUEST,
        MESSAGE_ID_PING_RESPONSE, MESSAGE_ID_SERVICE_DISCOVERY_REQUEST, build_auth_complete_packet,
        build_focus_notification_packet, build_handshake_packet, build_ping_request_packet,
        build_service_discovery_response_packet, build_version_request_packet,
    },
    protos,
    usb::{self, UsbManager},
};

enum PacketRouterConnectionState {
    WaitVersionResponse,
    Handshaking,
    Established { heartbeat: Heartbeater },
}

pub(crate) struct PacketRouter<'a> {
    usb_manager: &'a mut UsbManager,
    framer: PacketFramer,
    connection_state: PacketRouterConnectionState,
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

impl Heartbeater {
    const PING_INTERVAL_MS: u64 = 4000;
    const PING_MAX_RTT_MS: u64 = 1800;

    fn start() -> Heartbeater {
        Heartbeater {
            state: HeartbeatState::WaitingUntilSendingNextPingRequest,
            counter: 0,
            next_timeout: Self::next_ping_request_instant(),
        }
    }

    fn handle_timer_elapsed(&mut self) -> Result<Packet, ()> {
        match self.state {
            HeartbeatState::WaitingUntilSendingNextPingRequest => {
                let ping_packet = build_ping_request_packet(self.counter.to_be_bytes().to_vec());
                self.state = HeartbeatState::WaitingForPingResponse;
                self.next_timeout = Instant::now()
                    .checked_add(Duration::from_millis(Self::PING_MAX_RTT_MS))
                    .unwrap();
                Ok(ping_packet)
            }
            HeartbeatState::WaitingForPingResponse => {
                error!("Ping timeout");
                Err(())
            }
        }
    }

    fn restart_ping_countdown_timer(&mut self) {
        match self.state {
            HeartbeatState::WaitingUntilSendingNextPingRequest => {
                self.state = HeartbeatState::WaitingUntilSendingNextPingRequest;
                self.next_timeout = Self::next_ping_request_instant()
            }
            HeartbeatState::WaitingForPingResponse => {}
        };
    }

    fn handle_heartbeat_ping_response(&mut self, packet: Packet) {
        let ping_response = protos::PingResponse::decode(packet.payload()).unwrap();
        info!("ping response received: {:?}", &ping_response);

        match self.state {
            HeartbeatState::WaitingForPingResponse => {
                self.state = HeartbeatState::WaitingUntilSendingNextPingRequest;
                self.counter += 1;
                self.next_timeout = Self::next_ping_request_instant()
            }
            HeartbeatState::WaitingUntilSendingNextPingRequest => {}
        };
    }

    fn next_ping_request_instant() -> Instant {
        Instant::now()
            .checked_add(Duration::from_millis(Self::PING_INTERVAL_MS))
            .unwrap()
    }
}

enum PacketRouterResult {
    ConnectionClosedInterally,
    ConnectionClosedExternaly,
}

impl<'a> PacketRouter<'a> {
    pub(crate) async fn start(
        mut usb_manager: UsbManager,
        mut outgoing_packet_queue: Receiver<Packet>,
    ) {
        let encryption_manager = EncryptionManager::new();

        loop {
            let incoming_event = usb_manager.recv_frame().await;
            if matches!(incoming_event, usb::IncomingEvent::Connected) {
                let mut router = PacketRouter {
                    connection_state: PacketRouterConnectionState::WaitVersionResponse,
                    framer: PacketFramer::new(encryption_manager.new_connection()),
                    usb_manager: &mut usb_manager,
                };

                router.handle_connection(&mut outgoing_packet_queue).await;
            }
        }
        //usb_manager.terminate();
    }

    async fn handle_connection(
        &mut self,
        outgoing_packet_queue: &mut Receiver<Packet>,
    ) -> PacketRouterResult {
        self.send_packet(build_version_request_packet()).await;

        loop {
            match &mut self.connection_state {
                PacketRouterConnectionState::Established { heartbeat } => {
                    select! {
                        outgoing_packet = outgoing_packet_queue.recv() => {
                            if let Some(outgoing_packet) = outgoing_packet {
                                self.send_packet(outgoing_packet).await
                            } else {
                                return PacketRouterResult::ConnectionClosedInterally;
                            }
                        },
                        incoming_event = self.usb_manager.recv_frame() => match incoming_event {
                            usb::IncomingEvent::Connected => todo!(),
                            usb::IncomingEvent::Closed => return  PacketRouterResult::ConnectionClosedExternaly,
                            usb::IncomingEvent::Frame(frame) => self.process_incoming_frame(frame).await,
                        },
                        _ = time::sleep_until(heartbeat.next_timeout) => {
                            if let Ok(ping_packet) = heartbeat.handle_timer_elapsed() {
                                self.send_packet(ping_packet).await;
                            } else {
                                return PacketRouterResult::ConnectionClosedExternaly;
                            }
                        },
                    }
                }
                _ => match self.usb_manager.recv_frame().await {
                    usb::IncomingEvent::Connected => unreachable!(),
                    usb::IncomingEvent::Closed => {
                        return PacketRouterResult::ConnectionClosedExternaly;
                    }
                    usb::IncomingEvent::Frame(frame) => {
                        self.process_incoming_frame(frame).await;
                    }
                },
            }
        }
    }

    async fn process_incoming_frame(&mut self, frame: AAPFrame) {
        let packet = self.framer.process_incoming_frame(frame);
        if let Some(packet) = packet {
            self.dispatch_incoming_packet(packet).await;
        }
    }

    async fn dispatch_incoming_packet(&mut self, mut packet: Packet) {
        debug!("< {:?}", &packet);
        match &mut self.connection_state {
            PacketRouterConnectionState::WaitVersionResponse => {
                assert!(packet.channel_id == CHANNEL_ID_CONTROL);
                assert!(packet.message_id() == MESSAGE_ID_GET_VERSION_RESPONSE);
                self.connection_state = PacketRouterConnectionState::Handshaking;
                let handshake_message = self.framer.process_handshake_message(&mut []).unwrap();
                self.send_packet(build_handshake_packet(&handshake_message))
                    .await;
            }
            PacketRouterConnectionState::Handshaking => {
                assert!(packet.channel_id == CHANNEL_ID_CONTROL);
                assert!(packet.message_id() == MESSAGE_ID_HANDSHAKE);
                if let Some(handshake_message) =
                    self.framer.process_handshake_message(packet.payload_mut())
                {
                    self.send_packet(build_handshake_packet(&handshake_message))
                        .await;
                } else if !self.framer.is_handshaking() {
                    let heartbeat = Heartbeater::start();
                    self.connection_state = PacketRouterConnectionState::Established { heartbeat };
                    info!("handshaking done");
                    self.send_packet(build_auth_complete_packet()).await;
                }
            }
            PacketRouterConnectionState::Established { heartbeat } => {
                info!("received normal packet");

                heartbeat.restart_ping_countdown_timer();
                self.decode_packet(packet).await;
            }
        }
    }

    async fn decode_packet(&mut self, packet: Packet) {
        if matches!(packet.r#type, AAPFrameType::Control)
            && packet.message_id() == MESSAGE_ID_OPEN_CHANNEL_REQUEST
        {
            let open_channel_request = protos::ChannelOpenRequest::decode(packet.payload()).unwrap();
            info!("open channel {:?}", open_channel_request);
            return;
        }
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
                                    available_while_in_call: Some(true),

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
                                    available_while_in_call: Some(true),

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
                                    available_while_in_call: Some(true),

                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                            protos::Service {
                                id: 7,
                                media_sink_service: Some(protos::MediaSinkService {
                                    available_type: Some(
                                        protos::MediaCodecType::MediaCodecAudioPcm as i32,
                                    ),
                                    audio_type: Some(
                                        protos::AudioStreamType::AudioStreamGuidance as i32,
                                    ),
                                    audio_configs: vec![protos::AudioConfiguration {
                                        sampling_rate: 16000,
                                        number_of_bits: 16,
                                        number_of_channels: 1,
                                    }],
                                    available_while_in_call: Some(true),

                                    ..Default::default()
                                }),
                                ..Default::default()
                            },*/
                            protos::Service {
                                id: 8,
                                media_source_service: Some(protos::MediaSourceService {
                                    available_type: Some(
                                        protos::MediaCodecType::MediaCodecAudioPcm as i32,
                                    ),
                                    audio_config: Some(protos::AudioConfiguration {
                                        sampling_rate: 16000,
                                        number_of_bits: 16,
                                        number_of_channels: 1,
                                    }),
                                    available_while_in_call: Some(true),
                                }),
                                ..Default::default()
                            },
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
                    if let PacketRouterConnectionState::Established { heartbeat } =
                        &mut self.connection_state
                    {
                        heartbeat.handle_heartbeat_ping_response(packet);
                    }
                }
                MESSAGE_ID_AUDIO_FOCUS_NOTIFICATION_REQUEST => {
                    assert!(matches!(
                        self.connection_state,
                        PacketRouterConnectionState::Established { .. }
                    ));
                    let request =
                        protos::AudioFocusRequestNotification::decode(packet.payload()).unwrap();
                    self.send_packet(build_focus_notification_packet(
                        protos::AudioFocusStateType::AudioFocusStateLoss,
                    ))
                    .await;
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
        self.framer
            .process_outgoing_packet(packet, async |frame| {
                self.usb_manager.send_frame(frame).await.unwrap()
            })
            .await;
    }
}
