use gstreamer::{
    Caps, ClockTime,
    glib::object::Cast,
    prelude::{ElementExt, GstBinExtManual},
};
use gstreamer_app::AppSrc;
use lazy_static::lazy_static;
use log::{debug, error, info};
use prost::Message;
use std::{fs::File, io::Write, sync::Arc, thread};
use tokio::sync::Mutex;

use crate::{
    packet_router::ChannelPacketSender,
    protos,
    service::{
        Service,
        gst_custom_app_src::CustomAppSrc,
        media_sink::{StreamRenderer, StreamRendererFactory},
    },
};

pub(crate) struct VideoStreamRendererFactory {}

impl VideoStreamRendererFactory {
    pub(crate) fn new() -> Self {
        Self {}
    }
}

impl StreamRendererFactory for VideoStreamRendererFactory {
    type Renderer = VideoStreamRenderer;
    type Spec = ();

    fn get_descriptor(&self, spec: &Self::Spec) -> crate::protos::MediaSinkService {
        return protos::MediaSinkService {
            available_type: Some(protos::MediaCodecType::MediaCodecVideoH264Bp as i32),
            video_configs: vec![protos::VideoConfiguration {
                codec_resolution: Some(protos::VideoCodecResolutionType::Video1280x720 as i32),
                frame_rate: Some(protos::VideoFrameRateType::VideoFps60 as i32),
                width_margin: Some(0),
                height_margin: Some(0),
                density: Some(300),
                video_codec_type: Some(protos::MediaCodecType::MediaCodecVideoH264Bp as i32),
                ..Default::default()
            }],
            available_while_in_call: Some(true),
            ..Default::default()
        };
    }

    fn create(&self, spec: &Self::Spec) -> Result<Self::Renderer, ()> {
        Ok(VideoStreamRenderer::new())
    }
}

lazy_static! {
    pub(crate) static ref PACKET_SENDER: Arc<Mutex<Option<ChannelPacketSender>>> =
        Arc::new(Mutex::new(None));
}
pub(crate) struct InputService {}

impl InputService {
    pub(crate) fn new() -> Self {
        Self {}
    }
}

impl Service for InputService {
    fn get_descriptor(&self) -> protos::Service {
        protos::Service {
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
        }
    }

    fn instanciate(
        &self,
        packet_sender: ChannelPacketSender,
        mut packet_receiver: tokio::sync::mpsc::Receiver<crate::packet::Packet>,
    ) {
        tokio::spawn(async move {
            {
                *PACKET_SENDER.lock().await = Some(packet_sender.clone());
            }
            while let Some(packet) = packet_receiver.recv().await {
                match packet.message_id() {
                    0x8002 => {
                        debug!(
                            "service received packet {:?}",
                            protos::KeyBindingRequest::decode(packet.payload()).unwrap()
                        );
                        packet_sender
                            .send_proto(0x8003, &protos::KeyBindingResponse { status: 0 })
                            .await
                            .unwrap();
                    }
                    message_id => debug!("service received packet {message_id}"),
                }
            }
        });
    }
}

pub(crate) struct VideoStreamRenderer {
    file: File,
    appsrc: AppSrc,
    pipeline: gstreamer::Pipeline,
}

impl VideoStreamRenderer {
    fn new() -> Self {
        gstreamer::init().unwrap();

        let pipeline: gstreamer::Pipeline = gstreamer::Pipeline::default();

        let appsrc_caps = Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .field("height", 720)
            .field("width", 1280)
            .field("profile", "main")
            .build();
        //let structure = appsrc_caps.get_mut().unwrap().structure_mut(0).unwrap();
        //structure.set("stream-format", "byte-stream");
        //structure.set("alignment", "au");
        //info!("struct {:?}", &structure);

        let appsrc = gstreamer_app::AppSrc::builder()
            .caps(&appsrc_caps)
            .is_live(true)
            .stream_type(gstreamer_app::AppStreamType::Stream)
            .format(gstreamer::Format::Buffers)
            .build();

        let h264parse = gstreamer::ElementFactory::make("h264parse")
            .build()
            .unwrap();

        let event_interceptor = CustomAppSrc::new();

        let d3d11h264dec = gstreamer::ElementFactory::make("d3d11h264dec")
            .build()
            .unwrap();
        let d3dvideosink = gstreamer::ElementFactory::make("d3d11videosink")
            .build()
            .unwrap();

        //d3dvideosink.connect_closure(
        //    "mouse-event",
        //    false,
        //    glib::RustClosure::new(|values| {
        //        info!("moved {:?}", values);
        //        None
        //    }),
        //);
        pipeline
            .add_many([
                appsrc.upcast_ref(),
                event_interceptor.upcast_ref(),
                // &h264parse,
                &d3d11h264dec,
                &d3dvideosink,
            ])
            .unwrap();

        gstreamer::Element::link_many([
            appsrc.upcast_ref(),
            event_interceptor.upcast_ref(),
            //&h264parse,
            &d3d11h264dec,
            &d3dvideosink,
        ])
        .unwrap();
        Self {
            file: File::create("stream.h264").unwrap(),
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
        self.file.write_all(content).unwrap();

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
        request: protos::VideoFocusRequestNotification,
    ) -> Result<protos::VideoFocusNotification, ()> {
        return Ok(protos::VideoFocusNotification {
            focus: Some(protos::VideoFocusMode::VideoFocusProjected as i32),
            ..Default::default()
        });
    }
}
