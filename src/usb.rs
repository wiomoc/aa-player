use std::{
    ffi::CString,
    io::{Error, ErrorKind},
    sync::Arc,
    time::Duration,
};

use futures::stream::StreamExt;
use log::{error, info, trace, warn};
use nusb::{
    DeviceId, DeviceInfo, Interface,
    hotplug::HotplugEvent,
    transfer::{
        ControlIn, ControlOut, ControlType, Direction, EndpointType, Recipient, RequestBuffer,
    },
};
use tokio::{
    select, sync::{mpsc, Mutex}, task::JoinHandle, time
};
use tokio_util::sync::CancellationToken;

use crate::frame::{AAPFrame, AAPFrameCodec, AsyncReader, AsyncWriter, FrameDecoder, FrameEncoder};

pub struct UsbManager {
    incoming_queue_receiver: mpsc::Receiver<IncomingEvent>,
    outgoing_queue_sender: mpsc::Sender<OutgoingEvent>,
    join_handle: JoinHandle<()>,
    cancel_token: CancellationToken
}

struct ConnectedDeviceState {
    connected_device_id: DeviceId,
    terminate_connection_token: CancellationToken,
}

struct UsbInternalState {
    incoming_queue_sender: mpsc::Sender<IncomingEvent>,
    outgoing_queue_receiver: Arc<Mutex<mpsc::Receiver<OutgoingEvent>>>,
    connected_device: Option<ConnectedDeviceState>,
    rx_loop_join_handle: Option<JoinHandle<()>>,
    tx_loop_join_handle: Option<JoinHandle<()>>
}

pub(crate) enum IncomingEvent {
    Connected,
    Closed,
    Frame(AAPFrame),
}

pub(crate) enum OutgoingEvent {
    Close,
    Frame(AAPFrame),
}

//impl<C: FrameDecoder<UsbReader> + FrameEncoder<UsbWriter>> UsbManager {
impl UsbManager {
    pub fn start() -> Self {
        let cancel_token =  CancellationToken::new();
        let (incoming_queue_sender, incoming_queue_receiver) = mpsc::channel::<IncomingEvent>(8);
        let (outgoing_queue_sender, outgoing_queue_receiver) = mpsc::channel::<OutgoingEvent>(8);

        let state = UsbInternalState {
            incoming_queue_sender,
            outgoing_queue_receiver: Arc::new(Mutex::new(outgoing_queue_receiver)),
            connected_device: None,
            rx_loop_join_handle: None,
            tx_loop_join_handle: None
        };

        let join_handle = tokio::spawn(state.start_device_handle_loop(cancel_token.clone()));

        

        UsbManager {
            incoming_queue_receiver,
            outgoing_queue_sender,
            cancel_token,
            join_handle
        }
    }

    pub async fn send_frame(
        &self,
        frame: AAPFrame,
    ) -> Result<(), mpsc::error::SendError<OutgoingEvent>> {
        self.outgoing_queue_sender
            .send(OutgoingEvent::Frame(frame))
            .await
    }

    pub async fn send_connection_close(&self) -> Result<(), mpsc::error::SendError<OutgoingEvent>> {
        self.outgoing_queue_sender.send(OutgoingEvent::Close).await
    }

    pub async fn recv_frame(&mut self) -> IncomingEvent {
        self.incoming_queue_receiver
            .recv()
            .await
            .unwrap_or(IncomingEvent::Closed)
    }

    pub async fn close(self) {
        self.cancel_token.cancel();
        self.join_handle.await.unwrap();
    }
}

impl UsbInternalState {
    const AOA_VID: u16 = 0x18D1;
    const AOA_PID_V1: u16 = 0x2D00;
    const AOA_PID_V2: u16 = 0x2D01;

    async fn start_device_handle_loop(mut self, cancel_token: CancellationToken) {
        let mut watch = nusb::watch_devices().expect("can't start watching devices");
        let initial_devices = nusb::list_devices().expect("can't list devices");
        for device in initial_devices {
            self.handle_connected_device(device).await;
        }

        loop {
            let event = select! {
                event = watch.next() => event,
                _ = cancel_token.cancelled() => {
                    break;
                }
            };

            match event {
                Some(HotplugEvent::Connected(device)) => {
                    self.handle_connected_device(device).await;
                }
                Some(HotplugEvent::Disconnected(device_id)) => {
                    self.handle_disconnected_device(device_id).await;
                }
                None => {
                    break;
                }
            }
        }
        if let Some(rx_loop_join_handle) =  self.rx_loop_join_handle {
            rx_loop_join_handle.await.unwrap();
        }

        if let Some(tx_loop_join_handle) =  self.tx_loop_join_handle {
            tx_loop_join_handle.await.unwrap();
        }
    }

