use crate::Track;
use rand::prelude::SliceRandom;
use std::path::{Path, PathBuf};

pub fn populate_queue_offline(music_dir: &Path, queue: &mut Vec<Track>, exclude_keys: &[String]) {
    if queue.len() < 3 {
        let new_songs = get_random_batch(music_dir, exclude_keys, 5);
        for song in new_songs {
            queue.push(song);
        }
    }
}

fn get_random_batch(music_dir: &Path, exclude_keys: &[String], count: usize) -> Vec<Track> {
    let all = get_all_songs(music_dir);
    let mut rng = rand::rng();

    let candidates: Vec<PathBuf> = all
        .iter()
        // A local track's key comes only from its path, so filter on that directly. Building a
        // full Track here meant lofty re-parsing the tags of every file in the library on every
        // queue top-up, and then parsing the chosen ones a second time below.
        .filter(|p| !exclude_keys.contains(&crate::track_identity::path_key(p)))
        .cloned()
        .collect();

    let mut pool = if candidates.is_empty() {
        all
    } else {
        candidates
    };
    pool.shuffle(&mut rng);

    pool.into_iter().take(count).map(path_to_track).collect()
}

fn get_all_songs(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            // Accepts every container the cache can hold, not just opus/flac. A track saved as .m4a
            // (YouTube sometimes serves AAC) was written into the music dir and then ignored by the
            // offline library, so it looked like it had never been saved at all.
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_ascii_lowercase())
                        .is_some_and(|e| {
                            crate::player::CACHED_AUDIO_EXTENSIONS.contains(&e.as_str())
                        })
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

use lofty::prelude::*;

fn path_to_track(path: PathBuf) -> Track {
    let url = path.to_string_lossy().to_string();

    let mut title = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut artists = vec!["Unknown".to_string()];
    let mut album = "Offline Library".to_string();
    let mut duration = "0:00".to_string();

    if let Ok(tagged_file) = lofty::read_from_path(&path) {
        // Duration comes from the decoded stream properties, not from a tag, so read it whether or
        // not the file carries tags — nesting it inside the tag check left every untagged file
        // reporting "0:00", which also poisoned the lyric and lossless-match lookups.
        let total_seconds = tagged_file.properties().duration().as_secs();
        duration = format!("{}:{:02}", total_seconds / 60, total_seconds % 60);

        if let Some(tag) = tagged_file.primary_tag() {
            if let Some(t) = tag.title() {
                title = t.to_string();
            }

            if let Some(a) = tag.artist() {
                artists = a.split(", ").map(|s| s.to_string()).collect();
            }

            if let Some(al) = tag.album() {
                album = al.to_string();
            }
        }
    }

    Track::new(title, artists, album, duration, None, None, url)
}

pub fn get_excluded_track_keys() -> Vec<String> {
    let history = crate::RECENTLY_PLAYED
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let queue = crate::SONG_QUEUE.read().unwrap_or_else(|e| e.into_inner());

    history
        .iter()
        .chain(queue.iter())
        .map(crate::track_identity::track_key)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_library_ignores_directories_named_like_audio_files() {
        let dir = std::env::temp_dir().join(format!("whytui-offline-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("not-a-song.opus")).unwrap();
        std::fs::write(dir.join("song.FLAC"), b"not real audio").unwrap();

        let songs = get_all_songs(&dir);
        assert_eq!(songs, vec![dir.join("song.FLAC")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
