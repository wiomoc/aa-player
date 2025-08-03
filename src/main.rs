#![allow(dead_code)]

use log::{debug, info};
use prost::Message;
use simple_logger::SimpleLogger;
use tokio_util::sync::CancellationToken;

use crate::{packet_router::PacketRouter, service::Service, usb::UsbManager};

mod control_channel_packets;
mod encryption;
mod frame;
mod packet;
mod packet_router;
mod service;
mod usb;

mod protos {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

#[tokio::main]
async fn main() -> Result<(), ()> {
    SimpleLogger::new().init().unwrap();
    info!("starting");
    let cancel_token = CancellationToken::new();

    let usb_manager = UsbManager::start(cancel_token.clone());
    PacketRouter::start(
        usb_manager,
        cancel_token.clone(),
        &[
            StubService::new(protos::Service {
                id: 1,
                sensor_source_service: Some(protos::SensorSourceService {
                    sensors: vec![],
                    location_characterization: None,
                    supported_fuel_types: vec![protos::FuelType::Electric as i32],
                    supported_ev_connector_types: vec![protos::EvConnectorType::Mennekes as i32],
                }),
                ..Default::default()
            }),
            StubService::new(protos::Service {
                id: 2,
                media_sink_service: Some(protos::MediaSinkService {
                    available_type: Some(protos::MediaCodecType::MediaCodecVideoH264Bp as i32),
                    video_configs: vec![protos::VideoConfiguration {
                        codec_resolution: Some(
                            protos::VideoCodecResolutionType::Video1280x720 as i32,
                        ),
                        frame_rate: Some(protos::VideoFrameRateType::VideoFps60 as i32),
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
            }),
            StubService::new(protos::Service {
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
            }),
            StubService::new(protos::Service {
                id: 4,
                media_sink_service: Some(protos::MediaSinkService {
                    available_type: Some(protos::MediaCodecType::MediaCodecAudioPcm as i32),
                    audio_type: Some(protos::AudioStreamType::AudioStreamSystemAudio as i32),
                    audio_configs: vec![protos::AudioConfiguration {
                        sampling_rate: 16000,
                        number_of_bits: 16,
                        number_of_channels: 1,
                    }],
                    available_while_in_call: Some(true),

                    ..Default::default()
                }),
                ..Default::default()
            }),
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
            StubService::new(protos::Service {
                id: 8,
                media_source_service: Some(protos::MediaSourceService {
                    available_type: Some(protos::MediaCodecType::MediaCodecAudioPcm as i32),
                    audio_config: Some(protos::AudioConfiguration {
                        sampling_rate: 16000,
                        number_of_bits: 16,
                        number_of_channels: 1,
                    }),
                    available_while_in_call: Some(true),
                }),
                ..Default::default()
            }),
        ],
    )
    .await;

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        cancel_token.cancel();
    });
    Ok(())
}

struct StubService {
    descriptor: protos::Service,
}

impl StubService {
    fn new(descriptor: protos::Service) -> Box<Self> {
        Box::new(Self { descriptor })
    }
}

impl Service for StubService {
    fn get_id(&self) -> i32 {
        self.descriptor.id
    }
    fn get_descriptor(&self) -> protos::Service {
        self.descriptor.clone()
    }

    fn instanciate(
        &self,
        packet_sender: packet_router::ChannelPacketSender,
        mut packet_receiver: tokio::sync::mpsc::Receiver<packet::Packet>,
    ) {
        let service_id = self.descriptor.id;
        tokio::spawn(async move {
            while let Some(packet) = packet_receiver.recv().await {
                match packet.message_id() {
                    0x8002 => {
                        debug!(
                            "service {} received packet {:?}",
                            service_id,
                            protos::KeyBindingRequest::decode(packet.payload()).unwrap()
                        );
                        packet_sender
                            .send_proto(0x8003, &protos::KeyBindingResponse { status: 0 })
                            .await
                            .unwrap();
                    }
                    0x8000 => {
                        debug!(
                            "service {} received packet {:?}",
                            service_id,
                            protos::Setup::decode(packet.payload()).unwrap()
                        );
                        packet_sender
                            .send_proto(
                                0x8003,
                                &protos::Config {
                                    status: protos::config::Status::Ready as i32,
                                    configuration_indices: vec![0],
                                    max_unacked: Some(1),
                                },
                            )
                            .await
                            .unwrap();
                    }
                    0x8007 => {
                        debug!(
                            "service {} received packet {:?}",
                            service_id,
                            protos::VideoFocusRequestNotification::decode(packet.payload())
                                .unwrap()
                        );
                        packet_sender
                            .send_proto(
                                0x8008,
                                &protos::VideoFocusNotification {
                                    focus: Some(protos::VideoFocusMode::VideoFocusProjected as i32),
                                    ..Default::default()
                                },
                            )
                            .await
                            .unwrap();
                    }
                    message_id => debug!("service {} received packet {}", service_id, message_id),
                }
            }
        });
    }
}