    async fn handle_connected_device(&mut self, device_info: DeviceInfo) {
        {
            if let Some(device_state) = self.connected_device.as_ref()
                && !device_state.terminate_connection_token.is_cancelled()
            {
                return;
            }
        }
        let vid = device_info.vendor_id();
        let pid = device_info.product_id();

        if vid == Self::AOA_VID && (pid == Self::AOA_PID_V1 || pid == Self::AOA_PID_V2) {
            let _ = self.handle_connected_aoa_device(device_info).await;
        } else {
            let res = Self::switch_device_to_aoa_mode(device_info).await;
            if let Err(err) = res {
                warn!("switching device to aoa failed {err:?}");
            }
        }
    }

    async fn handle_connected_aoa_device(
        &mut self,
        device_info: DeviceInfo,
    ) -> Result<(), nusb::Error> {
        info!("handle connected aoa device {device_info:?}");

        let device = match device_info.open() {
            Ok(device) => device,
            Err(err) => {
                error!(
                    "could not open {}:{} - {:?}",
                    device_info.vendor_id(),
                    device_info.product_id(),
                    err
                );
                return Err(err);
            }
        };

        if let Some((
            _config_num,
            interface_num,
            in_endpoint_address,
            out_endpoint_address,
            max_packet_size,
        )) = device.configurations().find_map(|config| {
            config.interfaces().find_map(|interface| {
                interface.alt_settings().find_map(|alt_settings| {
                    if let Some(in_endpoint) = alt_settings.endpoints().find(|endpoint| {
                        endpoint.transfer_type() == EndpointType::Bulk
                            && endpoint.direction() == Direction::In
                    }) {
                        alt_settings
                            .endpoints()
                            .find(|endpoint| {
                                endpoint.transfer_type() == EndpointType::Bulk
                                    && endpoint.direction() == Direction::Out
                            })
                            .map(|out_endpoint| {
                                (
                                    config.configuration_value(),
                                    interface.interface_number(),
                                    in_endpoint.address(),
                                    out_endpoint.address(),
                                    in_endpoint.max_packet_size(),
                                )
                            })
                    } else {
                        None
                    }
                })
            })
        }) {
            //if device_info.product_id() == Self::AOA_PID_V2 {
            //    device.set_configuration(config_num).unwrap();
            //}
            time::sleep(Duration::from_millis(300)).await;

            let interface = match device.claim_interface(interface_num) {
                Ok(interface) => interface,
                Err(err) => {
                    error!("could not claim {interface_num}");
                    return Err(err);
                }
            };
            let terminate_connection_token = CancellationToken::new();

            assert!(self.connected_device.is_none());

            self.connected_device = Some(ConnectedDeviceState {
                connected_device_id: device_info.id(),
                terminate_connection_token: terminate_connection_token.clone(),
            });

            {
                // empty queue before starting a new connection
                let mut outgoing_queue_receiver = self.outgoing_queue_receiver.lock().await;
                while !outgoing_queue_receiver.is_empty() {
                    outgoing_queue_receiver.try_recv().unwrap();
                }
            }
            self.rx_loop_join_handle = Some(tokio::spawn(Self::start_aoa_rx_loop(
                self.incoming_queue_sender.clone(),
                interface.clone(),
                in_endpoint_address,
                max_packet_size,
                terminate_connection_token.clone(),
            )));
            self.tx_loop_join_handle = Some(tokio::spawn(Self::start_aoa_tx_loop(
                self.outgoing_queue_receiver.clone(),
                interface.clone(),
                out_endpoint_address,
                max_packet_size,
                terminate_connection_token.clone(),
            )));
        }

        Ok(())
    }

