use tokio::sync::mpsc;

use crate::{packet::Packet, packet_router::ChannelPacketSender, protos};
pub(crate) mod audio_renderer;
pub(crate) mod video_renderer;
pub(crate) mod media_sink;
pub(crate) mod input_source;
pub(crate) mod audio_source;

mod gst_input_event_tap;

/// A service advertised to the phone during service discovery. Once the phone
/// opens a channel for it, `instanciate` is called to handle that channel.
pub(crate) trait Service {
    fn get_id(&self) -> i32 {
        self.get_descriptor().id
    }

    fn get_descriptor(&self) -> protos::Service;

    /// Starts handling a newly opened channel. Implementations typically spawn a
    /// task that runs until `packet_receiver` is closed.
    fn instanciate(
        &self,
        packet_sender: ChannelPacketSender,
        packet_receiver: mpsc::Receiver<Packet>,
    );
}
