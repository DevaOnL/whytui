use crate::Track;

pub fn track_key(track: &Track) -> String {
    if let Some(video_id) = &track.video_id
        && !video_id.trim().is_empty()
    {
        return format!("yt:{}", video_id.trim());
    }

    if !track.url.trim().is_empty() {
        return url_key(&track.url);
    }

    format!(
        "meta:{}:{}:{}",
        normalize(&track.title),
        normalize(&track.artists.join(",")),
        normalize(&track.album)
    )
}

fn url_key(url: &str) -> String {
    format!("url:{}", url.trim())
}

/// The key `track_key` produces for a local file at `path`.
///
/// A track loaded from disk has no video id, so its identity is just its path. Exposing that
/// separately lets callers filter a directory listing without building a `Track` for each entry —
/// which, for the offline library, meant parsing every file's audio tags just to compute a key.
pub fn path_key(path: &std::path::Path) -> String {
    url_key(&path.to_string_lossy())
}

pub fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize() {
        assert_eq!(normalize("Hello World"), "hello world");
        assert_eq!(normalize("  UPPERCASE  "), "uppercase");
        assert_eq!(normalize("MiXeD CaSe"), "mixed case");
    }

    #[test]
    fn test_track_key_with_video_id() {
        let track = Track {
            title: "Song".to_string(),
            artists: vec!["Artist".to_string()],
            album: "Album".to_string(),
            duration: "3:30".to_string(),
            thumbnail_url: None,
            video_id: Some("abc123".to_string()),
            url: "https://example.com".to_string(),
            playback_context: None,
        };
        assert_eq!(track_key(&track), "yt:abc123");
    }

    #[test]
    fn test_track_key_with_url_fallback() {
        let track = Track {
            title: "Song".to_string(),
            artists: vec!["Artist".to_string()],
            album: "Album".to_string(),
            duration: "3:30".to_string(),
            thumbnail_url: None,
            video_id: None,
            url: "https://example.com/stream".to_string(),
            playback_context: None,
        };
        assert_eq!(track_key(&track), "url:https://example.com/stream");
    }

    #[test]
    fn path_key_agrees_with_track_key_for_a_local_file() {
        // offline.rs filters on path_key but plays tracks keyed by track_key; if these ever
        // diverge, the "don't repeat recent songs" exclusion silently stops working
        let path = std::path::Path::new("/music/whytui/Song - Artist [abc123].opus");
        let track = Track {
            title: "Song".to_string(),
            artists: vec!["Artist".to_string()],
            album: "Offline Library".to_string(),
            duration: "3:30".to_string(),
            thumbnail_url: None,
            video_id: None,
            url: path.to_string_lossy().to_string(),
            playback_context: None,
        };
        assert_eq!(path_key(path), track_key(&track));
    }

    #[test]
    fn test_track_key_with_metadata_fallback() {
        let track = Track {
            title: "Song Title".to_string(),
            artists: vec!["Artist One".to_string(), "Artist Two".to_string()],
            album: "Album Name".to_string(),
            duration: "3:30".to_string(),
            thumbnail_url: None,
            video_id: None,
            url: "".to_string(),
            playback_context: None,
        };
        let key = track_key(&track);
        assert!(key.starts_with("meta:"));
        assert!(key.contains("song title"));
        assert!(key.contains("artist one,artist two"));
        assert!(key.contains("album name"));
    }
}
