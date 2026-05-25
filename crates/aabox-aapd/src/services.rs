//! Service Discovery — tell the head unit which channels we expose plus the
//! per-channel configuration (codecs, resolutions, sample rates, etc.).
//!
//! The car sends `ServiceDiscoveryRequest`; we reply with a
//! `ServiceDiscoveryResponse` whose `channels` list is `Vec<ChannelDescriptor>`,
//! one per channel we advertise.
//!
//! As of Phase 4 we advertise the full minimum a real car expects to see:
//! input, sensor (incl. LOCATION), AV (video out), media-audio out, two AV
//! input audio channels (speech + system mic-in), and a navigation status
//! channel so the head unit will let us deliver TBT instructions.

use aabox_common::ChannelId;
use aabox_proto::data::{
    AudioConfig, AvChannel, AvInputChannel, ChannelDescriptor, InputChannel,
    NavigationChannel, NavigationImageOptions, Sensor, SensorChannel, TouchConfig,
    VideoConfig,
};
use aabox_proto::enums::{
    audio_type, av_stream_type, button_code, sensor_type, video_fps, video_resolution,
};
use aabox_proto::messages::ServiceDiscoveryResponse;
use prost::Message;

/// Screen geometry advertised to the head unit. 1920x1080 is what a typical
/// Carnival-class IVI panel expects to drive. The car *can* renegotiate down
/// if it likes — but advertising 1080p first is what gets us full screen.
pub const SCREEN_W: u32 = 1920;
pub const SCREEN_H: u32 = 1080;
pub const SCREEN_DPI: u32 = 160;

