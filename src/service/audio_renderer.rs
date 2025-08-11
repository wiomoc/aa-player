use std::{
    cmp::min,
    collections::VecDeque,
    io::Read,
    sync::{Arc, atomic::AtomicBool},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{error, info, warn};
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
        let (config, resplicate_sample_count) = self
            .device
            .supported_output_configs()
            .unwrap()
            .filter_map(|config| {
                for replicate_sample_count in 1..=4 {
                    if let Some(config) = config.try_with_sample_rate(cpal::SampleRate(
                        spec.sampling_rate * replicate_sample_count,
                    )) {
                        return Some((config, replicate_sample_count));
                    }
                }
                return None;
            })
            .filter_map(
                |(config, replicate_sample_count): (cpal::SupportedStreamConfig, u32)| {
                    let total_resample_count = match (spec.channels, config.channels()) {
                        (Channels::Mono, 1) => replicate_sample_count,
                        (Channels::Mono, 2) => 2 * replicate_sample_count,
                        (Channels::Stereo, 2) if replicate_sample_count == 1 => replicate_sample_count,
                        _ => return None,
                    };
                    let sampling_depth_acceptable = config.sample_format().sample_size()
                        == spec.sampling_depth.bytes() as usize;

                    if sampling_depth_acceptable {
                        return Some((config, total_resample_count));
                    } else {
                        None
                    }
                },
            )
            .min_by_key(
                |(_config, replicate_sample_count): &(cpal::SupportedStreamConfig, u32)| {
                    *replicate_sample_count
                },
            )
            .ok_or(())
            .map_err(|_| error!("couldn't find usable audio config"))?;

        AudioStreamRenderer::new(
            self.device.clone(),
            config.config(),
            resplicate_sample_count,
        )
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
        replicate_sample_count: u32,
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

                    let replicate_sample_count = replicate_sample_count as usize;
                    let sample_count = min(
                        data.len() / replicate_sample_count,
                        buffer.len() / (sample_size * replicate_sample_count),
                    );
                    let data_raw_slice = unsafe {
                        std::slice::from_raw_parts_mut(
                            data.as_ptr() as *mut u8,
                            sample_count * sample_size,
                        )
                    };

                    buffer.read_exact(data_raw_slice).unwrap();

                    let total_sample_count = sample_count * replicate_sample_count;
                    if replicate_sample_count > 1 {
                        let mut write_pos = total_sample_count - replicate_sample_count;
                        let mut read_pos = sample_count - 1;

                        while read_pos > 0 {
                            let sample = data[read_pos];
                            for i in 0..replicate_sample_count {
                                data[write_pos + i] = sample;
                            }
                            read_pos -= 1;
                            write_pos -= replicate_sample_count;
                        }
                    }

                    data[total_sample_count..].fill(0);
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
