use bytes::Bytes;
use rustyice_core::error::CodecError;
use rustyice_core::traits::Codec;
use rustyice_core::types::{CodecId, CodecInfo, EncodeConfig, EncodedPacket, PcmFrame};

/// Ogg Vorbis codec — probe-only.  Decode and encode live in
/// `rustyice-transcode` (Symphonia + `vorbis_rs`).
pub struct VorbisCodec;

const OGG_MAGIC: &[u8; 4] = b"OggS";
const VORBIS_IDENT: &[u8; 7] = b"\x01vorbis";

/// Returns the total byte length of the Ogg page whose header starts at the
/// beginning of `header`. `None` if the input is too short, the magic is
/// wrong, or the lacing table cannot be fully read.
///
/// Used by the transcode decoder to forward complete pages to Symphonia, the
/// same role `mp3_frame_size` plays for MP3 frames.
#[must_use]
pub fn ogg_page_size(header: &[u8]) -> Option<usize> {
    if header.len() < 27 || &header[..4] != OGG_MAGIC {
        return None;
    }
    let n_segments = header[26] as usize;
    if header.len() < 27 + n_segments {
        return None;
    }
    let body: usize = header[27..27 + n_segments].iter().map(|&b| b as usize).sum();
    Some(27 + n_segments + body)
}

