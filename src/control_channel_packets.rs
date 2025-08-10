use std::time::{SystemTime};

use crate::{frame::AAPFrameType, packet::Packet, protos};

pub(crate) const CHANNEL_ID_CONTROL: u8 = 0;
pub(crate) const MESSAGE_ID_GET_VERSION_REQUEST: u16 = 0x01;
pub(crate) const MESSAGE_ID_GET_VERSION_RESPONSE: u16 = 0x02;
pub(crate) const MESSAGE_ID_HANDSHAKE: u16 = 0x03;
pub(crate) const MESSAGE_ID_AUTH_COMPLETE: u16 = 0x04;
pub(crate) const MESSAGE_ID_SERVICE_DISCOVERY_REQUEST: u16 = 0x05;
pub(crate) const MESSAGE_ID_SERVICE_DISCOVERY_RESPONSE: u16 = 0x06;
pub(crate) const MESSAGE_ID_OPEN_CHANNEL_REQUEST: u16 = 0x07;
pub(crate) const MESSAGE_ID_OPEN_CHANNEL_RESPONSE: u16 = 0x08;
pub(crate) const MESSAGE_ID_PING_REQUEST: u16 = 0x0B;
pub(crate) const MESSAGE_ID_PING_RESPONSE: u16 = 0x0C;
pub(crate) const MESSAGE_ID_AUDIO_FOCUS_NOTIFICATION_REQUEST: u16 = 0x12;
pub(crate) const MESSAGE_ID_AUDIO_FOCUS_NOTIFICATION: u16 = 0x13;
pub(crate) const MESSAGE_ID_SHUTDOWN_REQUEST: u16 = 0x0f;
pub(crate) const MESSAGE_ID_SHUTDOWN_RESPONSE: u16 = 0x10;

pub(crate) fn build_version_request_packet() -> Packet {
    Packet::new_from_parts(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_GET_VERSION_REQUEST,
        &[0, 1, 0, 1],
    )
}

pub(crate) fn build_handshake_packet(payload: &[u8]) -> Packet {
    Packet::new_from_parts(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_HANDSHAKE,
        payload,
    )
}

pub(crate) fn build_auth_complete_packet() -> Packet {
    let payload = protos::AuthResponse {
        status: protos::MessageStatus::StatusSuccess as i32,
    };
    Packet::new_from_proto_message(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_AUTH_COMPLETE,
        &payload,
    )
}

pub(crate) fn build_service_discovery_response_packet(
    message: protos::ServiceDiscoveryResponse,
) -> Packet {
    Packet::new_from_proto_message(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        true,
        MESSAGE_ID_SERVICE_DISCOVERY_RESPONSE,
        &message,
    )
}

pub(crate) fn build_ping_request_packet(data: Vec<u8>) -> Packet {
    let payload = protos::PingRequest {
        timestamp: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
        bug_report: Some(false),
        data: Some(data),
    };
    Packet::new_from_proto_message(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_PING_REQUEST,
        &payload,
    )
}

pub(crate) fn build_focus_notification_packet(focus_state: protos::AudioFocusStateType) -> Packet {
    let payload = protos::AudioFocusNotification {
        focus_state: focus_state as i32,
        ..Default::default()
    };
    Packet::new_from_proto_message(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_AUDIO_FOCUS_NOTIFICATION,
        &payload,
    )
}

pub(crate) fn build_channel_open_response(channel_id: u8, status: protos::MessageStatus) -> Packet {
    let payload = protos::ChannelOpenResponse {
        status: status as i32,
    };
    Packet::new_from_proto_message(
        channel_id,
        AAPFrameType::Control,
        false,
        MESSAGE_ID_OPEN_CHANNEL_RESPONSE,
        &payload,
    )
}

pub(crate) fn build_shutdown_request() -> Packet {
    Packet::new_from_parts(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_SHUTDOWN_REQUEST,
        &[],
    )
}

pub(crate) fn build_shutdown_response() -> Packet {
    Packet::new_from_parts(
        CHANNEL_ID_CONTROL,
        AAPFrameType::ChannelSpecific,
        false,
        MESSAGE_ID_SHUTDOWN_RESPONSE,
        &[],
    )
}
