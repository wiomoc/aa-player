use std::{
    cmp::min, collections::VecDeque, io::Read, sync::{atomic::AtomicBool, Arc}
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{error, warn};
use tokio::sync::Mutex;

use crate::{
    protos,
    service::media_sink::{StreamRenderer, StreamRendererFactory, TimestampMicros},
};

#[derive(Clone, Copy)]
pub(crate) enum Channels {
    Mono,
    Stereo,
}

impl Channels {
    fn count(self) -> u8 {
        match self {
            Channels::Mono => 1,
            Channels::Stereo => 2,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SamplingDepth {
    Bits8,
    Bits16,
}

impl SamplingDepth {
    fn bits(self) -> u8 {
        match self {
            SamplingDepth::Bits8 => 8,
            SamplingDepth::Bits16 => 16,
        }
    }

    fn bytes(self) -> u8 {
        match self {
            SamplingDepth::Bits8 => 1,
            SamplingDepth::Bits16 => 2,
        }
    }
}

#[derive(Clone)]
pub(crate) struct AudioStreamSpec {
    pub(crate) sampling_rate: u32,
    pub(crate) sampling_depth: SamplingDepth,
    pub(crate) channels: Channels,
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
                number_of_bits: spec.sampling_depth.bits() as u32,
                number_of_channels: spec.channels.count() as u32,
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
                let channel_count_acceptable = match (spec.channels, config.channels()) {
                    (Channels::Mono, 1) => true,
                    (Channels::Mono, 2) => true,
                    (Channels::Stereo, 2) => true,
                    _ => false,
                };
                let sampling_depth_acceptable =
                    config.sample_format().sample_size() == spec.sampling_depth.bytes() as usize;

                channel_count_acceptable && sampling_depth_acceptable
            })
            .ok_or(())
            .map_err(|_| error!("couldn't find usable audio config"))?
            .config();
        let convert_mono_to_stereo =
            config.channels == 2 && matches!(spec.channels, Channels::Mono);
        AudioStreamRenderer::new(self.device.clone(), config, convert_mono_to_stereo)
    }
}

pub(crate) struct AudioStreamRenderer {
    device: cpal::Device,
    stream: cpal::Stream,
    buffer: Arc<Mutex<VecDeque<u8>>>,
}

impl AudioStreamRenderer {
    fn new(
        device: cpal::Device,
        config: cpal::StreamConfig,
        mono_to_stereo: bool,
    ) -> Result<Self, ()> {
        let buffer = Arc::new(Mutex::new(VecDeque::<u8>::with_capacity(10000)));
        let buffer_clone = buffer.clone();
        let mut first = AtomicBool::new(true);
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let sample_size = size_of::<i16>();
                    let first = first.get_mut();
                    if *first {
                        *first = false;
                        data.fill(0);
                        return;
                    }

                    let mut buffer: tokio::sync::MutexGuard<'_, VecDeque<u8>> =
                        buffer_clone.blocking_lock();

                    if mono_to_stereo {
                        let count_stereo_samples = min(data.len() / 2, buffer.len() / (2 * sample_size));
                        let data_raw_slice = unsafe {
                            std::slice::from_raw_parts_mut(
                                data.as_ptr() as *mut u8,
                                count_stereo_samples * sample_size,
                            )
                        };
                        buffer.read_exact(data_raw_slice).unwrap();

                        let mut write_pos = count_stereo_samples * 2 - 2;
                        let mut read_pos = count_stereo_samples - 1;

                        while read_pos > 0 {
                            let sample = data[read_pos];
                            data[write_pos] = sample;
                            data[write_pos + 1] = sample;
                            read_pos -= 1;
                            write_pos -= 2;
                        }

                        data[(count_stereo_samples * 2)..].fill(0);
                    } else {
                        let count_samples = min(data.len(), buffer.len() / sample_size);
                        let data_raw_slice = unsafe {
                            std::slice::from_raw_parts_mut(
                                data.as_ptr() as *mut u8,
                                count_samples * sample_size,
                            )
                        };
                        buffer.read_exact(data_raw_slice).unwrap();
                        data[count_samples..].fill(0);
                    }
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
