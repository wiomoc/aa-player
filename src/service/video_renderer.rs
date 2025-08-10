use glib::{GStr, object::ObjectExt};
use gstreamer::{
    Caps,
    glib::object::Cast,
    prelude::{ElementExt, GstBinExtManual},
};
use gstreamer_app::AppSrc;
use log::{error, info};
use std::{thread};

use crate::{
    protos,
    service::{
        gst_input_event_tap::InputEventTap,
        input_service::InputEventReceiver,
        media_sink::{StreamRenderer, StreamRendererFactory},
    },
};

pub(crate) struct VideoStreamRendererFactory {}

impl VideoStreamRendererFactory {
    pub(crate) fn new() -> Self {
        Self {}
    }
}

#[derive(Clone)]
pub(crate) struct VideoSpec {
    pub(crate) resolution: protos::VideoCodecResolutionType,
    pub(crate) frame_rate: protos::VideoFrameRateType,
    pub(crate) dpi: u32,
    pub(crate) input_event_receiver: InputEventReceiver,
}

impl VideoSpec {
    fn size(&self) -> (u32, u32) {
        match self.resolution {
            protos::VideoCodecResolutionType::Video800x480 => (800, 400),
            protos::VideoCodecResolutionType::Video1280x720 => (1280, 720),
            protos::VideoCodecResolutionType::Video1920x1080 => (1920, 1080),
            protos::VideoCodecResolutionType::Video2560x1440 => (2560, 1440),
            protos::VideoCodecResolutionType::Video3840x2160 => (3840, 2160),
            protos::VideoCodecResolutionType::Video720x1280 => (720, 1280),
            protos::VideoCodecResolutionType::Video1080x1920 => (1080, 1920),
            protos::VideoCodecResolutionType::Video1440x2560 => (1440, 2560),
            protos::VideoCodecResolutionType::Video2160x3840 => (2160, 3840),
        }
    }

    fn fps(&self) -> u8 {
        match self.frame_rate {
            protos::VideoFrameRateType::VideoFps60 => 60,
            protos::VideoFrameRateType::VideoFps30 => 30,
        }
    }
}

impl StreamRendererFactory for VideoStreamRendererFactory {
    type Renderer = VideoStreamRenderer;
    type Spec = VideoSpec;

    fn get_descriptor(&self, spec: &Self::Spec) -> crate::protos::MediaSinkService {
        protos::MediaSinkService {
            available_type: Some(protos::MediaCodecType::MediaCodecVideoH264Bp as i32),
            video_configs: vec![protos::VideoConfiguration {
                codec_resolution: Some(spec.resolution as i32),
                frame_rate: Some(spec.frame_rate as i32),
                width_margin: Some(0),
                height_margin: Some(0),
                density: Some(spec.dpi),
                video_codec_type: Some(protos::MediaCodecType::MediaCodecVideoH264Bp as i32),
                ..Default::default()
            }],
            available_while_in_call: Some(true),
            ..Default::default()
        }
    }

    fn create(&self, spec: &Self::Spec) -> Result<Self::Renderer, ()> {
        Ok(VideoStreamRenderer::new(spec))
    }
}

pub(crate) struct VideoStreamRenderer {
    appsrc: AppSrc,
    pipeline: gstreamer::Pipeline,
}

impl VideoStreamRenderer {
    fn new(spec: &VideoSpec) -> Self {
        gstreamer::init().unwrap();

        let pipeline: gstreamer::Pipeline = gstreamer::Pipeline::default();

        let size = spec.size();
        let appsrc_caps = Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .field("width", size.0 as i32)
            .field("height", size.1 as i32)
            .field("profile", "main")
            //.field("framerate", gstreamer::Fraction::new(1, spec.fps() as i32))
            .build();

        let appsrc = gstreamer_app::AppSrc::builder()
            .caps(&appsrc_caps)
            .is_live(true)
            .stream_type(gstreamer_app::AppStreamType::Stream)
            .format(gstreamer::Format::Buffers)
            .build();

        let input_event_receiver = spec.input_event_receiver.clone();
        let event_interceptor = InputEventTap::new();
        event_interceptor.connect("input-event", false, move |args| {
            let event: gstreamer::Event = args[1].get().unwrap();
            if let Some(structure) = event.structure() {
                let event_name: &GStr = structure.get("event").unwrap();
                let action = match event_name.as_str() {
                    "mouse-move" => protos::PointerAction::ActionMoved,
                    "mouse-button-release" => protos::PointerAction::ActionUp,
                    "mouse-button-press" => protos::PointerAction::ActionDown,
                    _ => return None,
                };
                let pointer_x = structure.get::<'_, f64>("pointer_x").unwrap() as u32;
                let pointer_y = structure.get::<'_, f64>("pointer_y").unwrap() as u32;
                input_event_receiver.report_touch_event(action, (pointer_x, pointer_y));
            }
            None
        });

        let d3d11h264dec = gstreamer::ElementFactory::make("d3d11h264dec")
            .build()
            .unwrap();
        let d3dvideosink = gstreamer::ElementFactory::make("d3d11videosink")
            .build()
            .unwrap();

        pipeline
            .add_many([
                appsrc.upcast_ref(),
                event_interceptor.upcast_ref(),
                &d3d11h264dec,
                &d3dvideosink,
            ])
            .unwrap();

        gstreamer::Element::link_many([
            appsrc.upcast_ref(),
            event_interceptor.upcast_ref(),
            &d3d11h264dec,
            &d3dvideosink,
        ])
        .unwrap();
        Self {
            pipeline,
            appsrc,
        }
    }
}

impl StreamRenderer for VideoStreamRenderer {
    fn start(&mut self) {
        self.pipeline.set_state(gstreamer::State::Playing).unwrap();
        let pipeline_clone = self.pipeline.clone();
        thread::spawn(move || {
            let bus = pipeline_clone
                .bus()
                .expect("Pipeline without bus. Shouldn't happen!");

            for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
                use gstreamer::MessageView;

                match msg.view() {
                    MessageView::Eos(..) => break,
                    MessageView::Error(err) => {
                        error!("gst {err:?}");
                        pipeline_clone.set_state(gstreamer::State::Null).unwrap();
                    }
                    _ => (),
                }
            }
        });
        info!("starting video streaming");
    }

    fn stop(&mut self) {
        todo!()
    }

    async fn add_content(
        &mut self,
        content: &[u8],
        timestamp: Option<super::media_sink::TimestampMicros>,
    ) {
        let mut buffer = gstreamer::Buffer::with_size(content.len()).unwrap();
        //buffer
        //    .get_mut()
        //    .unwrap()
        //    .set_dts(timestamp.map(|timestamp| ClockTime::from_useconds(timestamp)));
        buffer
            .get_mut()
            .unwrap()
            .copy_from_slice(0, content)
            .unwrap();
        self.appsrc.push_buffer(buffer).unwrap();
    }

    fn handle_video_focus_notification_request(
        &mut self,
        _request: protos::VideoFocusRequestNotification,
    ) -> Result<protos::VideoFocusNotification, ()> {
        Ok(protos::VideoFocusNotification {
            focus: Some(protos::VideoFocusMode::VideoFocusProjected as i32),
            ..Default::default()
        })
    }
}
