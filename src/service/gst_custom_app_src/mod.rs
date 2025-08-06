use glib::{types::StaticType, Object};

pub (crate) mod imp;

glib::wrapper! {
    pub struct CustomAppSrc(ObjectSubclass<imp::CustomAppSrc>)
         @extends gstreamer::Element, gstreamer::Object;
}



impl CustomAppSrc {
    pub fn new() -> Self {
        Object::builder().build()
    }
}