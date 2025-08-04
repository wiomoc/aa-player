use std::{collections::HashMap, time::Duration};

use log::{error, info, trace, warn};
use prost::Message;
use tokio::{
    select,
    sync::mpsc::{self, Receiver, Sender},
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;

use crate::{
    control_channel_packets::{
        CHANNEL_ID_CONTROL, MESSAGE_ID_AUDIO_FOCUS_NOTIFICATION_REQUEST,
        MESSAGE_ID_GET_VERSION_RESPONSE, MESSAGE_ID_HANDSHAKE, MESSAGE_ID_OPEN_CHANNEL_REQUEST,
        MESSAGE_ID_PING_RESPONSE, MESSAGE_ID_SERVICE_DISCOVERY_REQUEST,
        MESSAGE_ID_SHUTDOWN_REQUEST, build_auth_complete_packet, build_channel_open_response,
        build_focus_notification_packet, build_handshake_packet, build_ping_request_packet,
        build_service_discovery_response_packet, build_shutdown_request, build_shutdown_response,
        build_version_request_packet,
    },
    encryption::EncryptionManager,
    frame::{AAPFrame, AAPFrameType},
    packet::{Packet, PacketFramer},
    protos,
    service::Service,
    usb::{self, UsbManager},
};

enum PacketRouterConnectionState {
    WaitVersionResponse,
    Handshaking,
    Established { heartbeat: Heartbeater },
    TearingDown { reason: PacketRouterTeardownReason },
}

#[derive(Clone, Copy)]
enum PacketRouterTeardownReason {
    ClosingRouter,
    ConnectionShutdownExternaly,
    ConnectionError,
    ConnectionClosedExternaly,
    ConnectionPingTimeout,
}

pub(crate) struct PacketRouter<'a> {
    services: &'a [Box<dyn Service>],
    channel_mapping: HashMap<u8, (i32, Sender<Packet>)>,
    usb_manager: &'a mut UsbManager,
    framer: PacketFramer,
    outgoing_packet_queue_sender: Sender<Packet>,
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

    fn handle_heartbeat_ping_response(&mut self, packet: Packet) -> Result<(), ()> {
        let ping_response = protos::PingResponse::decode(packet.payload())
            .map_err(|_| error!("couldn't decode ping response request"))?;
        info!("ping response received: {:?}", &ping_response);

        match self.state {
            HeartbeatState::WaitingForPingResponse => {
                self.state = HeartbeatState::WaitingUntilSendingNextPingRequest;
                self.counter += 1;
                self.next_timeout = Self::next_ping_request_instant()
            }
            HeartbeatState::WaitingUntilSendingNextPingRequest => {}
        };
        Ok(())
    }

    fn next_ping_request_instant() -> Instant {
        Instant::now()
            .checked_add(Duration::from_millis(Self::PING_INTERVAL_MS))
            .unwrap()
    }
}

impl<'a> PacketRouter<'a> {
    pub(crate) async fn start(
        mut usb_manager: UsbManager,
        cancel_token: CancellationToken,
        services: &[Box<dyn Service>],
    ) {
        let encryption_manager = EncryptionManager::new();

        loop {
            let incoming_event = select! {
                incoming_event = usb_manager.recv_frame() => incoming_event,
                _ = cancel_token.cancelled() => return
            };
            if matches!(incoming_event, usb::IncomingEvent::Connected) {
                let (outgoing_packet_queue_sender, mut outgoing_packet_queue_receiver) =
                    mpsc::channel(2);

                let mut router = PacketRouter {
                    connection_state: PacketRouterConnectionState::WaitVersionResponse,
                    framer: PacketFramer::new(encryption_manager.new_connection()),
                    usb_manager: &mut usb_manager,
                    services,
                    outgoing_packet_queue_sender,
                    channel_mapping: HashMap::new(),
                };

                let cancel_reason = router
                    .handle_connection(&mut outgoing_packet_queue_receiver, &cancel_token)
                    .await;
                if matches!(
                    cancel_reason,
                    PacketRouterTeardownReason::ConnectionClosedExternaly
                ) {
                    continue;
                }
                usb_manager.send_connection_close().await.unwrap();

                if matches!(cancel_reason, PacketRouterTeardownReason::ClosingRouter) {
                    return;
                }
            }
        }
        //usb_manager.terminate();
    }

