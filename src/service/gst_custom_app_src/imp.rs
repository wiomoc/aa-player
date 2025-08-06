use std::default;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use glib::GStr;
use glib::subclass::object::ObjectImplExt;
use glib::subclass::types::ObjectSubclassExt;
use glib::subclass::{object::ObjectImpl, types::ObjectSubclass};
use gstreamer::prelude::PadExt;
use gstreamer::prelude::PadExtManual;
use gstreamer::prelude::{ElementClassExt, ElementExt};
use gstreamer::subclass::prelude::ElementImpl;
use gstreamer::subclass::prelude::ElementImplExt;
use gstreamer::subclass::prelude::GstObjectImpl;
use gstreamer_base::subclass::prelude::BaseTransformImpl;
use gstreamer_video::subclass::prelude::VideoFilterImpl;
use log::info;

use crate::packet_router::ChannelPacketSender;
use crate::protos;
use crate::service::video_renderer::PACKET_SENDER;

pub struct CustomAppSrc {
    srcpad: gstreamer::Pad,
    sinkpad: gstreamer::Pad,
    packet_sender: Option<Arc<Mutex<Option<ChannelPacketSender>>>>,
}

impl CustomAppSrc {
    fn set_sender(&mut self, packet_sender: Arc<Mutex<Option<ChannelPacketSender>>>) {
        self.packet_sender = Some(packet_sender);
    }

    fn sink_chain(
        &self,
        pad: &gstreamer::Pad,
        buffer: gstreamer::Buffer,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.srcpad.push(buffer)
    }

    fn sink_event(&self, pad: &gstreamer::Pad, event: gstreamer::Event) -> bool {
        self.srcpad.push_event(event)
    }

    fn sink_query(&self, pad: &gstreamer::Pad, query: &mut gstreamer::QueryRef) -> bool {
        self.srcpad.peer_query(query)
    }

    fn src_event(&self, pad: &gstreamer::Pad, event: gstreamer::Event) -> bool {
        if let Some(structure) = event.structure()
            && structure.name().as_str() == "application/x-gst-navigation"
        {
            let event_name: &GStr = structure.get("event").unwrap();
            let action = match event_name.as_str() {
                "mouse-move" => protos::PointerAction::ActionMoved,
                "mouse-button-release" => protos::PointerAction::ActionUp,
                "mouse-button-press" => protos::PointerAction::ActionDown,
                _ => return false,
            };
            let pointer = protos::touch_event::Pointer {
                x: structure.get::<'_, f64>("pointer_x").unwrap() as u32,
                y: structure.get::<'_, f64>("pointer_y").unwrap() as u32,
                pointer_id: 0,
            };
            let aa_event = protos::TouchEvent {
                pointer_data: vec![pointer],
                action_index: None,
                action: Some(action as i32),
            };
            info!("Handling event {:?}", &aa_event);

            if let Some(sender) = PACKET_SENDER.blocking_lock().as_ref() {
                sender.blocking_send_proto(
                    0x8001,
                    &protos::InputReport {
                        timestamp: SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_micros() as u64,
                        touch_event: Some(aa_event),
                        ..Default::default()
                    },
                ).unwrap();
            }
            true
        } else {
            self.sinkpad.push_event(event)
        }
    }

    fn src_query(&self, pad: &gstreamer::Pad, query: &mut gstreamer::QueryRef) -> bool {
        self.sinkpad.peer_query(query)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for CustomAppSrc {
    const NAME: &'static str = "CustomAppSrc";
    type Type = super::CustomAppSrc;
    type ParentType = gstreamer::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gstreamer::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                CustomAppSrc::catch_panic_pad_function(
                    parent,
                    || Err(gstreamer::FlowError::Error),
                    |identity| identity.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                CustomAppSrc::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                CustomAppSrc::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.sink_query(pad, query),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gstreamer::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                CustomAppSrc::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.src_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                CustomAppSrc::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.src_query(pad, query),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            packet_sender: None,
        }
    }
}

impl ElementImpl for CustomAppSrc {
    fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
            LazyLock::new(|| {
                gstreamer::subclass::ElementMetadata::new(
                    "Identity",
                    "Generic",
                    "Does nothing with the data",
                    "Sebastian Dröge <sebastian@centricular.com>",
                )
            });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gstreamer::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
            // Our element can accept any possible caps on both pads
            let caps = gstreamer::Caps::new_any();
            let src_pad_template = gstreamer::PadTemplate::new(
                "src",
                gstreamer::PadDirection::Src,
                gstreamer::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gstreamer::PadTemplate::new(
                "sink",
                gstreamer::PadDirection::Sink,
                gstreamer::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gstreamer::StateChange,
    ) -> Result<gstreamer::StateChangeSuccess, gstreamer::StateChangeError> {
        self.parent_change_state(transition)
    }
}
impl GstObjectImpl for CustomAppSrc {}
impl ObjectImpl for CustomAppSrc {
    fn constructed(&self) {
        // Call the parent class' ::constructed() implementation first
        self.parent_constructed();

        // Here we actually add the pads we created in Identity::new() to the
        // element so that GStreamer is aware of their existence.
        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}
