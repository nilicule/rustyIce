use rustyice_core::error::CodecError;
use rustyice_core::traits::Codec;
use rustyice_core::types::{CodecId, CodecInfo, EncodeConfig, EncodedPacket, PcmFrame};

/// MPEG Audio Layer 3 codec — probe-only for v1.
/// `decode` and `encode` return `CodecError::Unsupported`.
pub struct Mp3Codec;

// MPEG1 Layer3 bitrate table (kbps). Index 0 = free, 15 = bad.
const MPEG2_L3_BITRATES: [Option<u32>; 16] = [
    None,        // 0000 free
    Some(8),     // 0001
    Some(16),    // 0010
    Some(24),    // 0011
    Some(32),    // 0100
    Some(40),    // 0101
    Some(48),    // 0110
    Some(56),    // 0111
    Some(64),    // 1000
    Some(80),    // 1001
    Some(96),    // 1010
    Some(112),   // 1011
    Some(128),   // 1100
    Some(144),   // 1101
    Some(160),   // 1110
    None,        // 1111 bad
];

const MPEG1_L3_BITRATES: [Option<u32>; 16] = [
    None,        // 0000 free
    Some(32),    // 0001
    Some(40),    // 0010
    Some(48),    // 0011
    Some(56),    // 0100
    Some(64),    // 0101
    Some(80),    // 0110
    Some(96),    // 0111
    Some(112),   // 1000
    Some(128),   // 1001
    Some(160),   // 1010
    Some(192),   // 1011
    Some(224),   // 1100
    Some(256),   // 1101
    Some(320),   // 1110
    None,        // 1111 bad
];

/// Scan `data` for the first valid MPEG1 CBR Layer3 frame and return its
/// bitrate in bytes/sec. Skips ID3 tags and garbage bytes automatically.
/// Returns `None` for VBR streams (Xing header uses free-bitrate slot).
#[must_use]
pub fn scan_bitrate_bps(data: &[u8]) -> Option<u64> {
    let codec = Mp3Codec;
    let end = data.len().saturating_sub(3);
    for i in 0..end {
        if data[i] != 0xFF || (data[i + 1] & 0xE0) != 0xE0 {
            continue;
        }
        if let Some(info) = codec.probe(&data[i..])
            && let Some(kbps) = info.bitrate_kbps
        {
            return Some(u64::from(kbps) * 1000 / 8);
        }
    }
    None
}

/// Returns the total byte length of the MPEG Layer-3 frame whose header begins
/// at the start of `header`.  Returns `None` if the header is too short, has an
/// invalid sync word, uses a free/reserved bitrate, or is not a Layer-3 frame.
///
/// This is used by `StreamDecoder` to identify complete-frame boundaries before
/// handing data to Symphonia, so that `WouldBlock` from the pipe source never
/// fires mid-frame (which would cause the format reader to resync and skip frames).
#[must_use]
pub fn mp3_frame_size(header: &[u8]) -> Option<usize> {
    if header.len() < 4 {
        return None;
    }
    if header[0] != 0xFF || (header[1] & 0xE0) != 0xE0 {
        return None;
    }

    let version_bits = (header[1] >> 3) & 0x03;
    let layer_bits   = (header[1] >> 1) & 0x03;

    // Reserved MPEG version, reserved layer, or not Layer 3.
    if version_bits == 0b01 || layer_bits == 0b00 || layer_bits != 0b01 {
        return None;
    }

    let sr_index = (header[2] >> 2) & 0x03;
    let sr = sample_rate(version_bits, sr_index)? as usize;

    let br_index = ((header[2] >> 4) & 0x0F) as usize;
    let mpeg1 = version_bits == 0b11;

    let br_kbps = if mpeg1 {
        MPEG1_L3_BITRATES[br_index]?
    } else {
        MPEG2_L3_BITRATES[br_index]?
    } as usize;

    let padding = ((header[2] >> 1) & 0x01) as usize;

    Some(if mpeg1 {
        144 * br_kbps * 1000 / sr + padding
    } else {
        72 * br_kbps * 1000 / sr + padding
    })
}

/// If `data` starts with an MP3 frame carrying a Xing/Info/VBRI tag, return the
/// byte offset where that frame ends. Live-streaming clients (browsers, VLC)
/// that see this tag treat the response as a finite file with known duration
/// and use file-mode buffering (~300ms) instead of stream-mode (~1s), which
/// causes periodic underruns. Stripping the tag frame makes the stream look
/// like raw audio frames so clients stay in stream mode.
///
/// Returns `None` if no tag is present or the buffer is too short to contain
/// the full tagged frame.
#[must_use]
pub fn detect_xing_frame_end(data: &[u8]) -> Option<usize> {
    let codec = Mp3Codec;
    let end = data.len().saturating_sub(3);
    for sync in 0..end {
        if data[sync] != 0xFF || (data[sync + 1] & 0xE0) != 0xE0 {
            continue;
        }
        let header = &data[sync..];
        let info = codec.probe(header)?;
        let mpeg1 = (header[1] >> 3) & 0x03 == 0b11;

        // Tag starts right after the header + side info; side info size
        // depends on MPEG version and channel mode.
        let tag_offset = match (mpeg1, info.channels) {
            (true, 2) => 4 + 32,
            (true, 1) | (false, 2) => 4 + 17,
            (false, 1) => 4 + 9,
            _ => return None,
        };
        let tag_start = sync + tag_offset;
        if tag_start + 4 > data.len() {
            return None;
        }
        let tag = &data[tag_start..tag_start + 4];
        if tag != b"Xing" && tag != b"Info" && tag != b"VBRI" {
            return None;
        }

        let bitrate = info.bitrate_kbps? as usize * 1000;
        let padding = ((header[2] >> 1) & 0x01) as usize;
        let frame_length = if mpeg1 {
            144 * bitrate / info.sample_rate as usize + padding
        } else {
            72 * bitrate / info.sample_rate as usize + padding
        };
        let frame_end = sync + frame_length;
        if frame_end > data.len() {
            return None;
        }
        return Some(frame_end);
    }
    None
}

