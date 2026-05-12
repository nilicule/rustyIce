#![allow(clippy::module_name_repetitions)]

use bytes::Bytes;
use std::time::Duration;

/// Codec identifier. Use the associated constants for known codecs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodecId(pub &'static str);

impl CodecId {
    pub const MP3: Self    = Self("mp3");
    pub const OPUS: Self   = Self("opus");
    pub const VORBIS: Self = Self("vorbis");
    pub const FLAC: Self   = Self("flac");
    pub const AAC: Self    = Self("aac");
    pub const WAV: Self    = Self("wav");

    #[must_use]
    pub fn as_str(&self) -> &'static str { self.0 }
}

impl std::fmt::Display for CodecId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Metadata returned by `Codec::probe`.
#[derive(Debug, Clone)]
pub struct CodecInfo {
    pub id: CodecId,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_kbps: Option<u32>,
}

/// Parameters passed to `Codec::encode`.
#[derive(Debug, Clone)]
pub struct EncodeConfig {
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_kbps: u32,
}

/// A decoded PCM frame. Samples are interleaved, normalized to [-1.0, 1.0].
#[derive(Debug, Clone)]
pub struct PcmFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u8,
}

/// One chunk of encoded audio for a specific codec.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub codec: CodecId,
    pub data: Bytes,
}

/// The payload carried by a `StreamPacket`.
/// v1 only uses `Encoded`. `Decoded` is the hook for the future transcoding pipeline.
#[derive(Debug, Clone)]
pub enum AudioPayload {
    Encoded(EncodedPacket),
    Decoded(PcmFrame),
}

/// The unit that flows through the `BroadcastBus`.
/// Always heap-allocated behind `Arc` so subscribers pay only a pointer clone.
#[derive(Debug, Clone)]
pub struct StreamPacket {
    pub payload: AudioPayload,
    /// Presentation timestamp — enables DVR seek buffer and HLS segmentation.
    pub pts: Duration,
    /// Monotonically increasing per mount, starting at 0.
    pub sequence: u64,
}

/// Statistics returned when a source disconnects.
#[derive(Debug, Clone, Default)]
pub struct SourceStats {
    pub bytes_received: u64,
    pub packets_published: u64,
    pub duration: Duration,
}

/// Reason a listener connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    ClientDisconnected,
    SlowListener,
    Kicked,
    SourceEnded,
    ShuttingDown,
}

/// Statistics returned when a listener disconnects.
#[derive(Debug, Clone)]
pub struct ListenerStats {
    pub bytes_sent: u64,
    pub duration: Duration,
    pub disconnect_reason: DisconnectReason,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_id_constants_have_correct_str() {
        assert_eq!(CodecId::MP3.as_str(), "mp3");
        assert_eq!(CodecId::OPUS.as_str(), "opus");
        assert_eq!(CodecId::VORBIS.as_str(), "vorbis");
        assert_eq!(CodecId::FLAC.as_str(), "flac");
        assert_eq!(CodecId::AAC.as_str(), "aac");
        assert_eq!(CodecId::WAV.as_str(), "wav");
    }

    #[test]
    fn codec_id_equality() {
        assert_eq!(CodecId::MP3, CodecId::MP3);
        assert_ne!(CodecId::MP3, CodecId::OPUS);
    }

    #[test]
    fn stream_packet_encoded_variant() {
        let data = Bytes::from_static(b"\xff\xfb\x90\x00");
        let packet = StreamPacket {
            payload: AudioPayload::Encoded(EncodedPacket {
                codec: CodecId::MP3,
                data: data.clone(),
            }),
            pts: Duration::from_millis(26),
            sequence: 1,
        };
        let AudioPayload::Encoded(ref enc) = packet.payload else {
            panic!("expected Encoded variant");
        };
        assert_eq!(enc.codec, CodecId::MP3);
        assert_eq!(enc.data, data);
        assert_eq!(packet.sequence, 1);
    }

    #[test]
    fn pcm_frame_interleaved_stereo() {
        let frame = PcmFrame {
            samples: vec![0.0_f32, 0.0, 0.5, -0.5],
            sample_rate: 44_100,
            channels: 2,
        };
        assert_eq!(frame.samples.len(), 4);
        assert_eq!(frame.channels, 2);
    }

    #[test]
    fn source_stats_default() {
        let stats = SourceStats::default();
        assert_eq!(stats.bytes_received, 0);
        assert_eq!(stats.packets_published, 0);
    }

    #[test]
    fn disconnect_reason_debug() {
        let reason = DisconnectReason::SlowListener;
        assert_eq!(format!("{reason:?}"), "SlowListener");
    }
}