    async fn handle_disconnected_device(&mut self, device_id: DeviceId) {
        info!("disconnected device {device_id:?}");
        if let Some(device_state) = self
            .connected_device
            .take_if(|device_state| device_state.connected_device_id == device_id)
        {
            device_state.terminate_connection_token.cancel();
        }
    }

    async fn start_aoa_rx_loop(
        incoming_queue: mpsc::Sender<IncomingEvent>,
        interface: Interface,
        endpoint: u8,
        packet_size: usize,
        terminate_connection_token: CancellationToken,
    ) {
        incoming_queue.send(IncomingEvent::Connected).await.unwrap();
        let mut reader = UsbReader::new(
            interface,
            endpoint,
            packet_size,
            terminate_connection_token.clone(),
        );
        let mut codec = AAPFrameCodec::new();
        loop {
            let result = codec.read_frame(&mut reader).await;
            match result {
                Err(err) => {
                    if !terminate_connection_token.is_cancelled() {
                        error!("receiving frame failed {err:?}");
                        terminate_connection_token.cancel();
                    }
                    incoming_queue.send(IncomingEvent::Closed).await.unwrap();
                    return;
                }
                Ok(frame) => {
                    incoming_queue
                        .send(IncomingEvent::Frame(frame))
                        .await
                        .unwrap();
                }
            }
        }
    }

    async fn start_aoa_tx_loop(
        outgoing_queue: Arc<Mutex<mpsc::Receiver<OutgoingEvent>>>,
        interface: Interface,
        endpoint: u8,
        packet_size: usize,
        terminate_connection_token: CancellationToken,
    ) {
        let mut writer = UsbWriter::new(
            interface,
            endpoint,
            packet_size,
            terminate_connection_token.clone(),
        );
        let mut codec = AAPFrameCodec::new();
        let mut outgoing_queue = outgoing_queue.lock().await;

        loop {
            let frame: Option<OutgoingEvent> = select! {
                _ = terminate_connection_token.cancelled()  => { break; }
                frame = outgoing_queue.recv() => frame
            };
            match frame {
                Some(OutgoingEvent::Frame(frame)) => {
                    let mut result = codec.write_frame(frame, &mut writer).await;

                    if result.is_ok()
                    /*&& outgoing_queue.is_empty()*/
                    {
                        result = writer.flush_buffer().await;
                    }

                    if let Err(err) = result {
                        error!("sending frame failed {err:?}");
                        terminate_connection_token.cancel();
                        break;
                    }
                }
                Some(OutgoingEvent::Close) => {
                    terminate_connection_token.cancel();
                }
                None => break,
            }
        }
    }

    async fn switch_device_to_aoa_mode(device_info: DeviceInfo) -> Result<(), nusb::Error> {
        info!("switching usb device {:?} to aoa mode", device_info.id());
        let device = match device_info.open() {
            Ok(device) => device,
            Err(err) => {
                warn!(
                    "could not open {}:{} - {:?}",
                    device_info.vendor_id(),
                    device_info.product_id(),
                    err
                );
                return Err(err);
            }
        };

        let interface = match device.claim_interface(0) {
            Ok(interface) => interface,
            Err(err) => {
                warn!("could not claim {err:?}");
                return Err(err);
            }
        };

        let send_control_in =
            async |control: ControlIn| interface.control_in(control).await.into_result();

        let send_control_out =
            async |control: ControlOut| interface.control_out(control).await.into_result();

        let response = send_control_in(ControlIn {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: 51,
            value: 0,
            index: 0,
            length: 2,
        })
        .await;

        match response.as_deref() {
            Ok([2, 0]) => (),
            _ => return Ok(()),
        }

        let send_string = async |string_id: u16, string: &str| {
            let uri = CString::new(string).unwrap();

            send_control_out(ControlOut {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: 52,
                value: 0,
                index: string_id,
                data: uri.as_bytes_with_nul(),
            })
            .await
        };

        send_string(0, "Android").await?;
        send_string(1, "Android").await?;
        send_string(2, "Auto").await?;
        send_string(3, "1.0").await?;
        send_string(4, "").await?;
        send_string(5, "").await?;

        send_control_out(ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: 53,
            value: 0,
            index: 0,
            data: &[],
        })
        .await?;

        info!("switching to aoa mode done");

        Ok(())
    }
}