    async fn handle_connection(
        &mut self,
        outgoing_packet_queue: &mut Receiver<Packet>,
        cancel_token: &CancellationToken,
    ) -> PacketRouterTeardownReason {
        let send_packet_result = self.send_packet(build_version_request_packet()).await;
        if send_packet_result.is_err() {
            return PacketRouterTeardownReason::ConnectionError;
        }

        loop {
            match &mut self.connection_state {
                PacketRouterConnectionState::Established { heartbeat } => {
                    select! {
                        biased;
                        outgoing_packet = outgoing_packet_queue.recv() => {
                            if let Some(outgoing_packet) = outgoing_packet {
                                let send_packet_result = self.send_packet(outgoing_packet).await;
                                if send_packet_result.is_err() {
                                    return PacketRouterTeardownReason::ConnectionError;
                                }
                            } else {
                                unreachable!();
                            }
                        },
                        incoming_event = self.usb_manager.recv_frame() => match incoming_event {
                            usb::IncomingEvent::Connected => todo!(),
                            usb::IncomingEvent::Closed => return PacketRouterTeardownReason::ConnectionClosedExternaly,
                            usb::IncomingEvent::Frame(frame)  => {
                                if self.process_incoming_frame(frame).await.is_err() {
                                    return PacketRouterTeardownReason::ConnectionError;
                                }
                            }
                        },
                        _ = time::sleep_until(heartbeat.next_timeout) => {
                            if let Ok(ping_packet) = heartbeat.handle_timer_elapsed() {
                                let send_packet_result = self.send_packet(ping_packet).await;
                                if send_packet_result.is_err() {
                                    return PacketRouterTeardownReason::ConnectionError;
                                }
                            } else {
                                return PacketRouterTeardownReason::ConnectionPingTimeout;
                            }
                        },
                        _ = cancel_token.cancelled() => {
                            self.send_teardown().await;
                            return PacketRouterTeardownReason::ClosingRouter;
                        }
                    }
                }
                PacketRouterConnectionState::TearingDown { reason } => return *reason,
                _ => select! {
                    biased;
                    incoming_event = self.usb_manager.recv_frame() => match incoming_event {
                        usb::IncomingEvent::Connected => unreachable!(),
                        usb::IncomingEvent::Closed => {
                            return PacketRouterTeardownReason::ConnectionClosedExternaly;
                        }
                        usb::IncomingEvent::Frame(frame) => {
                            if self.process_incoming_frame(frame).await.is_err() {
                                return PacketRouterTeardownReason::ConnectionError;
                            }
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        self.send_teardown().await;
                        return PacketRouterTeardownReason::ClosingRouter;
                    }
                },
            }
        }
    }

    async fn process_incoming_frame(&mut self, frame: AAPFrame) -> Result<(), ()> {
        let packet = self.framer.process_incoming_frame(frame)?;
        if let Some(packet) = packet {
            self.dispatch_incoming_packet(packet).await?;
        }
        Ok(())
    }

    async fn dispatch_incoming_packet(&mut self, mut packet: Packet) -> Result<(), ()> {
        trace!("< {:?}", &packet);
        match &mut self.connection_state {
            PacketRouterConnectionState::WaitVersionResponse => {
                assert!(packet.channel_id == CHANNEL_ID_CONTROL);
                assert!(packet.message_id() == MESSAGE_ID_GET_VERSION_RESPONSE);
                self.connection_state = PacketRouterConnectionState::Handshaking;
                let handshake_message = self.framer.process_handshake_message(&mut [])?.unwrap();
                self.send_packet(build_handshake_packet(&handshake_message))
                    .await?;
            }
            PacketRouterConnectionState::Handshaking => {
                assert!(packet.channel_id == CHANNEL_ID_CONTROL);
                assert!(packet.message_id() == MESSAGE_ID_HANDSHAKE);
                if let Some(handshake_message) = self
                    .framer
                    .process_handshake_message(packet.payload_mut())?
                {
                    self.send_packet(build_handshake_packet(&handshake_message))
                        .await?;
                } else if !self.framer.is_handshaking() {
                    let heartbeat = Heartbeater::start();
                    self.connection_state = PacketRouterConnectionState::Established { heartbeat };
                    info!("handshaking done");
                    self.send_packet(build_auth_complete_packet()).await?;
                }
            }
            PacketRouterConnectionState::Established { heartbeat } => {
                heartbeat.restart_ping_countdown_timer();
                let decode_packet_result = self.decode_packet(packet).await;
                if decode_packet_result.is_err() {
                    self.send_teardown().await;
                }
                return decode_packet_result;
            }
            PacketRouterConnectionState::TearingDown { .. } => {
                info!("Ignoring incoming packet during teardown");
            }
        }
        Ok(())
    }

    async fn decode_packet(&mut self, packet: Packet) -> Result<(), ()> {
        if matches!(packet.r#type, AAPFrameType::Control)
            && packet.message_id() == MESSAGE_ID_OPEN_CHANNEL_REQUEST
        {
            self.handle_open_channel_request(packet).await?;
        } else if packet.channel_id == CHANNEL_ID_CONTROL {
            self.handle_control_packet(packet).await?;
        } else if let Some((_service_id, service_incoming_packet_queue)) =
            self.channel_mapping.get(&packet.channel_id)
        {
            let result = service_incoming_packet_queue.send(packet).await;
            if result.is_err() {
                warn!("Could not forward packet to service");
            }
        } else {
            info!("received packet on unknown channel {}", packet.channel_id);
        }
        Ok(())
    }

    async fn send_teardown(&mut self) {
        let _ = self.send_packet(build_shutdown_request()).await;
    }

    async fn handle_open_channel_request(&mut self, packet: Packet) -> Result<(), ()> {
        let open_channel_request = protos::ChannelOpenRequest::decode(packet.payload())
            .map_err(|_| error!("couldn't decode channel open request"))?;
        info!("open channel {:?}", &open_channel_request);

        let service_id = open_channel_request.service_id;
        let channel_exists_for_service = self
            .channel_mapping
            .values()
            .any(|(existing_service_id, _sender)| *existing_service_id == service_id);
        if channel_exists_for_service {
            warn!("Service {service_id} has already an opened channel");
            self.send_packet(build_channel_open_response(
                packet.channel_id,
                protos::MessageStatus::StatusInvalidService,
            ))
            .await?;
            return Err(());
        }

        let service = self
            .services
            .iter()
            .find(|service| service.get_id() == service_id);

        let service = if let Some(service) = service {
            service
        } else {
            warn!("Unknown service {service_id}");
            self.send_packet(build_channel_open_response(
                packet.channel_id,
                protos::MessageStatus::StatusInvalidService,
            ))
            .await?;
            return Err(());
        };

        let (incoming_packet_queue_sender, incoming_packet_queue_receiver) = mpsc::channel(2);

        let packet_sender = ChannelPacketSender {
            channel_id: packet.channel_id,
            sender: self.outgoing_packet_queue_sender.clone(),
        };

        self.channel_mapping.insert(
            packet.channel_id,
            (service_id, incoming_packet_queue_sender),
        );

        service.instanciate(packet_sender, incoming_packet_queue_receiver);
        self.send_packet(build_channel_open_response(
            packet.channel_id,
            protos::MessageStatus::StatusSuccess,
        ))
        .await?;
        Ok(())
    }

    async fn handle_control_packet(&mut self, packet: Packet) -> Result<(), ()> {
        match packet.message_id() {
            MESSAGE_ID_SERVICE_DISCOVERY_REQUEST => {
                let service_discovery_request =
                    protos::ServiceDiscoveryRequest::decode(packet.payload());
                info!(
                    "service discovery request received: {:?}",
                    &service_discovery_request
                );

                let service_discovery_response = protos::ServiceDiscoveryResponse {
                    services: self
                        .services
                        .iter()
                        .map(|service| service.get_descriptor())
                        .collect(),
                    display_name: Some("aa_player".to_string()),
                    connection_configuration: Some(protos::ConnectionConfiguration {
                        ping_configuration: Some(protos::PingConfiguration {
                            interval_ms: Some(Heartbeater::PING_INTERVAL_MS as u32),
                            timeout_ms: Some(Heartbeater::PING_MAX_RTT_MS as u32),
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
                .await?;
            }
            MESSAGE_ID_PING_RESPONSE => {
                if let PacketRouterConnectionState::Established { heartbeat } =
                    &mut self.connection_state
                {
                    heartbeat.handle_heartbeat_ping_response(packet)?;
                }
            }
            MESSAGE_ID_AUDIO_FOCUS_NOTIFICATION_REQUEST => {
                assert!(matches!(
                    self.connection_state,
                    PacketRouterConnectionState::Established { .. }
                ));
                let _request = protos::AudioFocusRequestNotification::decode(packet.payload())
                    .map_err(|_| error!("couldn't decode audio focus request"))?;
                self.send_packet(build_focus_notification_packet(
                    protos::AudioFocusStateType::AudioFocusStateGainMediaOnly,
                ))
                .await?;
            }
            MESSAGE_ID_SHUTDOWN_REQUEST => {
                self.connection_state = PacketRouterConnectionState::TearingDown {
                    reason: PacketRouterTeardownReason::ConnectionShutdownExternaly,
                };
                let _ = self.send_packet(build_shutdown_response()).await;
            }
            message_id => {
                info!("received packet on control channel with unknown message_id {message_id}");
                return Err(());
            }
        }
        Ok(())
    }

    async fn send_packet(&mut self, packet: Packet) -> Result<(), ()> {
        self.framer
            .process_outgoing_packet(packet, async |frame| {
                self.usb_manager.send_frame(frame).await.unwrap()
            })
            .await
    }
}

pub(crate) struct ChannelPacketSender {
    sender: Sender<Packet>,
    channel_id: u8,
}

impl ChannelPacketSender {
    pub(crate) async fn send_proto(
        &self,
        message_id: u16,
        payload: &impl Message,
    ) -> Result<(), mpsc::error::SendError<Packet>> {
        let packet = Packet::new_from_proto_message(
            self.channel_id,
            AAPFrameType::ChannelSpecific,
            true,
            message_id,
            payload,
        );
        self.sender.send(packet).await
    }
}
