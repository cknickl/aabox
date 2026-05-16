//! Service Discovery — tell the head unit which channels we expose plus the
//! per-channel configuration (codecs, resolutions, sample rates, etc.).
//!
//! The car sends `ServiceDiscoveryRequest`; we reply with a
//! `ServiceDiscoveryResponse` whose `channels` list is `Vec<ChannelDescriptor>`,
//! one per channel we advertise. ChannelDescriptor holds `channel_id` plus
//! optional sub-channel configs (sensor_channel, av_channel, input_channel…).
//!
//! Phase 3 advertises the **minimum** the car will accept:
//! - Sensor channel — declared so the car can negotiate (real sensor data is
//!   Phase 5)
//! - Video channel — one resolution, one codec — proves the AV pipeline
//!
//! Audio + Input come in Phase 4; Nav status in Phase 6.

use aabox_common::ChannelId;
use aabox_proto::data::{
    AvChannel, ChannelDescriptor, Sensor, SensorChannel, VideoConfig,
};
use aabox_proto::enums::{
    av_stream_type, sensor_type, video_fps, video_resolution,
};
use aabox_proto::messages::ServiceDiscoveryResponse;
use prost::Message;

/// Build a minimal ServiceDiscoveryResponse advertising sensor + video.
pub fn minimal_response() -> ServiceDiscoveryResponse {
    ServiceDiscoveryResponse {
        channels: vec![sensor_descriptor(), video_descriptor()],
        head_unit_name: "AABox".to_string(),
        car_model: "Generic".to_string(),
        car_year: "2026".to_string(),
        car_serial: "AABOX0001".to_string(),
        left_hand_drive_vehicle: true,
        headunit_manufacturer: "AABox".to_string(),
        headunit_model: "CM5".to_string(),
        sw_build: env!("CARGO_PKG_VERSION").to_string(),
        sw_version: "1.0".to_string(),
        can_play_native_media_during_vr: false,
        hide_clock: false,
    }
}

/// Encode the response payload (without the AAP frame header / message ID).
pub fn encode_response(resp: &ServiceDiscoveryResponse) -> Vec<u8> {
    let mut buf = Vec::with_capacity(resp.encoded_len());
    resp.encode(&mut buf).expect("encode ServiceDiscoveryResponse");
    buf
}

fn sensor_descriptor() -> ChannelDescriptor {
    ChannelDescriptor {
        channel_id: ChannelId::Sensor as u32,
        sensor_channel: Some(SensorChannel {
            sensors: vec![
                Sensor { r#type: sensor_type::Enum::DrivingStatus as i32 },
                Sensor { r#type: sensor_type::Enum::Gear as i32 },
                Sensor { r#type: sensor_type::Enum::ParkingBrake as i32 },
            ],
        }),
        av_channel: None,
        input_channel: None,
        av_input_channel: None,
        bluetooth_channel: None,
        navigation_channel: None,
        vendor_extension_channel: None,
    }
}

fn video_descriptor() -> ChannelDescriptor {
    ChannelDescriptor {
        channel_id: ChannelId::Video as u32,
        sensor_channel: None,
        av_channel: Some(AvChannel {
            stream_type: av_stream_type::Enum::Video as i32,
            audio_type: 0,
            audio_configs: vec![],
            video_configs: vec![VideoConfig {
                video_resolution: video_resolution::Enum::_720p as i32,
                video_fps: video_fps::Enum::_60 as i32,
                margin_width: 0,
                margin_height: 0,
                dpi: 160,
                additional_depth: 0,
            }],
            available_while_in_call: true,
        }),
        input_channel: None,
        av_input_channel: None,
        bluetooth_channel: None,
        navigation_channel: None,
        vendor_extension_channel: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_roundtrips() {
        let r = minimal_response();
        let bytes = encode_response(&r);
        let parsed = ServiceDiscoveryResponse::decode(bytes.as_slice()).unwrap();
        assert_eq!(parsed.head_unit_name, "AABox");
        assert_eq!(parsed.channels.len(), 2);
        // Sensor channel present
        let sensor = parsed
            .channels
            .iter()
            .find(|c| c.channel_id == ChannelId::Sensor as u32)
            .unwrap();
        assert!(sensor.sensor_channel.is_some());
        assert_eq!(sensor.sensor_channel.as_ref().unwrap().sensors.len(), 3);
        // Video channel present with 720p60
        let video = parsed
            .channels
            .iter()
            .find(|c| c.channel_id == ChannelId::Video as u32)
            .unwrap();
        let av = video.av_channel.as_ref().unwrap();
        assert_eq!(av.video_configs.len(), 1);
        assert_eq!(av.video_configs[0].video_resolution, video_resolution::Enum::_720p as i32);
    }
}
