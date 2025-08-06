#![allow(dead_code)]

use std::sync::Arc;

use log::{debug, error, info};
use prost::Message;
use simple_logger::SimpleLogger;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::{
    packet_router::PacketRouter,
    service::{
        Service,
        audio_renderer::{AudioStreamRendererFactory, AudioStreamSpec},
        media_sink::MediaService,
        video_renderer::{InputService, VideoStreamRendererFactory},
    },
    usb::UsbManager,
};

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
    SimpleLogger::new()
        .with_level(log::LevelFilter::Debug)
        .with_module_level("nusb", log::LevelFilter::Info)
        .init()
        .unwrap();
    info!("starting");
    let cancel_token = CancellationToken::new();

    let usb_manager = UsbManager::start();
    let audio_stream_renderer_factory = Arc::new(AudioStreamRendererFactory::new());
    let cancel_token_cloned = cancel_token.clone();
    tokio::spawn(async move {
        let mut buf = [0u8];
        while &buf != b"c" {
            tokio::io::stdin().read_exact(&mut buf).await.unwrap();
        }

        cancel_token_cloned.cancel();
        error!("control-c");
    });

    PacketRouter::start(
        usb_manager,
        cancel_token,
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
            Box::new(MediaService::new(
                2,
                Arc::new(VideoStreamRendererFactory::new()),
                (),
            )),
            Box::new(InputService::new()),
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
            Box::new(MediaService::new(
                5,
                audio_stream_renderer_factory.clone(),
                AudioStreamSpec {
                    sampling_rate: 48000,
                    channels: 2,
                    sampling_depth_bits: 16,
                    stream_type: protos::AudioStreamType::AudioStreamMedia,
                },
            )),
            /*Box::new(MediaService::new(
                6,
                audio_stream_renderer_factory.clone(),
                AudioStreamSpec {
                    sampling_rate: 16000,
                    channels: 1,
                    sampling_depth_bits: 8,
                    stream_type: protos::AudioStreamType::AudioStreamTelephony,
                },
            )),
            Box::new(MediaService::new(
                7,
                audio_stream_renderer_factory.clone(),
                AudioStreamSpec {
                    sampling_rate: 16000,
                    channels: 1,
                    sampling_depth_bits: 16,
                    stream_type: protos::AudioStreamType::AudioStreamGuidance,
                },
            )),
            Box::new(MediaService::new(
                8,
                audio_stream_renderer_factory,
                AudioStreamSpec {
                    sampling_rate: 16000,
                    channels: 1,
                    sampling_depth_bits: 16,
                    stream_type: protos::AudioStreamType::AudioStreamSystemAudio,
                },
            )),*/
            StubService::new(protos::Service {
                id: 9,
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
    error!("end");

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
                    message_id => debug!("service {service_id} received packet {message_id}"),
                }
            }
        });
    }
}