struct UsbReader {
    interface: Interface,
    endpoint: u8,
    packet_size: usize,
    terminate_connection_token: CancellationToken,
    buffer: Option<Vec<u8>>,
    buffer_offset: usize,
}

impl UsbReader {
    fn new(
        interface: Interface,
        endpoint: u8,
        packet_size: usize,
        terminate_connection_token: CancellationToken,
    ) -> Self {
        let buffer = Some(Vec::with_capacity(packet_size));
        Self {
            interface,
            endpoint,
            packet_size,
            terminate_connection_token,
            buffer,
            buffer_offset: 0,
        }
    }

    async fn refill_buffer(&mut self) -> Result<(), std::io::Error> {
        assert!(self.buffer.as_ref().unwrap().len() == self.buffer_offset);
        loop {
            let buf = RequestBuffer::reuse(self.buffer.take().unwrap(), self.packet_size);

            let transfer_completion = select! {
                _ = self.terminate_connection_token.cancelled() => { return Err(Error::from(ErrorKind::UnexpectedEof)); }
                packet = self.interface.bulk_in(self.endpoint, buf) => packet
            };

            let response = transfer_completion.into_result();

            match response {
                Ok(packet) => {
                    if packet.is_empty() {
                        select! {
                            _ = self.terminate_connection_token.cancelled() => { return Err(Error::from(ErrorKind::UnexpectedEof)); }
                            _ = time::sleep(Duration::from_millis(2)) => {}
                        };
                        self.buffer = Some(packet);
                        self.buffer_offset = 0;
                    } else {
                        trace!("usb < {:?}", &packet);
                        self.buffer = Some(packet);
                        self.buffer_offset = 0;
                        return Ok(());
                    }
                }
                Err(err) => {
                    return Err(Error::new(ErrorKind::UnexpectedEof, err));
                }
            }
        }
    }
}

impl AsyncReader for UsbReader {
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), std::io::Error> {
        let mut buf = &mut buf[..];
        loop {
            let read_buffer = self.buffer.as_ref().unwrap();
            let read_buffer = &read_buffer[self.buffer_offset..];
            let bytes_available = read_buffer.len();
            let pending = buf.len();
            if bytes_available >= pending {
                let bytes_to_copy = pending;
                buf.copy_from_slice(&read_buffer[..bytes_to_copy]);
                self.buffer_offset += bytes_to_copy;
                return Ok(());
            } else {
                let bytes_to_copy = bytes_available;
                buf[..bytes_to_copy].copy_from_slice(read_buffer);
                self.buffer_offset += bytes_to_copy;
                buf = &mut buf[bytes_to_copy..];
                self.refill_buffer().await?;
            }
        }
    }
}

struct UsbWriter {
    interface: Interface,
    endpoint: u8,
    packet_size: usize,
    terminate_connection_token: CancellationToken,

    buffer: Option<Vec<u8>>,
}

impl UsbWriter {
    fn new(
        interface: Interface,
        endpoint: u8,
        packet_size: usize,
        terminate_connection_token: CancellationToken,
    ) -> Self {
        Self {
            interface,
            endpoint,
            packet_size,
            terminate_connection_token,
            buffer: Some(Vec::new()),
        }
    }

    async fn flush_buffer(&mut self) -> Result<(), std::io::Error> {
        let buffer = self.buffer.take().unwrap();
        trace!("usb > {:?}", &buffer);

        let buffer = select! {
            response  = self.interface.bulk_out(self.endpoint, buffer) => response.into_result()?,
            _ = self.terminate_connection_token.cancelled() => {return Err(Error::from(ErrorKind::UnexpectedEof));}
        };

        self.buffer = Some(buffer.reuse());
        Ok(())
    }
}

impl AsyncWriter for UsbWriter {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
        let mut buf = buf;

        while !buf.is_empty() {
            let write_buffer = self.buffer.as_mut().unwrap();
            let bytes_available = self.packet_size - write_buffer.len();
            if bytes_available > buf.len() {
                write_buffer.extend_from_slice(buf);
                return Ok(());
            } else {
                write_buffer.extend_from_slice(&buf[..bytes_available]);
                buf = &buf[bytes_available..];
                assert!(write_buffer.len() == self.packet_size);
                self.flush_buffer().await?;
            }
        }
        Ok(())
    }
}
