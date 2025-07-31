use log::info;
use simple_logger::SimpleLogger;
use tokio::sync::mpsc::{self};

use crate::{
    packet_router::PacketRouter, usb::UsbManager
};

mod encryption;
mod frame;
mod usb;
mod packet;
mod packet_types;
mod packet_router;

mod protos {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

#[tokio::main]
async fn main() -> Result<(), ()> {
    SimpleLogger::new().init().unwrap();
    info!("starting");
    let usb_manager = UsbManager::start();

    let (outgoing_packet_queue_sender, outgoing_packet_queue_receiver) = mpsc::channel(2);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        drop(outgoing_packet_queue_sender);
    });
    PacketRouter::start(usb_manager, outgoing_packet_queue_receiver).await;
    Ok(())
}