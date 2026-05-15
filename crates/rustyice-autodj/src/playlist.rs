use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Track ordering strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Shuffle,
    Sequential,
}

impl From<rustyice_core::config::Order> for Order {
    fn from(o: rustyice_core::config::Order) -> Self {
        match o {
            rustyice_core::config::Order::Shuffle => Order::Shuffle,
            rustyice_core::config::Order::Sequential => Order::Sequential,
        }
    }
}

/// Walk `root` recursively and return every file whose extension is `mp3` or
/// `ogg` (case-insensitive). Sorted lexicographically.
///
/// # Errors
/// Returns an error if `root` is not a directory or cannot be read.
pub fn scan(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !root.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("autodj folder is not a directory: {}", root.display()),
        ));
    }
    let mut out: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| matches!(
            p.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase).as_deref(),
            Some("mp3") | Some("ogg")
        ))
        .collect();
    out.sort();
    Ok(out)
}

/// Apply `order` to a freshly-scanned list. Returns a deterministic permutation
/// for `Shuffle` when an explicit seed is provided (test hook).
#[must_use]
pub fn order_tracks(mut tracks: Vec<PathBuf>, order: Order, seed: Option<u64>) -> Vec<PathBuf> {
    match order {
        Order::Sequential => tracks,
        Order::Shuffle => {
            use rand::seq::SliceRandom;
            let mut rng: rand::rngs::StdRng = match seed {
                Some(s) => rand::SeedableRng::seed_from_u64(s),
                None => rand::SeedableRng::from_entropy(),
            };
            tracks.shuffle(&mut rng);
            tracks
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::File::create(p).unwrap();
    }

    #[test]
    fn scan_filters_to_mp3_and_ogg_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("a.mp3"));
        touch(&root.join("b.OGG"));
        touch(&root.join("c.flac"));
        touch(&root.join("notes.txt"));
        std::fs::create_dir(root.join("sub")).unwrap();
        touch(&root.join("sub").join("d.Mp3"));

        let mut paths = scan(root).unwrap();
        paths.sort();
        let names: Vec<&str> = paths.iter().filter_map(|p| p.file_name()?.to_str()).collect();
        assert_eq!(names, vec!["a.mp3", "b.OGG", "d.Mp3"]);
    }

    #[test]
    fn scan_errors_on_missing_folder() {
        let err = scan(Path::new("/this/path/does/not/exist")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn order_sequential_is_stable() {
        let tracks = vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
        let out = order_tracks(tracks.clone(), Order::Sequential, None);
        assert_eq!(out, tracks);
    }

    #[test]
    fn order_shuffle_is_deterministic_with_seed() {
        let tracks: Vec<PathBuf> = (0..50).map(|i| PathBuf::from(format!("{i:03}"))).collect();
        let a = order_tracks(tracks.clone(), Order::Shuffle, Some(42));
        let b = order_tracks(tracks.clone(), Order::Shuffle, Some(42));
        assert_eq!(a, b, "same seed must produce same order");
        let c = order_tracks(tracks.clone(), Order::Shuffle, Some(43));
        assert_ne!(a, c, "different seeds should usually differ");
        // And it's a permutation (every input is present once).
        let mut sorted = a.clone();
        sorted.sort();
        let mut original = tracks.clone();
        original.sort();
        assert_eq!(sorted, original);
    }
}
