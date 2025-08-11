use tokio::sync::mpsc;

use crate::{packet::Packet, packet_router::ChannelPacketSender, protos};
pub(crate) mod audio_renderer;
pub(crate) mod video_renderer;
pub(crate) mod media_sink;
pub(crate) mod input_source;
pub(crate) mod audio_source;

mod gst_input_event_tap;

pub(crate) trait Service {
    fn get_id(&self) -> i32 {
        self.get_descriptor().id
    }

    fn get_descriptor(&self) -> protos::Service;

    fn instanciate(
        &self,
        packet_sender: ChannelPacketSender,
        packet_receiver: mpsc::Receiver<Packet>,
    );
}
