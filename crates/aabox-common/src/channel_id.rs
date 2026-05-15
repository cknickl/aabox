//! AAP channel identifiers.
//!
//! Per the AAP wire protocol, the first byte of every frame is a channel ID.
//! These values are derived from aasdk's `aasdk_proto/ChannelIdEnum.proto` —
//! once that file is wired up via the submodule, replace this enum with the
//! generated one from aabox-proto.

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
    AudioInput = 7,
    Bluetooth = 8,
    NavigationStatus = 9,
    MediaStatus = 10,
    WifiProjection = 11,
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
            7 => Ok(Self::AudioInput),
            8 => Ok(Self::Bluetooth),
            9 => Ok(Self::NavigationStatus),
            10 => Ok(Self::MediaStatus),
            11 => Ok(Self::WifiProjection),
            other => Err(other),
        }
    }
}