impl Codec for Mp3Codec {
    fn codec_id(&self) -> CodecId {
        CodecId::MP3
    }

    fn probe(&self, header: &[u8]) -> Option<CodecInfo> {
        if header.len() < 4 {
            return None;
        }

        // Sync: byte 0 must be 0xFF; byte 1 high 3 bits must be 0b111.
        if header[0] != 0xFF || (header[1] & 0xE0) != 0xE0 {
            return None;
        }

        let version_bits = (header[1] >> 3) & 0x03;
        let layer_bits   = (header[1] >> 1) & 0x03;

        // Reserved MPEG version or reserved layer → not a valid frame.
        if version_bits == 0b01 || layer_bits == 0b00 {
            return None;
        }

        // This codec only handles Layer 3 (MP3).
        if layer_bits != 0b01 {
            return None;
        }

        let sr_index     = (header[2] >> 2) & 0x03;
        let sample_rate  = sample_rate(version_bits, sr_index)?;

        let bitrate_index = ((header[2] >> 4) & 0x0F) as usize;
        let bitrate_kbps  = if version_bits == 0b11 {
            MPEG1_L3_BITRATES[bitrate_index]
        } else {
            None // MPEG2/2.5 bitrate tables differ; skip for now
        };

        let channels = if (header[3] >> 6) == 0b11 { 1u8 } else { 2u8 };

        Some(CodecInfo { id: CodecId::MP3, sample_rate, channels, bitrate_kbps })
    }

    fn decode(&self, _packet: &EncodedPacket) -> Result<PcmFrame, CodecError> {
        Err(CodecError::Unsupported)
    }

    fn encode(&self, _frame: &PcmFrame, _config: &EncodeConfig) -> Result<EncodedPacket, CodecError> {
        Err(CodecError::Unsupported)
    }
}

