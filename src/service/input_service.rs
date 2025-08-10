use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use log::debug;
use prost::Message;
use tokio::sync::mpsc::error::SendError;

use crate::{
    packet::Packet,
    packet_router::{ChannelPacketSender, WeakChannelPacketSender},
    protos::{self, PointerAction},
    service::Service,
};

#[derive(Clone)]
pub(crate) struct InputEventReceiver {
    inner: Arc<Mutex<Option<InputEventReceiverInner>>>,
}

#[derive(Clone)]
pub(crate) struct InputEventReceiverInner {
    packet_sender: WeakChannelPacketSender,
    last_input_move_event: Option<(SystemTime, (u32, u32))>,
    pointer_down: bool,
}

impl InputEventReceiverInner {
    fn new(packet_sender: WeakChannelPacketSender) -> Self {
        Self {
            packet_sender,
            last_input_move_event: None,
            pointer_down: false,
        }
    }

    fn calc_distance_squared(a: (u32, u32), b: (u32, u32)) -> u32 {
        let x_diff = a.0.abs_diff(b.0);
        let y_diff = a.1.abs_diff(b.1);
        x_diff * x_diff + y_diff * y_diff
    }

    fn report_touch_event(
        &mut self,
        action: protos::PointerAction,
        pointer_position: (u32, u32),
    ) -> Result<Option<()>, SendError<Packet>> {
        let now = SystemTime::now();
        match action {
            PointerAction::ActionDown | PointerAction::ActionPointerDown => {
                self.pointer_down = true;
            },
            PointerAction::ActionUp | PointerAction::ActionPointerUp => {
                self.pointer_down = false;
            },
            PointerAction::ActionMoved => {
                if !self.pointer_down {
                    return Ok(Some(()));
                }
                let debounce_duration = Duration::from_millis(30);
                if let Some((last_timestamp, last_pointer_position)) = self.last_input_move_event
                    && now.duration_since(last_timestamp).unwrap_or(Duration::ZERO)
                        < debounce_duration
                    && Self::calc_distance_squared(pointer_position, last_pointer_position) < (15 * 15)
                {
                    // ignore event
                    return Ok(Some(()));
                } else {
                    self.last_input_move_event = Some((now, pointer_position))
                }
            }
        }

        self.packet_sender.blocking_send_proto(
            InputService::MESSAGE_ID_INPUT_REPORT,
            &protos::InputReport {
                timestamp: now
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as u64,
                touch_event: Some(protos::TouchEvent {
                    pointer_data: vec![protos::touch_event::Pointer {
                        x: pointer_position.0,
                        y: pointer_position.1,
                        pointer_id: 0,
                    }],
                    action: Some(action as i32),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
    }
}

impl InputEventReceiver {
    fn register_packet_sender(&self, packet_sender: WeakChannelPacketSender) {
        *self.inner.lock().unwrap() = Some(InputEventReceiverInner::new(packet_sender));
    }

    fn unregister_packet_sender(&self) {
        self.inner.lock().unwrap().take();
    }

    pub(crate) fn report_touch_event(
        &self,
        action: protos::PointerAction,
        pointer_position: (u32, u32),
    ) {
        let mut inner_locked = self.inner.lock().unwrap();
        if let Some(inner) = inner_locked.as_mut() {
            let result = inner.report_touch_event(action, pointer_position);
            if result.ok().flatten().is_none() {
                inner_locked.take();
            }
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }
}
pub(crate) struct InputService {
    service_id: u8,
    input_event_receiver: InputEventReceiver,
    display_size: (u32, u32)
}

impl InputService {
    const MESSAGE_ID_INPUT_REPORT: u16 = 0x8001;
    const MESSAGE_ID_KEY_BINDING_REQUEST: u16 = 0x8002;
    const MESSAGE_ID_KEY_BINDING_RESPONSE: u16 = 0x8003;
    const MESSAGE_ID_INPUT_FEEDBACK: u16 = 0x8004;

    pub(crate) fn new(service_id: u8, input_event_receiver: InputEventReceiver, display_size: (u32, u32)) -> Self {
        Self {
            service_id,
            input_event_receiver,
            display_size
        }
    }
}

impl Service for InputService {
    fn get_id(&self) -> i32 {
        self.service_id as i32
    }

    fn get_descriptor(&self) -> protos::Service {
        protos::Service {
            id: self.get_id(),
            input_source_service: Some(protos::InputSourceService {
                touchscreen: vec![protos::input_source_service::TouchScreen {
                    width: self.display_size.0 as i32,
                    height: self.display_size.1 as i32,
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
        self.input_event_receiver
            .register_packet_sender(packet_sender.downgrade());
        let input_event_receiver = self.input_event_receiver.clone();
        tokio::spawn(async move {
            while let Some(packet) = packet_receiver.recv().await {
                match packet.message_id() {
                    Self::MESSAGE_ID_KEY_BINDING_REQUEST => {
                        debug!(
                            "service received packet {:?}",
                            protos::KeyBindingRequest::decode(packet.payload()).unwrap()
                        );
                        packet_sender
                            .send_proto(
                                Self::MESSAGE_ID_KEY_BINDING_RESPONSE,
                                &protos::KeyBindingResponse { status: 0 },
                            )
                            .await
                            .unwrap();
                    }
                    message_id => debug!("service received packet {message_id}"),
                }
            }
            input_event_receiver.unregister_packet_sender();
        });
    }
}
