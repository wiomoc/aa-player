#![allow(dead_code)]

use std::{
    cmp::min,
    sync::{Arc, atomic::AtomicBool},
};

use byteorder::{BigEndian, ByteOrder};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{debug, info, warn};
use prost::Message;
use simple_logger::SimpleLogger;
use tokio::{fs::File, io::AsyncWriteExt, sync::Mutex};
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
    SimpleLogger::new()
        .with_level(log::LevelFilter::Debug)
        .with_module_level("nusb", log::LevelFilter::Info)
        .init()
        .unwrap();
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
            Box::new(MediaService::new(
                5,
                Arc::new(AudioStreamRendererFactory::new()),
                AudioStreamSpec {
                    sampling_rate: 48000,
                    channels: 2,
                    sampling_depth_bits: 16,
                    stream_type: protos::AudioStreamType::AudioStreamMedia,
                },
            )),
            /*protos::Service {
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

type TimestampMicros = u64;

trait StreamRendererFactory {
    type Renderer: StreamRenderer + Send;
    type Spec: Clone + Send;

    fn get_descriptor(&self, spec: &Self::Spec) -> protos::MediaSinkService;

    fn create(&self, spec: &Self::Spec) -> Result<Self::Renderer, ()>;
}
trait StreamRenderer {
    fn start(&mut self);
    fn stop(&mut self);
    fn add_content(
        &mut self,
        content: &[u8],
        timestamp: Option<TimestampMicros>,
    ) -> impl Future<Output = ()> + Send;
}

struct MediaService<P: StreamRendererFactory + Send + Sync> {
    service_id: u8,
    spec: P::Spec,
    stream_processor_factory: Arc<P>,
}

impl<P: StreamRendererFactory + Send + Sync> MediaService<P> {
    const MESSAGE_ID_TIMESTAMPED_CONTENT: u16 = 0x0000;
    const MESSAGE_ID_CONTENT: u16 = 0x0001;
    const MESSAGE_ID_SETUP: u16 = 0x8000;
    const MESSAGE_ID_START: u16 = 0x8001;
    const MESSAGE_ID_STOP: u16 = 0x8002;
    const MESSAGE_ID_CONFIG: u16 = 0x8003;
    const MESSAGE_ID_ACK: u16 = 0x8004;

    const MAX_UNACKED_CONTENT_MESSAGES: usize = 20;
    const SEND_ACK_AFTER_CONTENT_MESSAGES: usize = Self::MAX_UNACKED_CONTENT_MESSAGES - 5;

    fn new(service_id: u8, stream_processor_factory: Arc<P>, spec: P::Spec) -> Self {
        Self {
            service_id,
            stream_processor_factory,
            spec,
        }
    }
}

impl<P: StreamRendererFactory + Send + Sync + 'static> Service for MediaService<P> {
    fn get_id(&self) -> i32 {
        self.service_id as i32
    }

    fn get_descriptor(&self) -> protos::Service {
        protos::Service {
            id: self.get_id(),
            media_sink_service: Some(self.stream_processor_factory.get_descriptor(&self.spec)),
            ..Default::default()
        }
    }

    fn instanciate(
        &self,
        packet_sender: packet_router::ChannelPacketSender,
        mut packet_receiver: tokio::sync::mpsc::Receiver<packet::Packet>,
    ) {
        let renderer_factory = self.stream_processor_factory.clone();
        let spec = self.spec.clone();
        tokio::spawn(async move {
            let mut renderer: Option<P::Renderer> = None;
            let mut session_id: Option<i32> = None;
            let mut unacked_content_messages = Vec::new();
            while let Some(packet) = packet_receiver.recv().await {
                match packet.message_id() {
                    Self::MESSAGE_ID_TIMESTAMPED_CONTENT => {
                        let renderer = renderer.as_mut().expect("Not setuped yet");
                        let timestamp_micros = BigEndian::read_u64(packet.payload());
                        let content = &packet.payload()[8..];
                        renderer.add_content(content, Some(timestamp_micros)).await;
                        unacked_content_messages.push(timestamp_micros);
                        if unacked_content_messages.len() >= Self::MAX_UNACKED_CONTENT_MESSAGES - 1
                        {
                            packet_sender
                                .send_proto(
                                    Self::MESSAGE_ID_ACK,
                                    &protos::Ack {
                                        ack: Some(1),
                                        receive_timestamp_ns: unacked_content_messages,
                                        session_id: session_id.unwrap(),
                                    },
                                )
                                .await
                                .unwrap();
                            unacked_content_messages = Vec::new();
                        }
                    }
                    Self::MESSAGE_ID_SETUP => {
                        let config = protos::Setup::decode(packet.payload()).unwrap();
                        assert_eq!(
                            config.r#type(),
                            renderer_factory.get_descriptor(&spec).available_type()
                        );
                        renderer = renderer_factory.create(&spec).ok();
                        packet_sender
                            .send_proto(
                                Self::MESSAGE_ID_CONFIG,
                                &protos::Config {
                                    max_unacked: Some(Self::MAX_UNACKED_CONTENT_MESSAGES as u32),
                                    configuration_indices: vec![0],

                                    status: if renderer.is_some() {
                                        protos::config::Status::Ready
                                    } else {
                                        protos::config::Status::Wait
                                    } as i32,
                                },
                            )
                            .await
                            .unwrap();
                    }
                    Self::MESSAGE_ID_START => {
                        let renderer = renderer.as_mut().expect("Not setuped yet");
                        renderer.start();
                        let start_config = protos::Start::decode(packet.payload()).unwrap();
                        session_id = Some(start_config.session_id);
                    }
                    Self::MESSAGE_ID_STOP => {
                        let renderer = renderer.as_mut().expect("Not setuped yet");
                        renderer.stop();
                    }
                    _ => warn!("Received packet with unknown message id {packet:?}"),
                }
            }
        });
    }
}

#[derive(Clone)]
struct AudioStreamSpec {
    sampling_rate: u32,
    sampling_depth_bits: u8,
    channels: u8,
    stream_type: protos::AudioStreamType,
}

struct AudioStreamRendererFactory {
    host: cpal::Host,
    device: cpal::Device,
}

impl AudioStreamRendererFactory {
    fn new() -> Self {
        let host = cpal::default_host();
        let device = host.default_output_device().unwrap();
        AudioStreamRendererFactory { host, device }
    }
}

impl StreamRendererFactory for AudioStreamRendererFactory {
    type Renderer = AudioStreamRenderer;
    type Spec = AudioStreamSpec;

    fn get_descriptor(&self, spec: &Self::Spec) -> protos::MediaSinkService {
        protos::MediaSinkService {
            available_type: Some(protos::MediaCodecType::MediaCodecAudioPcm as i32),
            audio_configs: vec![protos::AudioConfiguration {
                sampling_rate: spec.sampling_rate,
                number_of_bits: spec.sampling_depth_bits as u32,
                number_of_channels: spec.channels as u32,
            }],
            audio_type: Some(spec.stream_type as i32),
            available_while_in_call: Some(true),
            ..Default::default()
        }
    }

    fn create(&self, spec: &Self::Spec) -> Result<Self::Renderer, ()> {
        let config = self
            .device
            .supported_output_configs()
            .unwrap()
            .filter_map(|config| config.try_with_sample_rate(cpal::SampleRate(spec.sampling_rate)))
            .find(|config: &_| {
                config.channels() == (spec.channels as cpal::ChannelCount)
                    && (config.sample_format().sample_size() * 8)
                        == spec.sampling_depth_bits as usize
            })
            .unwrap()
            .config();
        Ok(AudioStreamRenderer::new(self.device.clone(), config))
    }
}

struct AudioStreamRenderer {
    device: cpal::Device,
    stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl AudioStreamRenderer {
    fn new(device: cpal::Device, config: cpal::StreamConfig) -> Self {
        let buffer = Arc::new(Mutex::new(Vec::<u8>::with_capacity(10000)));
        let buffer_clone = buffer.clone();
        let mut first = AtomicBool::new(true);
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let mut buffer: tokio::sync::MutexGuard<'_, Vec<u8>> =
                        buffer_clone.blocking_lock();
                    let mut count = min(data.len() * 2, buffer.len());
                    let first = first.get_mut();
                    if *first {
                        count = 0;
                        *first = false;
                    }

                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            buffer.as_ptr(),
                            data.as_mut_ptr().cast(),
                            count,
                        )
                    }

                    buffer.drain(..count);
                    data[(count / 2)..].fill(0);
                },
                move |err| {
                    // react to errors here.
                },
                None,
            )
            .unwrap();
        AudioStreamRenderer {
            device,
            stream,
            buffer,
        }
    }
}

impl StreamRenderer for AudioStreamRenderer {
    fn start(&mut self) {
        self.stream.play().unwrap();
    }

    fn stop(&mut self) {
        self.stream.pause().unwrap();
    }

    async fn add_content(&mut self, content: &[u8], timestamp: Option<TimestampMicros>) {
        let mut buffer = self.buffer.lock().await;
        buffer.extend(content);
    }
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
            let mut stream_file = File::create("stream.h264").await.unwrap();
            while let Some(packet) = packet_receiver.recv().await {
                match packet.message_id() {
                    0x0000 => {
                        if service_id == 2 {
                            stream_file.write_all(&packet.payload()[8..]).await.unwrap();
                            stream_file.flush().await.unwrap();
                             packet_sender
                                .send_proto(
                                    0x8004,
                                    &protos::Ack {
                                        ack: Some(1),
                                        receive_timestamp_ns: vec![BigEndian::read_u64(packet.payload())],
                                        session_id: 1,
                                    },
                                )
                                .await
                                .unwrap();
                        }
                    }
                    0x0001 => {
                        if service_id == 2 {
                            stream_file.write_all(packet.payload()).await.unwrap();
                            stream_file.flush().await.unwrap();
                        }
                    }
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
                        //packet_sender
                        //    .send_proto(
                        //        0x8003,
                        //        &protos::Config {
                        //            status: protos::config::Status::Wait as i32,
                        //            configuration_indices: vec![0],
                        //            max_unacked: Some(1),
                        //        },
                        //    )
                        //    .await
                        //    .unwrap();
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
