pub (crate) mod imp;

glib::wrapper! {
    /// See [`imp::InputEventTap`].
    pub struct InputEventTap(ObjectSubclass<imp::InputEventTap>)
         @extends gstreamer::Element, gstreamer::Object;
}



impl Default for InputEventTap {
    fn default() -> Self {
        Self::new()
    }
}

impl InputEventTap {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }
}