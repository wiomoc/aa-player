use std::{i16, time::SystemTime};

use byteorder::{BigEndian, ByteOrder};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{debug, error, info, warn};
use prost::Message;

use crate::{packet_router::ChannelPacketSender, protos, service::Service};

pub(crate) struct AudioSourceService {
    service_id: u8,
}

impl AudioSourceService {
    const MESSAGE_ID_TIMESTAMPED_CONTENT: u16 = 0x0000;
    const MESSAGE_ID_CONTENT: u16 = 0x0001;
    const MESSAGE_ID_SETUP: u16 = 0x8000;
    const MESSAGE_ID_START: u16 = 0x8001;
    const MESSAGE_ID_STOP: u16 = 0x8002;
    const MESSAGE_ID_CONFIG: u16 = 0x8003;
    const MESSAGE_ID_ACK: u16 = 0x8004;
    const MESSAGE_ID_MICROPHONE_REQUEST: u16 = 0x8005;
    const MESSAGE_ID_MICROPHONE_RESPONSE: u16 = 0x8006;

    const MAX_UNACKED_CONTENT_MESSAGES: usize = 20;

    const SAMPLING_RATE: u32 = 16000;
    const SAMPLING_DEPTH_BITS: u8 = 16;
    const CHANNELS: u8 = 1;

    pub(crate) fn new(service_id: u8) -> Self {
        Self { service_id }
    }
}

impl Service for AudioSourceService {
    fn get_id(&self) -> i32 {
        self.service_id as i32
    }

    fn get_descriptor(&self) -> protos::Service {
        protos::Service {
            id: self.get_id(),
            media_source_service: Some(protos::MediaSourceService {
                available_type: Some(protos::MediaCodecType::MediaCodecAudioPcm as i32),
                audio_config: Some(protos::AudioConfiguration {
                    sampling_rate: Self::SAMPLING_RATE,
                    number_of_bits: Self::SAMPLING_DEPTH_BITS as u32,
                    number_of_channels: Self::CHANNELS as u32,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn instanciate(
        &self,
        packet_sender: ChannelPacketSender,
        mut packet_receiver: tokio::sync::mpsc::Receiver<crate::packet::Packet>,
    ) {
        tokio::spawn(async move {
            let device = cpal::default_host()
                .default_input_device()
                .ok_or(())
                .map_err(|_| error!("Not audio input device found"))?;

            info!("dev {:?}", device.name());
            let config = device
                .supported_input_configs()
                .unwrap()
                .map(|c| {
                    info!("{:?}", &c);
                    c
                })
                .map(|config| config.with_max_sample_rate())
                .find(|config| {
                    config.channels() == Self::CHANNELS as u16
                        && config.sample_format() == cpal::SampleFormat::I16
                })
                .ok_or(())
                .map_err(|_| error!("Not supported audio input config found"))?
                .config();
            let mut stream: Option<cpal::Stream> = None;
            while let Some(packet) = packet_receiver.recv().await {
                match packet.message_id() {
                    Self::MESSAGE_ID_SETUP => {
                        let config = protos::Setup::decode(packet.payload())
                            .map_err(|err| warn!("could not decode setup message {err:?}"))?;
                        info!("received setup {:?}", &config);
                        packet_sender
                            .send_proto(
                                Self::MESSAGE_ID_CONFIG,
                                &protos::Config {
                                    max_unacked: None,
                                    configuration_indices: vec![0],
                                    status: if true {
                                        protos::config::Status::Ready
                                    } else {
                                        protos::config::Status::Wait
                                    } as i32,
                                },
                            )
                            .await
                            .unwrap();
                    }
                    Self::MESSAGE_ID_MICROPHONE_REQUEST => {
                        let request =
                            protos::MicrophoneRequest::decode(packet.payload()).map_err(|err| {
                                warn!("could not decode microphone request message {err:?}")
                            })?;
                        info!("request {:?}", &request);
                        stream.take();
                        if !request.open {
                            packet_sender
                                .send_proto(
                                    Self::MESSAGE_ID_MICROPHONE_RESPONSE,
                                    &protos::MicrophoneResponse {
                                        session_id: Some(0),
                                        status: 1,
                                    },
                                )
                                .await
                                .unwrap();

                            continue;
                        }
                        packet_sender
                            .send_proto(
                                Self::MESSAGE_ID_MICROPHONE_RESPONSE,
                                &protos::MicrophoneResponse {
                                    session_id: Some(0),
                                    status: 0,
                                },
                            )
                            .await
                            .unwrap();

                        let packet_sender_weak = packet_sender.downgrade();
                        let new_stream = device
                            .build_input_stream_raw(
                                &config,
                                cpal::SampleFormat::I16,
                                move |data: &cpal::Data, info: &_| {
                                    //let converted = samplerate::convert(
                                    //    config.sample_rate.0,
                                    //    Self::SAMPLING_RATE,
                                    //    1,
                                    //    samplerate::ConverterType::SincMediumQuality,
                                    //    &data,
                                    //)
                                    //.unwrap();
                                    //
                                    //let mut samples = vec![0u8; converted.len() * 2];
                                    //for (i, sample) in converted.iter().enumerate() {
                                    //    let sample_i16 = (sample * i16::MAX as f32).round() as i16;
                                    //    BigEndian::write_i16(&mut samples[(i * 2)..], sample_i16);
                                    //}

                                    //info!("samples {:?}", &samples);

                                    let mut buf = Vec::with_capacity(data.bytes().len() + 8);
                                    buf.extend_from_slice(
                                        &(SystemTime::now()
                                            .duration_since(SystemTime::UNIX_EPOCH)
                                            .unwrap()
                                            .as_micros()
                                            as u64)
                                            .to_be_bytes(),
                                    );

                                    buf.extend_from_slice(data.bytes());
                                    packet_sender_weak
                                        .blocking_send_raw(
                                            Self::MESSAGE_ID_TIMESTAMPED_CONTENT,
                                            &buf,
                                        )
                                        .unwrap();
                                },
                                |_err| {},
                                None,
                            )
                            .unwrap();
                        new_stream.play().unwrap();
                        stream = Some(new_stream);
                    }
                    Self::MESSAGE_ID_ACK => {}
                    message_id => debug!("service received packet {message_id}"),
                }
            }
            Ok::<(), ()>(())
        });
    }
}
