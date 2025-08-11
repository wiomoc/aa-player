use std::sync::{LazyLock};

use glib::object::ObjectExt;
use glib::subclass::object::ObjectImplExt;
use glib::subclass::types::ObjectSubclassExt;
use glib::subclass::{Signal, SignalType};
use glib::subclass::{object::ObjectImpl, types::ObjectSubclass};
use glib::translate::FromGlib;
use gstreamer::prelude::PadExt;
use gstreamer::prelude::PadExtManual;
use gstreamer::prelude::{ElementClassExt, ElementExt};
use gstreamer::subclass::prelude::ElementImpl;
use gstreamer::subclass::prelude::ElementImplExt;
use gstreamer::subclass::prelude::GstObjectImpl;
use log::info;


pub struct InputEventTap {
    srcpad: gstreamer::Pad,
    sinkpad: gstreamer::Pad,
}

impl InputEventTap {

    fn sink_chain(
        &self,
        _pad: &gstreamer::Pad,
        buffer: gstreamer::Buffer,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.srcpad.push(buffer)
    }

    fn sink_event(&self, _pad: &gstreamer::Pad, event: gstreamer::Event) -> bool {
        self.srcpad.push_event(event)
    }

    fn sink_query(&self, _pad: &gstreamer::Pad, query: &mut gstreamer::QueryRef) -> bool {
        self.srcpad.peer_query(query)
    }

    fn src_event(&self, _pad: &gstreamer::Pad, event: gstreamer::Event) -> bool {
        if let Some(structure) = event.structure()
            && structure.name().as_str() == "application/x-gst-navigation"
        {
            self.obj().emit_by_name::<()>("input-event", &[&event]);
            true
        } else {
            self.sinkpad.push_event(event)
        }
    }

    fn src_query(&self, _pad: &gstreamer::Pad, query: &mut gstreamer::QueryRef) -> bool {
        self.sinkpad.peer_query(query)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for InputEventTap {
    const NAME: &'static str = "InputEventTap";
    type Type = super::InputEventTap;
    type ParentType = gstreamer::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gstreamer::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                InputEventTap::catch_panic_pad_function(
                    parent,
                    || Err(gstreamer::FlowError::Error),
                    |identity| identity.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                InputEventTap::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                InputEventTap::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.sink_query(pad, query),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gstreamer::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                InputEventTap::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.src_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                InputEventTap::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.src_query(pad, query),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
        }
    }
}

impl ElementImpl for InputEventTap {
    fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
            LazyLock::new(|| {
                gstreamer::subclass::ElementMetadata::new(
                    "InputEventTap",
                    "Events",
                    "Provides Navigation Events via Glib Signals",
                    "Christoph Walcher",
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

impl GstObjectImpl for InputEventTap {}
impl ObjectImpl for InputEventTap {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: LazyLock<Vec<Signal>> = LazyLock::new(|| {
            vec![
                Signal::builder("input-event")
                    .param_types([unsafe {
                        SignalType::from_glib(gstreamer::ffi::gst_event_get_type())
                    }])
                    .run_last()
                    .build(),
            ]
        });
        SIGNALS.as_ref()
    }
}
