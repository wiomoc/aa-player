use tokio::sync::mpsc;

use crate::{packet::Packet, packet_router::ChannelPacketSender, protos};

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
