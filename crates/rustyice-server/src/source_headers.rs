use http::HeaderMap;
use rustyice_core::mount::SourceOverlay;
use std::str::FromStr;

/// Parse the icecast2-style station identity headers from a source request.
///
/// Reads each field's `Ice-*` spelling first, falling back to `Icy-*` if absent.
/// Malformed values become `None` rather than rejecting the request — icecast
/// behaves permissively here.
#[must_use]
pub fn parse_source_overlay(headers: &HeaderMap) -> SourceOverlay {
    SourceOverlay {
        name: read_string(headers, "ice-name", "icy-name"),
        description: read_string(headers, "ice-description", "icy-description"),
        genre: read_string(headers, "ice-genre", "icy-genre"),
        url: read_string(headers, "ice-url", "icy-url"),
        public: read_public(headers),
        audio_info: read_string(headers, "ice-audio-info", "icy-audioinfo"),
        bitrate_kbps: read_bitrate(headers),
    }
}

fn read_string(headers: &HeaderMap, primary: &str, fallback: &str) -> Option<String> {
    let raw = headers
        .get(primary)
        .or_else(|| headers.get(fallback))?
        .to_str()
        .ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn read_public(headers: &HeaderMap) -> Option<bool> {
    let raw = headers
        .get("ice-public")
        .or_else(|| headers.get("icy-pub"))?
        .to_str()
        .ok()?
        .trim();
    match raw {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

fn read_bitrate(headers: &HeaderMap) -> Option<u32> {
    let raw = headers
        .get("ice-bitrate")
        .or_else(|| headers.get("icy-br"))?
        .to_str()
        .ok()?
        .trim();
    u32::from_str(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name = HeaderName::from_bytes(k.as_bytes()).unwrap();
            h.insert(name, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn empty_headers_produce_empty_overlay() {
        let o = parse_source_overlay(&HeaderMap::new());
        assert!(o.name.is_none());
        assert!(o.description.is_none());
        assert!(o.genre.is_none());
        assert!(o.url.is_none());
        assert!(o.public.is_none());
        assert!(o.audio_info.is_none());
        assert!(o.bitrate_kbps.is_none());
    }

    #[test]
    fn ice_headers_populate_overlay() {
        let h = hm(&[
            ("Ice-Name", "Awesome Radio"),
            ("Ice-Description", "The best"),
            ("Ice-Genre", "Electronic"),
            ("Ice-URL", "https://example.com"),
            ("Ice-Public", "1"),
            ("Ice-Audio-Info", "samplerate=44100;channels=2"),
            ("Ice-Bitrate", "192"),
        ]);
        let o = parse_source_overlay(&h);
        assert_eq!(o.name.as_deref(), Some("Awesome Radio"));
        assert_eq!(o.description.as_deref(), Some("The best"));
        assert_eq!(o.genre.as_deref(), Some("Electronic"));
        assert_eq!(o.url.as_deref(), Some("https://example.com"));
        assert_eq!(o.public, Some(true));
        assert_eq!(o.audio_info.as_deref(), Some("samplerate=44100;channels=2"));
        assert_eq!(o.bitrate_kbps, Some(192));
    }

    #[test]
    fn icy_headers_populate_overlay_when_ice_absent() {
        let h = hm(&[
            ("Icy-Name", "Legacy Radio"),
            ("Icy-Genre", "Talk"),
            ("Icy-Pub", "0"),
            ("Icy-Br", "64"),
            ("Icy-Audioinfo", "samplerate=22050"),
        ]);
        let o = parse_source_overlay(&h);
        assert_eq!(o.name.as_deref(), Some("Legacy Radio"));
        assert_eq!(o.genre.as_deref(), Some("Talk"));
        assert_eq!(o.public, Some(false));
        assert_eq!(o.bitrate_kbps, Some(64));
        assert_eq!(o.audio_info.as_deref(), Some("samplerate=22050"));
    }

    #[test]
    fn ice_wins_when_both_spellings_sent() {
        let h = hm(&[("Ice-Name", "ICE"), ("Icy-Name", "ICY")]);
        let o = parse_source_overlay(&h);
        assert_eq!(o.name.as_deref(), Some("ICE"));
    }

    #[test]
    fn empty_or_whitespace_string_values_become_none() {
        let h = hm(&[
            ("Ice-Name", ""),
            ("Ice-Description", "   "),
            ("Ice-Genre", "\t"),
        ]);
        let o = parse_source_overlay(&h);
        assert!(o.name.is_none());
        assert!(o.description.is_none());
        assert!(o.genre.is_none());
    }

    #[test]
    fn string_values_are_trimmed() {
        let h = hm(&[("Ice-Name", "  Trimmed  ")]);
        let o = parse_source_overlay(&h);
        assert_eq!(o.name.as_deref(), Some("Trimmed"));
    }

    #[test]
    fn malformed_public_becomes_none() {
        let h = hm(&[("Ice-Public", "yes")]);
        let o = parse_source_overlay(&h);
        assert!(o.public.is_none());
    }

    #[test]
    fn malformed_bitrate_becomes_none() {
        let h = hm(&[("Ice-Bitrate", "not-a-number")]);
        let o = parse_source_overlay(&h);
        assert!(o.bitrate_kbps.is_none());
    }

    #[test]
    fn negative_bitrate_becomes_none() {
        let h = hm(&[("Ice-Bitrate", "-1")]);
        let o = parse_source_overlay(&h);
        // u32::from_str rejects negatives.
        assert!(o.bitrate_kbps.is_none());
    }

    #[test]
    fn other_ice_headers_are_ignored() {
        // Headers outside the in-scope set must not pollute the overlay.
        let h = hm(&[
            ("Ice-Notice-1", "do not log this"),
            ("Icy-Reset", "should be ignored"),
            ("Ice-Name", "kept"),
        ]);
        let o = parse_source_overlay(&h);
        assert_eq!(o.name.as_deref(), Some("kept"));
    }

    #[test]
    fn non_utf8_header_value_is_treated_as_absent() {
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_bytes(b"ice-name").unwrap(),
            HeaderValue::from_bytes(&[0xFFu8, 0xFE, 0xFD]).unwrap(),
        );
        let o = parse_source_overlay(&h);
        assert!(o.name.is_none());
    }
}
