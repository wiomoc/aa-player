use std::{
    cmp::min,
    sync::{Arc, atomic::AtomicBool},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{error, warn};
use tokio::sync::Mutex;

use crate::{
    protos,
    service::media_sink::{StreamRenderer, StreamRendererFactory, TimestampMicros},
};

#[derive(Clone)]
pub(crate) struct AudioStreamSpec {
    pub(crate) sampling_rate: u32,
    pub(crate) sampling_depth_bits: u8,
    pub(crate) channels: u8,
    pub(crate) stream_type: protos::AudioStreamType,
}

pub(crate) struct AudioStreamRendererFactory {
    host: cpal::Host,
    device: cpal::Device,
}

impl AudioStreamRendererFactory {
    pub(crate) fn new() -> Self {
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
            .ok_or(())
            .map_err(|_| error!("couldn't find usable audio config"))?
            .config();
        AudioStreamRenderer::new(self.device.clone(), config)
    }
}

pub(crate) struct AudioStreamRenderer {
    device: cpal::Device,
    stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl AudioStreamRenderer {
    fn new(device: cpal::Device, config: cpal::StreamConfig) -> Result<Self, ()> {
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
            .map_err(|err| {
                warn!("Could not build audio output stream {err:?}");
            })?;
        Ok(AudioStreamRenderer {
            device,
            stream,
            buffer,
        })
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
