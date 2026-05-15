use std::path::Path;

/// Best-effort display title for a file. Returns `Artist - Title` if both are
/// present, `Title` alone if only that is set, otherwise the file stem.
///
/// Read errors and missing-tags both fall back to the stem — playback continues
/// either way.
#[must_use]
pub fn display_title(path: &Path) -> String {
    use lofty::file::TaggedFileExt;
    use lofty::tag::Accessor;

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let Ok(tagged) = lofty::read_from_path(path) else {
        return stem;
    };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return stem;
    };
    let title = tag.title().map(|c| c.into_owned());
    let artist = tag.artist().map(|c| c.into_owned());
    match (artist, title) {
        (Some(a), Some(t)) if !a.is_empty() && !t.is_empty() => format!("{a} - {t}"),
        (_, Some(t)) if !t.is_empty() => t,
        _ => stem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_artist_and_title_from_mp3() {
        let dir = tempfile::tempdir().unwrap();
        let (mp3, _) = crate::test_fixtures::write_test_fixtures(dir.path()).unwrap();
        let title = display_title(&mp3);
        assert_eq!(title, "Test Artist - Test Track");
    }

    #[test]
    fn extracts_artist_and_title_from_ogg() {
        let dir = tempfile::tempdir().unwrap();
        let (_, ogg) = crate::test_fixtures::write_test_fixtures(dir.path()).unwrap();
        let title = display_title(&ogg);
        assert_eq!(title, "Test Artist - Test Track");
    }

    #[test]
    fn falls_back_to_stem_when_tags_missing() {
        let path = std::path::PathBuf::from("/tmp/__no_such_file_xy.mp3");
        let title = display_title(&path);
        assert_eq!(title, "__no_such_file_xy");
    }
}