#[must_use]
fn sample_rate(version: u8, index: u8) -> Option<u32> {
    match (version, index) {
        (0b11, 0b00) => Some(44_100),
        (0b11, 0b01) => Some(48_000),
        (0b11, 0b10) => Some(32_000),
        (0b10, 0b00) => Some(22_050),
        (0b10, 0b01) => Some(24_000),
        (0b10, 0b10) => Some(16_000),
        (0b00, 0b00) => Some(11_025),
        (0b00, 0b01) => Some(12_000),
        (0b00, 0b10) => Some(8_000),
        _ => None, // reserved (0b11 index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyice_core::traits::Codec;

    // MPEG1 Layer3 128kbps 44100Hz stereo, no CRC:
    //   0xFF 0xFB 0x90 0x04
    //   Byte1: 1111 1011 → sync(111) MPEG1(11) Layer3(01) NoCRC(1)
    //   Byte2: 1001 0000 → bitrate(1001=128k) samplerate(00=44100) pad(0) priv(0)
    //   Byte3: 0000 0100 → stereo(00) modeext(00) copy(0) orig(1) emph(00)
    const MPEG1_L3_128K_44100_STEREO: [u8; 4] = [0xFF, 0xFB, 0x90, 0x04];

    // MPEG1 Layer3 any bitrate 44100Hz mono:
    //   Byte3: 1100 0000 → mono(11) modeext(00) copy(0) orig(0) emph(00)
    const MPEG1_L3_44100_MONO: [u8; 4] = [0xFF, 0xFB, 0x90, 0xC0];

    // MPEG1 Layer3 320kbps 48000Hz stereo:
    //   Byte2: 1110 0100 → bitrate(1110=320k) samplerate(01=48000) pad(0) priv(0)
    const MPEG1_L3_320K_48000_STEREO: [u8; 4] = [0xFF, 0xFB, 0xE4, 0x04];

    // MPEG2 Layer3 (byte1 bit4-3 = 0b10):
    //   Byte1: 1111 0011 → sync(111) MPEG2(10) Layer3(01) NoCRC(1)
    const MPEG2_L3: [u8; 4] = [0xFF, 0xF3, 0x90, 0x04];

    // Not an MP3 frame:
    const NOT_MP3: [u8; 4] = [0x00, 0x00, 0x00, 0x00];

    #[test]
    fn probes_mpeg1_layer3_stereo() {
        let codec = Mp3Codec;
        let info = codec.probe(&MPEG1_L3_128K_44100_STEREO).unwrap();
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bitrate_kbps, Some(128));
    }

    #[test]
    fn probes_mpeg1_layer3_mono() {
        let codec = Mp3Codec;
        let info = codec.probe(&MPEG1_L3_44100_MONO).unwrap();
        assert_eq!(info.channels, 1);
    }

    #[test]
    fn probes_mpeg1_layer3_320k_48000() {
        let codec = Mp3Codec;
        let info = codec.probe(&MPEG1_L3_320K_48000_STEREO).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.bitrate_kbps, Some(320));
    }

    #[test]
    fn probes_mpeg2_layer3_returns_some() {
        // MPEG2 is valid MP3 data; we accept it but return None bitrate.
        let codec = Mp3Codec;
        assert!(codec.probe(&MPEG2_L3).is_some());
    }

    #[test]
    fn rejects_non_mp3_bytes() {
        let codec = Mp3Codec;
        assert!(codec.probe(&NOT_MP3).is_none());
    }

    #[test]
    fn rejects_too_short_input() {
        let codec = Mp3Codec;
        assert!(codec.probe(&[0xFF, 0xFB]).is_none());
    }

    #[test]
    fn scan_finds_bitrate_at_stream_start() {
        // MPEG1 Layer3 128kbps 44100Hz — should detect 16000 B/s
        let data = [0xFF, 0xFB, 0x90, 0x04, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(scan_bitrate_bps(&data), Some(128 * 1000 / 8));
    }

    #[test]
    fn scan_finds_bitrate_after_garbage_prefix() {
        let mut data = vec![0x00u8; 64];
        data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x04]);
        assert_eq!(scan_bitrate_bps(&data), Some(128 * 1000 / 8));
    }

    #[test]
    fn scan_returns_none_for_non_mp3_data() {
        let data = vec![0x00u8; 64];
        assert_eq!(scan_bitrate_bps(&data), None);
    }

    #[test]
    fn scan_returns_none_for_short_input() {
        assert_eq!(scan_bitrate_bps(&[0xFF, 0xFB]), None);
    }

    fn build_frame(channels: u8, with_xing: bool) -> Vec<u8> {
        // MPEG1 Layer3 128k 44.1kHz, frame size 144*128000/44100 = 417 bytes.
        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        // Byte 3 channel mode bits 7-6: 0b00 stereo, 0b11 mono.
        frame[3] = if channels == 1 { 0xC0 } else { 0x04 };
        if with_xing {
            let tag_offset = if channels == 1 { 4 + 17 } else { 4 + 32 };
            frame[tag_offset..tag_offset + 4].copy_from_slice(b"Xing");
        }
        frame
    }

    #[test]
    fn detects_xing_in_mpeg1_stereo_frame() {
        let frame = build_frame(2, true);
        assert_eq!(detect_xing_frame_end(&frame), Some(417));
    }

    #[test]
    fn detects_info_tag_too() {
        let mut frame = build_frame(2, false);
        frame[36..40].copy_from_slice(b"Info");
        assert_eq!(detect_xing_frame_end(&frame), Some(417));
    }

    #[test]
    fn detects_xing_in_mpeg1_mono_frame() {
        let frame = build_frame(1, true);
        assert_eq!(detect_xing_frame_end(&frame), Some(417));
    }

    #[test]
    fn returns_none_when_no_tag() {
        let frame = build_frame(2, false);
        assert_eq!(detect_xing_frame_end(&frame), None);
    }

    #[test]
    fn returns_none_when_buffer_truncates_frame() {
        let mut frame = build_frame(2, true);
        frame.truncate(200); // Less than the 417-byte frame
        assert_eq!(detect_xing_frame_end(&frame), None);
    }

    #[test]
    fn detects_xing_after_garbage_prefix() {
        let mut data = vec![0xAAu8; 50];
        data.extend_from_slice(&build_frame(2, true));
        assert_eq!(detect_xing_frame_end(&data), Some(50 + 417));
    }

    #[test]
    fn decode_returns_unsupported() {
        use rustyice_core::error::CodecError;
        use rustyice_core::types::EncodedPacket;
        use bytes::Bytes;
        use rustyice_core::types::CodecId;
        let codec = Mp3Codec;
        let packet = EncodedPacket { codec: CodecId::MP3, data: Bytes::new() };
        assert!(matches!(codec.decode(&packet), Err(CodecError::Unsupported)));
    }

    #[test]
    fn encode_returns_unsupported() {
        use rustyice_core::error::CodecError;
        use rustyice_core::types::{EncodeConfig, PcmFrame};
        let codec = Mp3Codec;
        let frame = PcmFrame { samples: vec![], sample_rate: 44_100, channels: 2 };
        let cfg = EncodeConfig { sample_rate: 44_100, channels: 2, bitrate_kbps: 128 };
        assert!(matches!(codec.encode(&frame, &cfg), Err(CodecError::Unsupported)));
    }
}
