#![allow(dead_code)]

use log::info;
use simple_logger::SimpleLogger;
use tokio_util::sync::CancellationToken;

use crate::{
    packet_router::PacketRouter, usb::UsbManager
};

mod encryption;
mod frame;
mod usb;
mod packet;
mod control_channel_packets;
mod packet_router;
mod service;

mod protos {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

#[tokio::main]
async fn main() -> Result<(), ()> {
    SimpleLogger::new().init().unwrap();
    info!("starting");
    let cancel_token = CancellationToken::new();

    let usb_manager = UsbManager::start(cancel_token.clone());
    PacketRouter::start(usb_manager, cancel_token.clone(), &[]).await;

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        cancel_token.cancel();
    });
    Ok(())
}
//
 // protos::Service {
 //                               id: 1,
 //                               sensor_source_service: Some(protos::SensorSourceService {
 //                                   sensors: vec![],
 //                                   location_characterization: None,
 //                                   supported_fuel_types: vec![protos::FuelType::Electric as i32],
 //                                   supported_ev_connector_types: vec![
 //                                       protos::EvConnectorType::Mennekes as i32,
 //                                   ],
 //                               }),
 //                               ..Default::default()
 //                           },
 //                           protos::Service {
 //                               id: 2,
 //                               media_sink_service: Some(protos::MediaSinkService {
 //                                   available_type: Some(
 //                                       protos::MediaCodecType::MediaCodecVideoH264Bp as i32,
 //                                   ),
 //                                   video_configs: vec![protos::VideoConfiguration {
 //                                       codec_resolution: Some(
 //                                           protos::VideoCodecResolutionType::Video1280x720 as i32,
 //                                       ),
 //                                       frame_rate: Some(
 //                                           protos::VideoFrameRateType::VideoFps60 as i32,
 //                                       ),
 //                                       width_margin: Some(0),
 //                                       height_margin: Some(0),
 //                                       density: Some(300),
 //                                       video_codec_type: Some(
 //                                           protos::MediaCodecType::MediaCodecVideoH264Bp as i32,
 //                                       ),
 //                                       ..Default::default()
 //                                   }],
 //                                   available_while_in_call: Some(true),
 //                                   ..Default::default()
 //                               }),
 //                               ..Default::default()
 //                           },
 //                           protos::Service {
 //                               id: 3,
 //                               input_source_service: Some(protos::InputSourceService {
 //                                   touchscreen: vec![protos::input_source_service::TouchScreen {
 //                                       width: 1280,
 //                                       height: 720,
 //                                       r#type: Some(protos::TouchScreenType::Capacitive as i32),
 //                                       is_secondary: Some(false),
 //                                   }],
 //                                   ..Default::default()
 //                               }),
 //                               ..Default::default()
 //                           },
 //                           protos::Service {
 //                               id: 4,
 //                               media_sink_service: Some(protos::MediaSinkService {
 //                                   available_type: Some(
 //                                       protos::MediaCodecType::MediaCodecAudioPcm as i32,
 //                                   ),
 //                                   audio_type: Some(
 //                                       protos::AudioStreamType::AudioStreamSystemAudio as i32,
 //                                   ),
 //                                   audio_configs: vec![protos::AudioConfiguration {
 //                                       sampling_rate: 16000,
 //                                       number_of_bits: 16,
 //                                       number_of_channels: 1,
 //                                   }],
 //                                   available_while_in_call: Some(true),
//
 //                                   ..Default::default()
 //                               }),
 //                               ..Default::default()
 //                           },
 //                           /*protos::Service {
 //                               id: 5,
 //                               media_sink_service: Some(protos::MediaSinkService {
 //                                   available_type: Some(
 //                                       protos::MediaCodecType::MediaCodecAudioPcm as i32,
 //                                   ),
 //                                   audio_type: Some(
 //                                       protos::AudioStreamType::AudioStreamMedia as i32,
 //                                   ),
 //                                   audio_configs: vec![protos::AudioConfiguration {
 //                                       sampling_rate: 48000,
 //                                       number_of_bits: 16,
 //                                       number_of_channels: 2,
 //                                   }],
 //                                   available_while_in_call: Some(true),
//
 //                                   ..Default::default()
 //                               }),
 //                               ..Default::default()
 //                           },
 //                           protos::Service {
 //                               id: 6,
 //                               media_sink_service: Some(protos::MediaSinkService {
 //                                   available_type: Some(
 //                                       protos::MediaCodecType::MediaCodecAudioPcm as i32,
 //                                   ),
 //                                   audio_type: Some(
 //                                       protos::AudioStreamType::AudioStreamTelephony as i32,
 //                                   ),
 //                                   audio_configs: vec![protos::AudioConfiguration {
 //                                       sampling_rate: 16000,
 //                                       number_of_bits: 16,
 //                                       number_of_channels: 1,
 //                                   }],
 //                                   available_while_in_call: Some(true),
//
 //                                   ..Default::default()
 //                               }),
 //                               ..Default::default()
 //                           },
 //                           protos::Service {
 //                               id: 7,
 //                               media_sink_service: Some(protos::MediaSinkService {
 //                                   available_type: Some(
 //                                       protos::MediaCodecType::MediaCodecAudioPcm as i32,
 //                                   ),
 //                                   audio_type: Some(
 //                                       protos::AudioStreamType::AudioStreamGuidance as i32,
 //                                   ),
 //                                   audio_configs: vec![protos::AudioConfiguration {
 //                                       sampling_rate: 16000,
 //                                       number_of_bits: 16,
 //                                       number_of_channels: 1,
 //                                   }],
 //                                   available_while_in_call: Some(true),
//
 //                                   ..Default::default()
 //                               }),
 //                               ..Default::default()
 //                           },*/
 //                           protos::Service {
 //                               id: 8,
 //                               media_source_service: Some(protos::MediaSourceService {
 //                                   available_type: Some(
 //                                       protos::MediaCodecType::MediaCodecAudioPcm as i32,
 //                                   ),
 //                                   audio_config: Some(protos::AudioConfiguration {
 //                                       sampling_rate: 16000,
 //                                       number_of_bits: 16,
 //                                       number_of_channels: 1,
 //                                   }),
 //                                   available_while_in_call: Some(true),
 //                               }),
 //                               ..Default::default()
 //                           },
 //                       ],