/// Build the full Phase 4 ServiceDiscoveryResponse — every channel the car
/// needs to see in order to launch projection.
pub fn full_response() -> ServiceDiscoveryResponse {
    ServiceDiscoveryResponse {
        channels: vec![
            input_descriptor(),
            sensor_descriptor(),
            video_descriptor(),
            media_audio_descriptor(),
            speech_audio_in_descriptor(),
            system_audio_in_descriptor(),
            navigation_descriptor(),
        ],
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

/// Kept for backwards-compat with the Phase 3 round-trip test. Two-channel
/// version (sensor + video only). New code should call `full_response`.
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
    // We advertise the sensors we want the CAR to push to us. The big one is
    // LOCATION (GPS) — that's the whole point of declaring the channel. We
    // also keep driving status / gear / parking brake so the car will gate
    // restricted UI like keyboards properly.
    ChannelDescriptor {
        channel_id: ChannelId::Sensor as u32,
        sensor_channel: Some(SensorChannel {
            sensors: vec![
                Sensor { r#type: sensor_type::Enum::Location as i32 },
                Sensor { r#type: sensor_type::Enum::DrivingStatus as i32 },
                Sensor { r#type: sensor_type::Enum::Gear as i32 },
                Sensor { r#type: sensor_type::Enum::ParkingBrake as i32 },
                Sensor { r#type: sensor_type::Enum::NightData as i32 },
                Sensor { r#type: sensor_type::Enum::CarSpeed as i32 },
                Sensor { r#type: sensor_type::Enum::Compass as i32 },
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
    // 1080p60 first preferred, with a 720p60 fallback in the same descriptor
    // so a car that can't drive 1080 can pick config_index=1.
    ChannelDescriptor {
        channel_id: ChannelId::Video as u32,
        sensor_channel: None,
        av_channel: Some(AvChannel {
            stream_type: av_stream_type::Enum::Video as i32,
            audio_type: audio_type::Enum::None as i32,
            audio_configs: vec![],
            video_configs: vec![
                VideoConfig {
                    video_resolution: video_resolution::Enum::_1080p as i32,
                    // Our embedded test patterns are encoded at 30 fps
                    // (ffprobe-verified Constrained Baseline). Advertising 60
                    // here would tell a peer to expect ~16ms frame intervals
                    // while our streamer paces at 33ms. Keep 30 for now —
                    // when we switch to MediaCodec hardware capture we can
                    // negotiate 60 cleanly.
                    video_fps: video_fps::Enum::_30 as i32,
                    margin_width: 0,
                    margin_height: 0,
                    dpi: SCREEN_DPI,
                    additional_depth: 0,
                },
                VideoConfig {
                    video_resolution: video_resolution::Enum::_720p as i32,
                    // Our embedded test patterns are encoded at 30 fps
                    // (ffprobe-verified Constrained Baseline). Advertising 60
                    // here would tell a peer to expect ~16ms frame intervals
                    // while our streamer paces at 33ms. Keep 30 for now —
                    // when we switch to MediaCodec hardware capture we can
                    // negotiate 60 cleanly.
                    video_fps: video_fps::Enum::_30 as i32,
                    margin_width: 0,
                    margin_height: 0,
                    dpi: SCREEN_DPI,
                    additional_depth: 0,
                },
            ],
            available_while_in_call: true,
        }),
        input_channel: None,
        av_input_channel: None,
        bluetooth_channel: None,
        navigation_channel: None,
        vendor_extension_channel: None,
    }
}

fn input_descriptor() -> ChannelDescriptor {
    // Touch + a useful baseline of hardware keys. The keycodes match aasdk
    // ButtonCode enum values; the car only cares which scan codes we bind.
    ChannelDescriptor {
        channel_id: ChannelId::Input as u32,
        sensor_channel: None,
        av_channel: None,
        input_channel: Some(InputChannel {
            supported_keycodes: vec![
                button_code::Enum::Home as u32,
                button_code::Enum::Back as u32,
                button_code::Enum::Menu as u32,
                button_code::Enum::Phone as u32,
                button_code::Enum::CallEnd as u32,
                button_code::Enum::Microphone1 as u32,
                button_code::Enum::Up as u32,
                button_code::Enum::Down as u32,
                button_code::Enum::Left as u32,
                button_code::Enum::Right as u32,
                button_code::Enum::Enter as u32,
                button_code::Enum::TogglePlay as u32,
                button_code::Enum::Next as u32,
                button_code::Enum::Prev as u32,
                button_code::Enum::Play as u32,
                button_code::Enum::Pause as u32,
            ],
            touch_screen_config: Some(TouchConfig {
                width: SCREEN_W,
                height: SCREEN_H,
            }),
            touch_pad_config: None,
        }),
        av_input_channel: None,
        bluetooth_channel: None,
        navigation_channel: None,
        vendor_extension_channel: None,
    }
}

fn media_audio_descriptor() -> ChannelDescriptor {
    // Phone -> car: app music / navigation prompts. 48 kHz / 16-bit / stereo
    // is the standard AA media config — cars universally accept it.
    ChannelDescriptor {
        channel_id: ChannelId::MediaAudio as u32,
        sensor_channel: None,
        av_channel: Some(AvChannel {
            stream_type: av_stream_type::Enum::Audio as i32,
            audio_type: audio_type::Enum::Media as i32,
            audio_configs: vec![AudioConfig {
                sample_rate: 48_000,
                bit_depth: 16,
                channel_count: 2,
            }],
            video_configs: vec![],
            available_while_in_call: false,
        }),
        input_channel: None,
        av_input_channel: None,
        bluetooth_channel: None,
        navigation_channel: None,
        vendor_extension_channel: None,
    }
}

fn speech_audio_in_descriptor() -> ChannelDescriptor {
    // Car mic -> phone (voice recognition input). 16 kHz mono is what AOSP
    // assistant pipelines expect. We advertise stereo per the spec's "stereo
    // 16khz" but the wire AudioConfig uses channel_count=2; the head unit
    // will downmix if needed.
    ChannelDescriptor {
        channel_id: ChannelId::SpeechAudio as u32,
        sensor_channel: None,
        av_channel: None,
        input_channel: None,
        av_input_channel: Some(AvInputChannel {
            stream_type: av_stream_type::Enum::Audio as i32,
            audio_config: Some(AudioConfig {
                sample_rate: 16_000,
                bit_depth: 16,
                channel_count: 2,
            }),
            available_while_in_call: true,
        }),
        bluetooth_channel: None,
        navigation_channel: None,
        vendor_extension_channel: None,
    }
}

fn system_audio_in_descriptor() -> ChannelDescriptor {
    // System audio in: the car talking *to* us — chimes, prompts, dings from
    // car buttons. 48 kHz stereo so we don't lose the high end of warning
    // tones.
    ChannelDescriptor {
        channel_id: ChannelId::SystemAudio as u32,
        sensor_channel: None,
        av_channel: None,
        input_channel: None,
        av_input_channel: Some(AvInputChannel {
            stream_type: av_stream_type::Enum::Audio as i32,
            audio_config: Some(AudioConfig {
                sample_rate: 48_000,
                bit_depth: 16,
                channel_count: 2,
            }),
            available_while_in_call: true,
        }),
        bluetooth_channel: None,
        navigation_channel: None,
        vendor_extension_channel: None,
    }
}

fn navigation_descriptor() -> ChannelDescriptor {
    // We are the navigation provider. The car will render our turn icons +
    // play our TTS clips. NavigationChannel.type=1 means turn-by-turn (the
    // common AAP value); minimum_interval_ms throttles status updates.
    ChannelDescriptor {
        channel_id: ChannelId::Navigation as u32,
        sensor_channel: None,
        av_channel: None,
        input_channel: None,
        av_input_channel: None,
        bluetooth_channel: None,
        navigation_channel: Some(NavigationChannel {
            minimum_interval_ms: 1000,
            r#type: 1,
            image_options: Some(NavigationImageOptions {
                width: 256,
                height: 256,
                colour_depth_bits: 16,
            }),
        }),
        vendor_extension_channel: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_response_roundtrips() {
        let r = minimal_response();
        let bytes = encode_response(&r);
        let parsed = ServiceDiscoveryResponse::decode(bytes.as_slice()).unwrap();
        assert_eq!(parsed.head_unit_name, "AABox");
        assert_eq!(parsed.channels.len(), 2);
        let sensor = parsed
            .channels
            .iter()
            .find(|c| c.channel_id == ChannelId::Sensor as u32)
            .unwrap();
        assert!(sensor.sensor_channel.is_some());
        let video = parsed
            .channels
            .iter()
            .find(|c| c.channel_id == ChannelId::Video as u32)
            .unwrap();
        let av = video.av_channel.as_ref().unwrap();
        // Video descriptor now advertises 1080p60 + 720p60 fallback.
        assert_eq!(av.video_configs.len(), 2);
        assert_eq!(
            av.video_configs[0].video_resolution,
            video_resolution::Enum::_1080p as i32
        );
    }

    #[test]
    fn full_response_roundtrips_all_channels() {
        let r = full_response();
        let bytes = encode_response(&r);
        let parsed = ServiceDiscoveryResponse::decode(bytes.as_slice()).unwrap();
        assert_eq!(parsed.channels.len(), 7, "expected 7 advertised channels");

        let by_id = |cid: ChannelId| {
            parsed
                .channels
                .iter()
                .find(|c| c.channel_id == cid as u32)
                .cloned()
                .unwrap_or_else(|| panic!("missing channel id {cid:?}"))
        };

        // Input: touchscreen at 1920x1080 + a non-empty key set.
        let input = by_id(ChannelId::Input);
        let ic = input.input_channel.as_ref().unwrap();
        assert!(!ic.supported_keycodes.is_empty());
        let tc = ic.touch_screen_config.as_ref().unwrap();
        assert_eq!(tc.width, SCREEN_W);
        assert_eq!(tc.height, SCREEN_H);

        // Sensor: must include LOCATION.
        let sensor = by_id(ChannelId::Sensor);
        let sc = sensor.sensor_channel.as_ref().unwrap();
        assert!(sc
            .sensors
            .iter()
            .any(|s| s.r#type == sensor_type::Enum::Location as i32));

        // Video: 1080p60 first.
        let video = by_id(ChannelId::Video);
        let av = video.av_channel.as_ref().unwrap();
        assert_eq!(av.stream_type, av_stream_type::Enum::Video as i32);
        assert_eq!(
            av.video_configs[0].video_resolution,
            video_resolution::Enum::_1080p as i32
        );
        assert_eq!(av.video_configs[0].video_fps, video_fps::Enum::_30 as i32);

        // Media audio: 48k stereo 16-bit.
        let ma = by_id(ChannelId::MediaAudio);
        let av = ma.av_channel.as_ref().unwrap();
        assert_eq!(av.stream_type, av_stream_type::Enum::Audio as i32);
        assert_eq!(av.audio_configs[0].sample_rate, 48_000);
        assert_eq!(av.audio_configs[0].channel_count, 2);

        // Speech audio in: 16 kHz.
        let speech = by_id(ChannelId::SpeechAudio);
        let avi = speech.av_input_channel.as_ref().unwrap();
        assert_eq!(avi.audio_config.as_ref().unwrap().sample_rate, 16_000);

        // System audio in: 48 kHz.
        let sysaudio = by_id(ChannelId::SystemAudio);
        let avi = sysaudio.av_input_channel.as_ref().unwrap();
        assert_eq!(avi.audio_config.as_ref().unwrap().sample_rate, 48_000);

        // Navigation: present + has image options + 1 sec floor.
        let nav = by_id(ChannelId::Navigation);
        let nc = nav.navigation_channel.as_ref().unwrap();
        assert_eq!(nc.minimum_interval_ms, 1000);
        assert!(nc.image_options.is_some());
    }
}
