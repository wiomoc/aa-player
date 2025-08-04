use std::sync::Arc;

use byteorder::{BigEndian, ByteOrder};
use log::{error, warn};
use prost::Message;

use crate::{packet, packet_router, protos, service::Service};

pub(crate) type TimestampMicros = u64;
pub(crate) trait StreamRendererFactory {
    type Renderer: StreamRenderer + Send;
    type Spec: Clone + Send;

    fn get_descriptor(&self, spec: &Self::Spec) -> protos::MediaSinkService;

    fn create(&self, spec: &Self::Spec) -> Result<Self::Renderer, ()>;
}
pub(crate) trait StreamRenderer {
    fn start(&mut self);
    fn stop(&mut self);
    fn add_content(
        &mut self,
        content: &[u8],
        timestamp: Option<TimestampMicros>,
    ) -> impl Future<Output = ()> + Send;
}

pub(crate) struct MediaService<P: StreamRendererFactory + Send + Sync> {
    service_id: u8,
    spec: P::Spec,
    stream_processor_factory: Arc<P>,
}

impl<P: StreamRendererFactory + Send + Sync> MediaService<P> {
    const MESSAGE_ID_TIMESTAMPED_CONTENT: u16 = 0x0000;
    const MESSAGE_ID_CONTENT: u16 = 0x0001;
    const MESSAGE_ID_SETUP: u16 = 0x8000;
    const MESSAGE_ID_START: u16 = 0x8001;
    const MESSAGE_ID_STOP: u16 = 0x8002;
    const MESSAGE_ID_CONFIG: u16 = 0x8003;
    const MESSAGE_ID_ACK: u16 = 0x8004;

    const MAX_UNACKED_CONTENT_MESSAGES: usize = 20;
    const SEND_ACK_AFTER_CONTENT_MESSAGES: usize = Self::MAX_UNACKED_CONTENT_MESSAGES - 5;

    pub(crate) fn new(service_id: u8, stream_processor_factory: Arc<P>, spec: P::Spec) -> Self {
        Self {
            service_id,
            stream_processor_factory,
            spec,
        }
    }

    async fn handle_packets(
        packet_sender: &packet_router::ChannelPacketSender,
        mut packet_receiver: tokio::sync::mpsc::Receiver<packet::Packet>,
        renderer_factory: Arc<P>,
        spec: P::Spec,
    ) -> Result<(), ()> {
        let mut renderer: Option<P::Renderer> = None;
        let mut session_id: Option<i32> = None;
        let mut unacked_content_messages = Vec::new();
        while let Some(packet) = packet_receiver.recv().await {
            match packet.message_id() {
                Self::MESSAGE_ID_TIMESTAMPED_CONTENT => {
                    let session_id = session_id.ok_or(()).map_err(|_| warn!("Not started yet"))?;
                    let renderer = renderer
                        .as_mut()
                        .ok_or(())
                        .map_err(|_| warn!("Not setuped yet"))?;
                    let timestamp_micros = BigEndian::read_u64(packet.payload());
                    let content = &packet.payload()[8..];
                    renderer.add_content(content, Some(timestamp_micros)).await;
                    unacked_content_messages.push(timestamp_micros);
                    if unacked_content_messages.len() >= Self::MAX_UNACKED_CONTENT_MESSAGES - 1 {
                        packet_sender
                            .send_proto(
                                Self::MESSAGE_ID_ACK,
                                &protos::Ack {
                                    ack: Some(1),
                                    receive_timestamp_ns: unacked_content_messages,
                                    session_id,
                                },
                            )
                            .await
                            .unwrap();
                        unacked_content_messages = Vec::new();
                    }
                }
                Self::MESSAGE_ID_SETUP => {
                    let config = protos::Setup::decode(packet.payload())
                        .map_err(|err| warn!("could not decode setup message {err:?}"))?;
                    assert_eq!(
                        config.r#type(),
                        renderer_factory.get_descriptor(&spec).available_type()
                    );
                    renderer = renderer_factory.create(&spec).ok();
                    packet_sender
                        .send_proto(
                            Self::MESSAGE_ID_CONFIG,
                            &protos::Config {
                                max_unacked: Some(Self::MAX_UNACKED_CONTENT_MESSAGES as u32),
                                configuration_indices: vec![0],

                                status: if renderer.is_some() {
                                    protos::config::Status::Ready
                                } else {
                                    protos::config::Status::Wait
                                } as i32,
                            },
                        )
                        .await
                        .unwrap();
                }
                Self::MESSAGE_ID_START => {
                    let renderer = renderer
                        .as_mut()
                        .ok_or(())
                        .map_err(|_| warn!("Not setuped yet"))?;
                    renderer.start();
                    let start_config = protos::Start::decode(packet.payload())
                        .map_err(|err| warn!("could not decode start message {err:?}"))?;

                    session_id = Some(start_config.session_id);
                }
                Self::MESSAGE_ID_STOP => {
                    let renderer = renderer
                        .as_mut()
                        .ok_or(())
                        .map_err(|_| warn!("Not setuped yet"))?;
                    renderer.stop();
                }
                _ => warn!("Received packet with unknown message id {packet:?}"),
            }
        }
        Ok(())
    }
}

impl<P: StreamRendererFactory + Send + Sync + 'static> Service for MediaService<P> {
    fn get_id(&self) -> i32 {
        self.service_id as i32
    }

    fn get_descriptor(&self) -> protos::Service {
        protos::Service {
            id: self.get_id(),
            media_sink_service: Some(self.stream_processor_factory.get_descriptor(&self.spec)),
            ..Default::default()
        }
    }

    fn instanciate(
        &self,
        packet_sender: packet_router::ChannelPacketSender,
        packet_receiver: tokio::sync::mpsc::Receiver<packet::Packet>,
    ) {
        let renderer_factory = self.stream_processor_factory.clone();
        let spec = self.spec.clone();
        tokio::spawn(async move {
            if Self::handle_packets(&packet_sender, packet_receiver, renderer_factory, spec)
                .await
                .is_err()
            {
                error!("Media Service stopped due to error");
            }
        });
    }
}
