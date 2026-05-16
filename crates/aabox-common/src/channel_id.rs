//! AAP channel identifiers — values from aasdk's
//! `references/aasdk/include/f1x/aasdk/Messenger/ChannelId.hpp`.
//!
//! These are byte 0 of every AAP frame. Channels beyond `Bluetooth` (e.g.
//! navigation status, media status, wifi projection) are part of negotiated
//! services rather than fixed channel IDs and will land here once Phase 6
//! reverse-engineers them from captures + milek7's notes.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ChannelId {
    Control = 0,
    Input = 1,
    Sensor = 2,
    Video = 3,
    MediaAudio = 4,
    SpeechAudio = 5,
    SystemAudio = 6,
    AvInput = 7,
    Bluetooth = 8,
    None = 255,
}

impl TryFrom<u8> for ChannelId {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Control),
            1 => Ok(Self::Input),
            2 => Ok(Self::Sensor),
            3 => Ok(Self::Video),
            4 => Ok(Self::MediaAudio),
            5 => Ok(Self::SpeechAudio),
            6 => Ok(Self::SystemAudio),
            7 => Ok(Self::AvInput),
            8 => Ok(Self::Bluetooth),
            255 => Ok(Self::None),
            other => Err(other),
        }
    }
}

/// Control-channel message IDs (the u16 prepended to control-channel payloads).
/// Values from aasdk's `ControlMessageIdsEnum.proto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ControlMessageId {
    None = 0x0000,
    VersionRequest = 0x0001,
    VersionResponse = 0x0002,
    SslHandshake = 0x0003,
    AuthComplete = 0x0004,
    ServiceDiscoveryRequest = 0x0005,
    ServiceDiscoveryResponse = 0x0006,
    ChannelOpenRequest = 0x0007,
    ChannelOpenResponse = 0x0008,
    PingRequest = 0x000B,
    PingResponse = 0x000C,
    NavigationFocusRequest = 0x000D,
    NavigationFocusResponse = 0x000E,
    ShutdownRequest = 0x000F,
    ShutdownResponse = 0x0010,
    VoiceSessionRequest = 0x0011,
    AudioFocusRequest = 0x0012,
    AudioFocusResponse = 0x0013,
}

impl TryFrom<u16> for ControlMessageId {
    type Error = u16;

    fn try_from(v: u16) -> Result<Self, Self::Error> {
        use ControlMessageId::*;
        Ok(match v {
            0x0000 => None,
            0x0001 => VersionRequest,
            0x0002 => VersionResponse,
            0x0003 => SslHandshake,
            0x0004 => AuthComplete,
            0x0005 => ServiceDiscoveryRequest,
            0x0006 => ServiceDiscoveryResponse,
            0x0007 => ChannelOpenRequest,
            0x0008 => ChannelOpenResponse,
            0x000B => PingRequest,
            0x000C => PingResponse,
            0x000D => NavigationFocusRequest,
            0x000E => NavigationFocusResponse,
            0x000F => ShutdownRequest,
            0x0010 => ShutdownResponse,
            0x0011 => VoiceSessionRequest,
            0x0012 => AudioFocusRequest,
            0x0013 => AudioFocusResponse,
            other => return Err(other),
        })
    }
}