/// Scan `data` for the first BOS page carrying a Vorbis identification header
/// and return its nominal bitrate in **bytes per second**. Mirrors MP3's
/// `scan_bitrate_bps` and is used for source-rate pacing.
#[must_use]
pub fn scan_nominal_bitrate_bps(data: &[u8]) -> Option<u64> {
    let codec = VorbisCodec;
    let end = data.len().saturating_sub(4);
    for i in 0..=end {
        if &data[i..].get(..4)? != OGG_MAGIC {
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

/// Maximum bytes we'll buffer while hunting for the three Vorbis header
/// pages.  Typical Vorbis streams put all three headers within the first
/// few KB; we give up at 64 KB to bound memory growth on hostile input.
const MAX_HEADER_BUFFER: usize = 64 * 1024;

/// Accumulates the leading bytes of an Ogg Vorbis stream until the three
/// header packets (identification / comment / setup) have been seen,
/// allowing a fresh listener to be primed with the headers before joining
/// a live broadcast mid-stream.
///
/// A Vorbis decoder cannot decode audio packets without all three headers.
/// In rustyIce both Vorbis passthrough and Vorbis-target transcode publish
/// these headers as part of the first bytes hitting the bus, so capture
/// works uniformly by tailing the bus's output side.
pub struct OggHeaderCapture {
    /// Bytes accumulated so far, starting at the source/encoder stream origin.
    buf: Vec<u8>,
    /// Byte offset within `buf` where the next page to be parsed begins.
    cursor: usize,
    /// Number of "fresh packet" pages observed so far. The 4th fresh-packet
    /// page begins the first audio packet; we stop just before it.
    fresh_packet_pages: u32,
    /// Set once the prefix is complete; `header_bytes()` returns `Some`.
    done: bool,
    /// Set when we encounter a non-Ogg/non-Vorbis stream or exceed the
    /// buffer cap; capture is abandoned and `header_bytes()` returns `None`.
    failed: bool,
}

impl Default for OggHeaderCapture {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            fresh_packet_pages: 0,
            done: false,
            failed: false,
        }
    }
}

impl OggHeaderCapture {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` once enough bytes have been seen to extract the three
    /// header pages, OR if capture has failed. Either way the caller can stop
    /// feeding bytes.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.done || self.failed
    }

    /// Take captured header bytes, if any. Returns `None` until `is_settled`
    /// is true with `done`, or permanently `None` if capture failed.
    #[must_use]
    pub fn header_bytes(&self) -> Option<Bytes> {
        if self.done {
            Some(Bytes::copy_from_slice(&self.buf))
        } else {
            None
        }
    }

    /// Feed more bytes from the encoded stream.  Idempotent after settling.
    pub fn push(&mut self, data: &[u8]) {
        if self.is_settled() {
            return;
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() > MAX_HEADER_BUFFER {
            self.failed = true;
            self.buf.clear();
            return;
        }
        loop {
            let page_start = self.cursor;
            if page_start + 27 > self.buf.len() {
                return; // need the page header
            }
            if &self.buf[page_start..page_start + 4] != OGG_MAGIC {
                self.failed = true;
                self.buf.clear();
                return;
            }
            let n_segments = self.buf[page_start + 26] as usize;
            let lacing_end = page_start + 27 + n_segments;
            if lacing_end > self.buf.len() {
                return; // need the lacing table
            }
            let body_len: usize = self.buf[page_start + 27..lacing_end]
                .iter()
                .map(|&b| b as usize)
                .sum();
            let page_size = 27 + n_segments + body_len;
            if page_start + page_size > self.buf.len() {
                return; // page body not yet fully buffered
            }
            let header_type = self.buf[page_start + 5];
            let is_continuation = (header_type & 0x01) != 0;

            if !is_continuation {
                // 4th fresh-packet page is the first audio packet — stop
                // *before* this page so `buf` ends on a packet boundary.
                if self.fresh_packet_pages >= 3 {
                    self.buf.truncate(page_start);
                    self.done = true;
                    return;
                }
                self.fresh_packet_pages += 1;
            }
            self.cursor = page_start + page_size;
        }
    }
}

impl Codec for VorbisCodec {
    fn codec_id(&self) -> CodecId {
        CodecId::VORBIS
    }

    fn probe(&self, header: &[u8]) -> Option<CodecInfo> {
        // Header layout: Ogg page (27 + n_segments bytes of header) wrapping
        // the Vorbis identification packet (30 bytes). We need the body start
        // and at least 30 bytes of body to read fields up through bitrate_min.
        if header.len() < 27 {
            return None;
        }
        if &header[..4] != OGG_MAGIC {
            return None;
        }
        // Must be a BOS page (header_type bit 1 set).
        if header[5] & 0x02 == 0 {
            return None;
        }
        let n_segments = header[26] as usize;
        let header_len = 27 + n_segments;
        if header.len() < header_len + 30 {
            return None;
        }
        let body = &header[header_len..];

        // Identification packet starts with 0x01 + "vorbis".
        if &body[..7] != VORBIS_IDENT {
            return None;
        }

        // vorbis_version at bytes 7..11 must be 0.
        let vorbis_version = u32::from_le_bytes([body[7], body[8], body[9], body[10]]);
        if vorbis_version != 0 {
            return None;
        }

        let channels = body[11];
        if channels == 0 {
            return None;
        }

        let sample_rate = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
        if sample_rate == 0 {
            return None;
        }

        let bitrate_nominal = i32::from_le_bytes([body[20], body[21], body[22], body[23]]);
        let bitrate_kbps = if bitrate_nominal > 0 {
            #[allow(clippy::cast_sign_loss)]
            Some(bitrate_nominal as u32 / 1000)
        } else {
            None
        };

        Some(CodecInfo {
            id: CodecId::VORBIS,
            sample_rate,
            channels,
            bitrate_kbps,
        })
    }

    fn decode(&self, _packet: &EncodedPacket) -> Result<PcmFrame, CodecError> {
        Err(CodecError::Unsupported)
    }

    fn encode(&self, _frame: &PcmFrame, _config: &EncodeConfig) -> Result<EncodedPacket, CodecError> {
        Err(CodecError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyice_core::traits::Codec;

    /// Build a minimal Ogg BOS page containing a Vorbis identification packet.
    /// Returns the full page bytes. `bitrate_nominal_kbps`=0 → header writes 0
    /// (probe will return `bitrate_kbps: None`).
    fn build_vorbis_bos_page(
        sample_rate: u32,
        channels: u8,
        bitrate_nominal_kbps: i32,
    ) -> Vec<u8> {
        // 30-byte identification packet.
        let mut packet = Vec::with_capacity(30);
        packet.extend_from_slice(VORBIS_IDENT);
        packet.extend_from_slice(&0u32.to_le_bytes());          // vorbis_version
        packet.push(channels);
        packet.extend_from_slice(&sample_rate.to_le_bytes());
        packet.extend_from_slice(&0i32.to_le_bytes());          // bitrate_max
        packet.extend_from_slice(&(bitrate_nominal_kbps * 1000).to_le_bytes());
        packet.extend_from_slice(&0i32.to_le_bytes());          // bitrate_min
        packet.push(0xB8);                                       // blocksize bits
        packet.push(0x01);                                       // framing flag
        assert_eq!(packet.len(), 30);

        // Ogg page: header type 0x02 (BOS), one segment of 30 bytes.
        let mut page = Vec::with_capacity(27 + 1 + 30);
        page.extend_from_slice(OGG_MAGIC);
        page.push(0);                                            // version
        page.push(0x02);                                         // header_type: BOS
        page.extend_from_slice(&0u64.to_le_bytes());             // granule
        page.extend_from_slice(&0u32.to_le_bytes());             // serial
        page.extend_from_slice(&0u32.to_le_bytes());             // page_seq
        page.extend_from_slice(&0u32.to_le_bytes());             // checksum (test: skip CRC)
        page.push(1);                                            // page_segments
        page.push(30);                                           // lacing[0]
        page.extend_from_slice(&packet);
        page
    }

    #[test]
    fn probes_vorbis_bos_page() {
        let page = build_vorbis_bos_page(44_100, 2, 192);
        let info = VorbisCodec.probe(&page).expect("should probe");
        assert_eq!(info.id, CodecId::VORBIS);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bitrate_kbps, Some(192));
    }

    #[test]
    fn probe_returns_none_for_zero_nominal_bitrate() {
        let page = build_vorbis_bos_page(48_000, 2, 0);
        let info = VorbisCodec.probe(&page).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert!(info.bitrate_kbps.is_none(), "zero nominal → bitrate omitted");
    }

    #[test]
    fn rejects_truncated_bos() {
        let page = build_vorbis_bos_page(44_100, 2, 128);
        // Cut off in the middle of the identification packet.
        assert!(VorbisCodec.probe(&page[..40]).is_none());
    }

    #[test]
    fn rejects_non_ogg_magic() {
        let mut page = build_vorbis_bos_page(44_100, 2, 128);
        page[0] = b'X';
        assert!(VorbisCodec.probe(&page).is_none());
    }

    #[test]
    fn rejects_non_bos_page() {
        let mut page = build_vorbis_bos_page(44_100, 2, 128);
        page[5] = 0x00; // not BOS
        assert!(VorbisCodec.probe(&page).is_none());
    }

    #[test]
    fn ogg_page_size_matches_lacing_sum() {
        // Header type 0 (continuation), three segments of varying length.
        let mut page = Vec::new();
        page.extend_from_slice(OGG_MAGIC);
        page.push(0);
        page.push(0);
        page.extend_from_slice(&0u64.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.push(3); // page_segments
        page.push(50);
        page.push(100);
        page.push(20);
        // Page body would be 170 bytes; we only need the header for the helper.
        assert_eq!(ogg_page_size(&page), Some(27 + 3 + 170));
    }

    #[test]
    fn ogg_page_size_rejects_truncated_lacing() {
        let mut page = Vec::new();
        page.extend_from_slice(OGG_MAGIC);
        page.push(0);
        page.push(0);
        page.extend_from_slice(&0u64.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.push(4); // says 4 segments, but no lacing bytes follow
        assert!(ogg_page_size(&page).is_none());
    }

    #[test]
    fn ogg_page_size_rejects_non_ogg() {
        let mut page = vec![0u8; 27];
        page[26] = 0; // zero segments
        assert!(ogg_page_size(&page).is_none(), "missing OggS magic → None");
    }

    #[test]
    fn scan_finds_nominal_bitrate() {
        let page = build_vorbis_bos_page(44_100, 2, 128);
        assert_eq!(scan_nominal_bitrate_bps(&page), Some(128 * 1000 / 8));
    }

    #[test]
    fn scan_finds_bitrate_after_garbage_prefix() {
        let mut data = vec![0u8; 32];
        data.extend(build_vorbis_bos_page(44_100, 2, 96));
        assert_eq!(scan_nominal_bitrate_bps(&data), Some(96 * 1000 / 8));
    }

    #[test]
    fn scan_returns_none_for_non_ogg() {
        assert_eq!(scan_nominal_bitrate_bps(&[0u8; 256]), None);
    }

    #[test]
    fn decode_returns_unsupported() {
        use bytes::Bytes;
        let p = EncodedPacket { codec: CodecId::VORBIS, data: Bytes::new() };
        assert!(matches!(VorbisCodec.decode(&p), Err(CodecError::Unsupported)));
    }

    /// Build a fake Ogg page: header_type byte = `header_type`, one segment
    /// of `body.len()` bytes (must be <= 255).
    fn build_page(header_type: u8, body: &[u8]) -> Vec<u8> {
        assert!(body.len() <= 255);
        let mut page = Vec::with_capacity(27 + 1 + body.len());
        page.extend_from_slice(OGG_MAGIC);
        page.push(0);                                            // version
        page.push(header_type);
        page.extend_from_slice(&0u64.to_le_bytes());             // granule
        page.extend_from_slice(&0u32.to_le_bytes());             // serial
        page.extend_from_slice(&0u32.to_le_bytes());             // page_seq
        page.extend_from_slice(&0u32.to_le_bytes());             // checksum
        page.push(1);                                            // page_segments
        page.push(body.len() as u8);                             // lacing[0]
        page.extend_from_slice(body);
        page
    }

    fn ident_packet() -> Vec<u8> {
        let mut p = Vec::with_capacity(30);
        p.extend_from_slice(VORBIS_IDENT);
        p.extend_from_slice(&0u32.to_le_bytes());                // vorbis_version
        p.push(2);                                                // channels
        p.extend_from_slice(&44_100u32.to_le_bytes());           // sample_rate
        p.extend_from_slice(&0i32.to_le_bytes());                // bitrate_max
        p.extend_from_slice(&128_000i32.to_le_bytes());          // bitrate_nominal
        p.extend_from_slice(&0i32.to_le_bytes());                // bitrate_min
        p.push(0xB8);                                            // blocksizes
        p.push(0x01);                                            // framing
        p
    }

    #[test]
    fn capture_settles_after_three_header_pages_plus_audio() {
        let mut cap = OggHeaderCapture::new();
        // BOS page with identification packet.
        let ident = build_page(0x02, &ident_packet());
        // Comment header page (fresh packet, smaller body).
        let mut comment_packet = vec![0x03];
        comment_packet.extend_from_slice(b"vorbis");
        comment_packet.extend_from_slice(&[0u8; 16]); // arbitrary payload
        let comment = build_page(0x00, &comment_packet);
        // Setup header page.
        let mut setup_packet = vec![0x05];
        setup_packet.extend_from_slice(b"vorbis");
        setup_packet.extend_from_slice(&[0u8; 32]);
        let setup = build_page(0x00, &setup_packet);
        // First audio packet page (fresh packet, no "vorbis" prefix).
        let audio = build_page(0x00, &[0u8; 64]);

        cap.push(&ident);
        cap.push(&comment);
        cap.push(&setup);
        assert!(!cap.is_settled(), "should still want more bytes before audio page");

        cap.push(&audio);
        assert!(cap.is_settled());
        let bytes = cap.header_bytes().expect("should have header bytes");
        // Captured bytes are exactly ident + comment + setup, not the audio page.
        let expected_len = ident.len() + comment.len() + setup.len();
        assert_eq!(bytes.len(), expected_len);
    }

    #[test]
    fn capture_handles_continuation_pages() {
        let mut cap = OggHeaderCapture::new();
        let ident = build_page(0x02, &ident_packet());
        let mut comment_packet = vec![0x03];
        comment_packet.extend_from_slice(b"vorbis");
        let comment = build_page(0x00, &comment_packet);
        // Big setup spans two pages: first fresh, second a continuation.
        let mut setup_packet = vec![0x05];
        setup_packet.extend_from_slice(b"vorbis");
        setup_packet.extend_from_slice(&[0u8; 200]);
        let setup_a = build_page(0x00, &setup_packet); // fresh-packet start
        let setup_b = build_page(0x01, &[0u8; 100]);   // continuation page
        let audio = build_page(0x00, &[0u8; 64]);

        for chunk in [&ident, &comment, &setup_a, &setup_b, &audio] {
            cap.push(chunk);
        }
        assert!(cap.is_settled());
        let bytes = cap.header_bytes().unwrap();
        assert_eq!(bytes.len(), ident.len() + comment.len() + setup_a.len() + setup_b.len());
    }

    #[test]
    fn capture_streams_partial_pages_progressively() {
        let mut cap = OggHeaderCapture::new();
        let ident = build_page(0x02, &ident_packet());
        let mut comment_packet = vec![0x03];
        comment_packet.extend_from_slice(b"vorbis");
        let comment = build_page(0x00, &comment_packet);
        let mut setup_packet = vec![0x05];
        setup_packet.extend_from_slice(b"vorbis");
        let setup = build_page(0x00, &setup_packet);
        let audio = build_page(0x00, &[0u8; 64]);

        // Feed byte-by-byte to verify the capture handles split pages.
        let mut all = Vec::new();
        all.extend_from_slice(&ident);
        all.extend_from_slice(&comment);
        all.extend_from_slice(&setup);
        all.extend_from_slice(&audio);
        for byte in &all {
            cap.push(std::slice::from_ref(byte));
            if cap.is_settled() {
                break;
            }
        }
        assert!(cap.is_settled());
        assert_eq!(
            cap.header_bytes().unwrap().len(),
            ident.len() + comment.len() + setup.len(),
        );
    }

    #[test]
    fn capture_fails_on_non_ogg_stream() {
        let mut cap = OggHeaderCapture::new();
        cap.push(&[0xFF, 0xFB, 0x90, 0x04, 0, 0, 0, 0]); // MP3 sync word
        cap.push(&vec![0u8; 256]);
        assert!(cap.is_settled());
        assert!(cap.header_bytes().is_none(), "non-Ogg input should fail");
    }

    #[test]
    fn capture_bounded_by_max_buffer() {
        let mut cap = OggHeaderCapture::new();
        // Push lots of valid-magic-but-truncated bytes until we exceed the cap.
        for _ in 0..(MAX_HEADER_BUFFER / 4 + 8) {
            cap.push(b"OggS");
        }
        assert!(cap.is_settled());
        assert!(cap.header_bytes().is_none(), "should give up past the cap");
    }

    #[test]
    fn encode_returns_unsupported() {
        let frame = PcmFrame { samples: vec![], sample_rate: 44_100, channels: 2 };
        let cfg = EncodeConfig { sample_rate: 44_100, channels: 2, bitrate_kbps: 128 };
        assert!(matches!(VorbisCodec.encode(&frame, &cfg), Err(CodecError::Unsupported)));
    }
}
