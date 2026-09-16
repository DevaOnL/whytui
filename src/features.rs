use crate::Track;
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

#[derive(Debug, Clone)]
pub struct LrcLine {
    pub timestamp: Duration,
    pub text: String,
    pub translation: Option<String>,
    pub romanized: Option<String>,
}

impl LrcLine {
    /// Text for one explicitly selected sheet. Missing variants stay missing rather than silently
    /// turning into the original line, which would produce a mixed-language lyric sheet.
    pub fn text_for_mode(&self, mode: u8) -> Option<&str> {
        match mode {
            0 => Some(&self.text),
            1 => self.romanized.as_deref(),
            2 => self.translation.as_deref(),
            _ => Some(&self.text),
        }
    }
}

fn normalize_metadata(value: &str) -> String {
    let mut normalized = String::new();
    let mut pending_space = false;
    let mut base_is_latin = None;

    for character in value.nfkd() {
        if is_combining_mark(character) {
            if matches!(base_is_latin, Some(false)) {
                normalized.push(character);
            }
            continue;
        }

        if character.is_alphanumeric() {
            for lowercase in character.to_lowercase() {
                if pending_space && !normalized.is_empty() {
                    normalized.push(' ');
                }
                normalized.push(lowercase);
                pending_space = false;
            }
            base_is_latin = Some(is_latin_character(character));
        } else {
            base_is_latin = None;
            if character != '\'' && character != '’' {
                pending_space = true;
            }
        }
    }

    normalized
}

fn is_latin_character(character: char) -> bool {
    matches!(
        character as u32,
        0x0041..=0x005A | 0x0061..=0x007A | 0x00C0..=0x024F | 0x1E00..=0x1EFF
    )
}

/// Upstream's important Japanese-title path: a mixed title such as
/// `Shinunoga E-Wa 死ぬのがいいわ` also gets queried by its Latin catalog alias.
fn mixed_script_ascii_alias(title: &str) -> Option<String> {
    let has_non_latin_script = title
        .chars()
        .any(|c| c.is_alphabetic() && !is_latin_character(c));
    // A year or track number is not a usable Latin title ("踊り子 2021" must not create "2021").
    let has_latin_identity = title
        .chars()
        .any(|c| c.is_alphabetic() && is_latin_character(c));
    if !has_non_latin_script || !has_latin_identity {
        return None;
    }

    let latin_only: String = title
        .nfc()
        .map(|c| {
            if is_latin_character(c) || (c.is_ascii() && !c.is_alphabetic()) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let alias = latin_only
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string();
    (!alias.is_empty()).then_some(alias)
}

fn mixed_script_native_alias(title: &str) -> Option<String> {
    let has_non_latin_script = title
        .chars()
        .any(|c| c.is_alphabetic() && !is_latin_character(c));
    let has_latin_identity = title
        .chars()
        .any(|c| c.is_alphabetic() && is_latin_character(c));
    if !has_non_latin_script || !has_latin_identity {
        return None;
    }

    let native_only: String = title
        .nfc()
        .map(|c| {
            if is_latin_character(c) || c.is_ascii_alphanumeric() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let alias = native_only
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string();
    (!alias.is_empty()).then_some(alias)
}

fn strip_presentation_suffix(title: &str) -> Option<String> {
    fn is_presentation_label(label: &str) -> bool {
        matches!(
            normalize_metadata(label).as_str(),
            "official audio"
                | "official video"
                | "official music video"
                | "official lyric video"
                | "lyric video"
                | "lyrics"
                | "visualizer"
        )
    }

    let title = title.trim();
    for (open, close) in [('(', ')'), ('[', ']')] {
        if title.ends_with(close)
            && let Some(start) = title.rfind(open)
            && is_presentation_label(
                &title[start + open.len_utf8()..title.len() - close.len_utf8()],
            )
        {
            let base = title[..start]
                .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '|' | '_'));
            if !base.is_empty() {
                return Some(base.to_string());
            }
        }
    }

    for separator in [" - ", " | ", " _ "] {
        if let Some((base, label)) = title.rsplit_once(separator)
            && is_presentation_label(label)
            && !base.trim().is_empty()
        {
            return Some(base.trim().to_string());
        }
    }
    None
}

fn push_identity(identities: &mut Vec<String>, identity: String) {
    let normalized = normalize_metadata(&identity);
    if !normalized.is_empty()
        && !identities
            .iter()
            .any(|existing| normalize_metadata(existing) == normalized)
    {
        identities.push(identity);
    }
}

fn title_identities(title: &str) -> Vec<String> {
    let mut identities = Vec::new();
    let title = title.trim();
    if title.is_empty()
        || matches!(
            normalize_metadata(title).as_str(),
            "unknown" | "nothing playing"
        )
    {
        return identities;
    }

    push_identity(&mut identities, title.to_string());
    let cleaned = strip_presentation_suffix(title);
    if let Some(clean) = cleaned.as_ref() {
        push_identity(&mut identities, clean.clone());
    }
    let alias_source = cleaned.as_deref().unwrap_or(title);
    if let Some(alias) = mixed_script_native_alias(alias_source) {
        push_identity(&mut identities, alias);
    }
    if let Some(alias) = mixed_script_ascii_alias(alias_source) {
        push_identity(&mut identities, alias);
    }
    identities
}

fn reliable_artist(artist: &str) -> bool {
    !matches!(
        normalize_metadata(artist).as_str(),
        "" | "unknown" | "various artists"
    ) && artist.trim() != "~"
}

fn reliable_album(album: &str) -> bool {
    !matches!(
        normalize_metadata(album).as_str(),
        "" | "unknown" | "single" | "offline library"
    )
}

fn duration_to_seconds(duration: &str) -> Option<f64> {
    let parts: Vec<_> = duration.trim().split(':').collect();
    let total = match parts.as_slice() {
        [minutes, seconds] => {
            let minutes: u64 = minutes.parse().ok()?;
            let seconds: f64 = seconds.parse().ok()?;
            if !(0.0..60.0).contains(&seconds) {
                return None;
            }
            minutes as f64 * 60.0 + seconds
        }
        [hours, minutes, seconds] => {
            let hours: u64 = hours.parse().ok()?;
            let minutes: u64 = minutes.parse().ok()?;
            let seconds: f64 = seconds.parse().ok()?;
            if minutes >= 60 || !(0.0..60.0).contains(&seconds) {
                return None;
            }
            hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds
        }
        _ => return None,
    };

    (total.is_finite() && (1.0..=43_200.0).contains(&total)).then_some(total)
}

fn duration_parameter(duration: Option<f64>) -> Option<String> {
    duration.map(|seconds| {
        if seconds.fract() == 0.0 {
            (seconds as u64).to_string()
        } else {
            seconds.to_string()
        }
    })
}

fn lrclib_get_url(
    track_name: &str,
    artist: &str,
    album: Option<&str>,
    duration: Option<f64>,
) -> String {
    let mut url = format!(
        "https://lrclib.net/api/get?track_name={}&artist_name={}",
        urlencoding::encode(track_name),
        urlencoding::encode(artist),
    );

    if let Some(album) = album {
        url.push_str(&format!("&album_name={}", urlencoding::encode(album)));
    }
    if let Some(duration) = duration_parameter(duration) {
        url.push_str(&format!("&duration={duration}"));
    }

    url
}

fn lrclib_search_url(track_name: &str, artist: &str, album: Option<&str>) -> String {
    let mut url = format!(
        "https://lrclib.net/api/search?track_name={}&artist_name={}",
        urlencoding::encode(track_name),
        urlencoding::encode(artist)
    );
    if let Some(album) = album {
        url.push_str(&format!("&album_name={}", urlencoding::encode(album)));
    }
    url
}

fn lyric_search_urls(track: &Track) -> Vec<String> {
    let titles = title_identities(&track.title);
    let artists: Vec<_> = track
        .artists
        .iter()
        .filter(|artist| reliable_artist(artist))
        .take(2)
        .map(|artist| artist.trim().to_string())
        .collect();
    let album = reliable_album(&track.album).then_some(track.album.trim());
    let duration = duration_to_seconds(&track.duration);

    // Without duration or album there is no evidence to distinguish studio, live, remix and cover
    // results. Returning unavailable is safer than confidently displaying a different recording.
    if titles.is_empty() || artists.is_empty() || (duration.is_none() && album.is_none()) {
        return Vec::new();
    }

    let mut urls = Vec::new();
    let mut push_url = |url: String| {
        if !urls.contains(&url) {
            urls.push(url);
        }
    };

    // Exact, duration-aware lookups retain upstream's first-two-artists strategy. Full/native title,
    // safe presentation title and mixed-script Latin alias are all tried before broad search.
    for artist in &artists {
        for title in &titles {
            push_url(lrclib_get_url(title, artist, album, duration));
        }
    }
    for artist in &artists {
        for title in &titles {
            push_url(lrclib_search_url(title, artist, album));
        }
    }
    // Album metadata is often a compilation or YouTube placeholder. Only after all album-scoped
    // exact and search attempts miss do we allow duration-verified album mismatches.
    if album.is_some() && duration.is_some() {
        for artist in &artists {
            for title in &titles {
                push_url(lrclib_get_url(title, artist, None, duration));
            }
        }
        for artist in &artists {
            for title in &titles {
                push_url(lrclib_search_url(title, artist, None));
            }
        }
    }

    urls
}

struct MatchContext {
    titles: Vec<String>,
    individual_artists: Vec<String>,
    aggregate_artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
}

impl MatchContext {
    fn new(track: &Track) -> Self {
        let titles = title_identities(&track.title)
            .into_iter()
            .map(|title| normalize_metadata(&title))
            .collect();
        let individual_artists: Vec<String> = track
            .artists
            .iter()
            .filter(|artist| reliable_artist(artist))
            .map(|artist| normalize_metadata(artist))
            .collect();
        let aggregate_artist = (individual_artists.len() > 1).then(|| {
            normalize_metadata(
                &track
                    .artists
                    .iter()
                    .filter(|artist| reliable_artist(artist))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        });

        Self {
            titles,
            individual_artists,
            aggregate_artist,
            album: reliable_album(&track.album).then(|| normalize_metadata(&track.album)),
            duration: duration_to_seconds(&track.duration),
        }
    }
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CandidateRank {
    album_mismatch: bool,
    duration_delta_ms: u64,
    title: usize,
    artist: usize,
    sparse_lyrics: usize,
    id: u64,
}

fn normalize_artist_credit(artist: &str) -> String {
    normalize_metadata(artist)
        .split_whitespace()
        .filter(|word| !matches!(*word, "feat" | "featuring" | "ft" | "with"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn candidate_match(value: &Value, context: &MatchContext) -> Option<(CandidateRank, Vec<LrcLine>)> {
    if value
        .get("instrumental")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let title = normalize_metadata(value.get("trackName")?.as_str()?);
    let artist = normalize_metadata(value.get("artistName")?.as_str()?);
    let title_rank = context
        .titles
        .iter()
        .position(|expected| expected == &title)?;
    let candidate_duration = value.get("duration").and_then(Value::as_f64);
    let duration_delta = match context.duration {
        Some(expected) => {
            let candidate = candidate_duration.filter(|d| d.is_finite() && *d > 0.0)?;
            let delta = (candidate - expected).abs();
            if delta > 2.0 {
                return None;
            }
            delta
        }
        None => 0.0,
    };

    let candidate_album = value
        .get("albumName")
        .and_then(Value::as_str)
        .map(normalize_metadata);
    let album_matches = context
        .album
        .as_ref()
        .zip(candidate_album.as_ref())
        .map(|(expected, candidate)| expected == candidate)
        .unwrap_or(false);
    if context.duration.is_none() && !album_matches {
        return None;
    }

    let artist_rank = if let Some(aggregate) = context.aggregate_artist.as_ref() {
        if aggregate == &artist || aggregate == &normalize_artist_credit(&artist) {
            Some(0)
        } else {
            // LRCLIB often credits only the primary artist even when YouTube lists every
            // collaborator. Accept that narrower credit only when album and duration independently
            // identify the same recording; otherwise it could be a solo or cover version.
            (album_matches && context.duration.is_some())
                .then(|| {
                    context
                        .individual_artists
                        .iter()
                        .position(|expected| expected == &artist)
                        .map(|rank| rank + 1)
                })
                .flatten()
        }
    } else {
        context
            .individual_artists
            .iter()
            .position(|expected| expected == &artist)
    }?;

    let synced = value.get("syncedLyrics")?.as_str()?.trim();
    if synced.is_empty() {
        return None;
    }
    let lines = parse_lrc(synced);
    let nonblank_lines = lines
        .iter()
        .filter(|line| !line.text.trim().is_empty())
        .count();
    if nonblank_lines == 0 {
        return None;
    }

    if let Some(duration) = context.duration.or(candidate_duration)
        && lines
            .iter()
            .map(|line| line.timestamp.as_secs_f64())
            .fold(0.0, f64::max)
            > duration + 10.0
    {
        return None;
    }

    let rank = CandidateRank {
        album_mismatch: context.album.is_some() && !album_matches,
        duration_delta_ms: (duration_delta * 1000.0).round() as u64,
        title: title_rank,
        artist: artist_rank,
        sparse_lyrics: usize::MAX.saturating_sub(nonblank_lines),
        id: value.get("id").and_then(Value::as_u64).unwrap_or(u64::MAX),
    };
    Some((rank, lines))
}

#[cfg(test)]
fn select_search_candidate(values: &[Value], context: &MatchContext) -> Option<Vec<LrcLine>> {
    values
        .iter()
        .filter_map(|value| candidate_match(value, context))
        .min_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, lines)| lines)
}

fn retain_better_candidate(
    best: &mut Option<(CandidateRank, Vec<LrcLine>)>,
    candidate: (CandidateRank, Vec<LrcLine>),
) {
    if best.as_ref().is_none_or(|(rank, _)| candidate.0 < *rank) {
        *best = Some(candidate);
    }
}

pub async fn fetch_synced_lyrics(
    track: &Track,
) -> Result<Vec<LrcLine>, Box<dyn std::error::Error + Send + Sync>> {
    // without a timeout an unresponsive lrclib/translate host leaves this task hanging forever
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| Client::new());
    let context = MatchContext::new(track);
    let mut best = None;
    let lookup = async {
        let mut urls = lyric_search_urls(track).into_iter();
        let mut requests = tokio::task::JoinSet::new();
        const CONCURRENCY: usize = 6;

        let spawn_request = |requests: &mut tokio::task::JoinSet<Option<Value>>, url: String| {
            let client = client.clone();
            requests.spawn(async move {
                let response = client.get(url).send().await.ok()?;
                response
                    .status()
                    .is_success()
                    .then_some(response)?
                    .json::<Value>()
                    .await
                    .ok()
            });
        };

        for url in urls.by_ref().take(CONCURRENCY) {
            spawn_request(&mut requests, url);
        }

        while let Some(result) = requests.join_next().await {
            if let Some(url) = urls.next() {
                spawn_request(&mut requests, url);
            }
            let Ok(Some(json)) = result else { continue };
            if let Some(arr) = json.as_array() {
                for candidate in arr
                    .iter()
                    .filter_map(|value| candidate_match(value, &context))
                {
                    retain_better_candidate(&mut best, candidate);
                }
            } else if let Some(candidate) = candidate_match(&json, &context) {
                // Parse and validate exact responses too. A non-empty but malformed syncedLyrics
                // field must not suppress later aliases and search fallbacks.
                retain_better_candidate(&mut best, candidate);
            }
        }
    };

    let _ = tokio::time::timeout(Duration::from_secs(25), lookup).await;
    if let Some((_, lines)) = best {
        return Ok(lines);
    }

    Err("No synced lyrics available".into())
}

/// Fill in the English `translation` and the source-language `romanized` (romaji) for each lyric line,
/// when Google provides them.
///
/// One Google request per line, deliberately. The previous version joined every line into a single
/// query with " / " separators and split the response back apart by index — but Google re-segments
/// sentences and returns the romaji as one combined blob, so the per-line counts almost never matched
/// and the code then dropped BOTH the translation and the romaji entirely (only applying them when the
/// count matched exactly). That is why a Japanese song showed its original lyrics fine but the [t]
/// toggle produced nothing. A request per line makes each result line up with exactly one lyric;
/// a bounded number run at once so a whole song still resolves in a few seconds without Google
/// answering 429 to the burst.
pub async fn translate_lines(lines: &mut [LrcLine]) {
    if lines.is_empty() {
        return;
    }

    for line in lines.iter_mut().filter(|line| !line.text.trim().is_empty()) {
        if !needs_romanization(&line.text) {
            line.romanized = Some(line.text.clone());
        }
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| Client::new());

    // Only lines that actually carry text; blank spacer lines are left alone.
    let jobs: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.text.trim().is_empty())
        .map(|(i, l)| (i, l.text.clone()))
        .collect();

    // Keep the fan-out modest so the burst does not get rate-limited.
    const CONCURRENCY: usize = 6;

    for batch in jobs.chunks(CONCURRENCY) {
        let mut set = tokio::task::JoinSet::new();
        for (idx, text) in batch {
            let client = client.clone();
            let text = text.clone();
            let idx = *idx;
            set.spawn(async move { (idx, translate_one_line(&client, &text).await) });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok((idx, (translation, romanized))) = joined {
                if let Some(t) = translation {
                    lines[idx].translation = Some(t);
                }
                if let Some(r) = romanized {
                    lines[idx].romanized = Some(r);
                }
            }
        }
    }
}

fn needs_romanization(text: &str) -> bool {
    text.chars().any(|character| {
        character.is_alphabetic()
            && !matches!(
                character as u32,
                0x0041..=0x007A | 0x00C0..=0x024F | 0x1E00..=0x1EFF
            )
    })
}

/// Translate one lyric line to English and get its source romanization in the same call.
/// Returns `(translation, romaji)`, each `None` if the request failed or the field was absent.
async fn translate_one_line(client: &Client, text: &str) -> (Option<String>, Option<String>) {
    let url = "https://translate.googleapis.com/translate_a/single";
    let params = [
        // `gtx` is routinely answered with Google's automated-query HTML block instead of JSON.
        // This supported client returns the same translation/romanization shape without that block.
        ("client", "dict-chrome-ex"),
        ("sl", "auto"),
        ("tl", "en"),
        ("dt", "t"),  // translation
        ("dt", "rm"), // romanization (transliteration of the source)
        ("q", text),
    ];

    let resp = match client.get(url).query(&params).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return (None, None),
    };
    match resp.json::<Value>().await {
        Ok(json) => parse_translate_response(&json),
        Err(_) => (None, None),
    }
}

/// Extract `(translation, romaji)` from a `translate_a/single` response.
///
/// `data[0]` is an array of chunks. Translation chunks hold the translated text at index 0 (and the
/// original at 1). When `dt=rm` is requested for a non-Latin source, a final chunk is appended whose
/// index 0 is null and whose index 3 is the source romaji, e.g.
/// `[[["I love you","愛してる",null,null,3],[null,null,null,"Aishiteru"]],null,"ja"]`.
fn parse_translate_response(json: &Value) -> (Option<String>, Option<String>) {
    let Some(chunks) = json.get(0).and_then(|v| v.as_array()) else {
        return (None, None);
    };

    let mut translation = String::new();
    for chunk in chunks {
        if let Some(t) = chunk.get(0).and_then(|v| v.as_str()) {
            translation.push_str(t);
        }
    }
    let translation = {
        let t = translation.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    };

    // The romaji lives in the chunk whose translated-text slot (index 0) is null/absent.
    let romaji = chunks.iter().find_map(|chunk| {
        let no_translation = chunk.get(0).is_none_or(|v| v.is_null());
        chunk
            .get(3)
            .and_then(|v| v.as_str())
            .filter(|s| no_translation && !s.trim().is_empty())
            .map(|s| s.trim().to_string())
    });

    (translation, romaji)
}

pub fn parse_lrc(lrc: &str) -> Vec<LrcLine> {
    let mut lines = Vec::new();
    let mut offset_ms = 0i64;
    for line in lrc.lines() {
        // Timestamps are the *leading* run of [mm:ss(.xx)] tags; the text is whatever follows.
        // Stop at the first bracket group that is not a timestamp, so a lyric containing '[' or
        // ']' keeps all of its words: scanning the whole line and then splitting on the last ']'
        // turned "[00:12] hey [ok] there" into "there".
        let mut timestamps = Vec::new();
        let mut rest = line.strip_prefix('\u{feff}').unwrap_or(line).trim_start();

        if let Some(raw_offset) = rest
            .strip_prefix("[offset:")
            .and_then(|value| value.strip_suffix(']'))
        {
            if let Ok(parsed) = raw_offset.trim().parse::<i64>() {
                offset_ms = parsed;
            }
            continue;
        }

        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else { break };
            match parse_timestamp(&rest[1..end]) {
                Some(dur) => {
                    timestamps.push(dur);
                    rest = rest[end + 1..].trim_start();
                }
                // metadata tags like [ar:...] and stray brackets end the timestamp run
                None => break,
            }
        }

        if timestamps.is_empty() {
            continue;
        }

        let text = rest.trim().to_string();

        // Create an LrcLine for each timestamp
        for timestamp in timestamps {
            lines.push(LrcLine {
                timestamp,
                text: text.clone(),
                translation: None,
                romanized: None,
            });
        }
    }
    if offset_ms != 0 {
        let magnitude = Duration::from_millis(offset_ms.unsigned_abs());
        for line in &mut lines {
            line.timestamp = if offset_ms > 0 {
                line.timestamp.saturating_add(magnitude)
            } else {
                line.timestamp.saturating_sub(magnitude)
            };
        }
    }
    lines.sort_by_key(|l| l.timestamp);
    lines
}

fn parse_timestamp(ts: &str) -> Option<Duration> {
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let minutes: u64 = parts[0].parse().ok()?;
    let seconds: f64 = parts[1].parse().ok()?;

    // Validate seconds are in valid range (0-60)
    if !(0.0..60.0).contains(&seconds) {
        return None;
    }

    // Cap at reasonable song duration (12 hours)
    if minutes > 12 * 60 {
        return None;
    }

    let total_ms = ((minutes as f64) * 60.0 + seconds) * 1000.0;
    Some(Duration::from_millis(total_ms as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artists: &[&str], album: &str, duration: &str) -> Track {
        Track::new(
            title.to_string(),
            artists.iter().map(|artist| (*artist).to_string()).collect(),
            album.to_string(),
            duration.to_string(),
            None,
            Some("video-id".to_string()),
            "https://example.com/audio".to_string(),
        )
    }

    fn candidate(
        id: u64,
        title: &str,
        artist: &str,
        album: &str,
        duration: f64,
        lyrics: &str,
    ) -> Value {
        serde_json::json!({
            "id": id,
            "trackName": title,
            "artistName": artist,
            "albumName": album,
            "duration": duration,
            "instrumental": false,
            "syncedLyrics": lyrics,
        })
    }

    #[test]
    fn parse_translate_response_extracts_translation_and_romaji() {
        // Shape Google returns for a Japanese line with dt=t & dt=rm: a translation chunk plus a
        // final transliteration chunk whose index 0 is null and index 3 is the romaji.
        let v = serde_json::json!([
            [
                ["I love you", "愛してる", null, null, 3],
                [null, null, null, "Aishiteru"]
            ],
            null,
            "ja"
        ]);
        let (t, r) = parse_translate_response(&v);
        assert_eq!(t.as_deref(), Some("I love you"));
        assert_eq!(r.as_deref(), Some("Aishiteru"));
    }

    #[test]
    fn parse_translate_response_joins_multi_chunk_translation() {
        // Google can split one line into several translation chunks (each carries its own trailing
        // space); concatenating index 0 reconstructs the sentence, and the romaji is still the null-0
        // chunk at the end.
        let v = serde_json::json!([
            [
                ["Hello ", "こんにちは", null, null, 3],
                ["world", "世界", null, null, 3],
                [null, null, null, "Kon'nichiwa sekai"]
            ],
            null,
            "ja"
        ]);
        let (t, r) = parse_translate_response(&v);
        assert_eq!(t.as_deref(), Some("Hello world"));
        assert_eq!(r.as_deref(), Some("Kon'nichiwa sekai"));
    }

    #[test]
    fn parse_translate_response_has_no_romaji_for_latin_source() {
        // An already-Latin source produces no transliteration chunk.
        let v = serde_json::json!([[["Hello", "Hola", null, null, 3]], null, "es"]);
        let (t, r) = parse_translate_response(&v);
        assert_eq!(t.as_deref(), Some("Hello"));
        assert_eq!(r, None);
    }

    #[test]
    fn parse_translate_response_tolerates_garbage() {
        assert_eq!(
            parse_translate_response(&serde_json::json!({})),
            (None, None)
        );
        assert_eq!(
            parse_translate_response(&serde_json::json!(null)),
            (None, None)
        );
        assert_eq!(
            parse_translate_response(&serde_json::json!([])),
            (None, None)
        );
        assert_eq!(
            parse_translate_response(&serde_json::json!([null])),
            (None, None)
        );
    }

    #[test]
    fn lyric_text_never_falls_back_to_the_original_variant() {
        let line = LrcLine {
            timestamp: Duration::ZERO,
            text: "original".to_string(),
            translation: Some("translated".to_string()),
            romanized: None,
        };

        assert_eq!(line.text_for_mode(0), Some("original"));
        assert_eq!(line.text_for_mode(1), None);
        assert_eq!(line.text_for_mode(2), Some("translated"));
    }

    #[test]
    fn romanization_is_only_required_for_non_latin_script() {
        assert!(!needs_romanization("Yeah! cafe 123"));
        assert!(!needs_romanization("déjà vu"));
        assert!(needs_romanization("愛してる"));
        assert!(needs_romanization("사랑해"));
    }

    #[test]
    fn mixed_japanese_titles_keep_native_and_latin_identities() {
        assert_eq!(
            title_identities("Shinunoga E-Wa 死ぬのがいいわ"),
            vec![
                "Shinunoga E-Wa 死ぬのがいいわ",
                "死ぬのがいいわ",
                "Shinunoga E-Wa"
            ]
        );
        assert_eq!(title_identities("踊り子"), vec!["踊り子"]);
        // Accented Latin text is not a second script and must not be damaged into "Beyonc".
        assert_eq!(title_identities("Beyoncé"), vec!["Beyoncé"]);
        assert_eq!(title_identities("踊り子 2021"), vec!["踊り子 2021"]);
        assert!(!title_identities("踊り子 2021").contains(&"2021".to_string()));
        assert_eq!(
            title_identities("Déjà Vu デジャヴ"),
            vec!["Déjà Vu デジャヴ", "デジャヴ", "Déjà Vu"]
        );
        assert_eq!(
            title_identities("Cafe\u{301} 日本語"),
            vec!["Cafe\u{301} 日本語", "日本語", "Café"]
        );
    }

    #[test]
    fn normalization_preserves_non_latin_combining_marks() {
        assert_ne!(normalize_metadata("が"), normalize_metadata("か"));
        assert_eq!(normalize_metadata("Beyoncé"), normalize_metadata("Beyonce"));
    }

    #[test]
    fn only_presentation_suffixes_are_removed_from_titles() {
        assert_eq!(
            title_identities("Idol (Official Music Video)"),
            vec!["Idol (Official Music Video)", "Idol"]
        );
        assert_eq!(title_identities("Idol (Live)"), vec!["Idol (Live)"]);
        assert_eq!(title_identities("Idol - Remix"), vec!["Idol - Remix"]);
    }

    #[test]
    fn duration_metadata_must_be_valid_and_nonzero() {
        assert_eq!(duration_to_seconds("3:21"), Some(201.0));
        assert_eq!(duration_to_seconds("0:59:59"), Some(3599.0));
        assert_eq!(duration_to_seconds("1:05:00"), Some(3900.0));
        for invalid in ["0:00", "12:00:01", "1:60", "1:99:00", "garbage", ""] {
            assert_eq!(duration_to_seconds(invalid), None, "accepted {invalid:?}");
        }
    }

    #[test]
    fn exact_lookups_precede_search_and_keep_full_artist_names() {
        let urls = lyric_search_urls(&track(
            "Song Name",
            &["Taylor Swift", "Kendrick Lamar"],
            "Album Name",
            "3:21",
        ));
        let first_search = urls
            .iter()
            .position(|url| url.contains("/search?"))
            .expect("search fallback");
        assert!(urls[..first_search].iter().all(|url| url.contains("/get?")));
        assert!(
            urls.iter()
                .any(|url| url.contains("artist_name=Taylor%20Swift"))
        );
        assert!(
            urls.iter()
                .any(|url| url.contains("artist_name=Kendrick%20Lamar"))
        );
        assert!(!urls.iter().any(|url| url.contains("artist_name=Taylor&")));
    }

    #[test]
    fn synthetic_metadata_does_not_constrain_lrclib() {
        let urls = lyric_search_urls(&track("Song", &["Artist"], "Single", "3:21"));
        assert!(!urls.is_empty());
        assert!(urls.iter().all(|url| !url.contains("album_name=")));

        let unverifiable =
            lyric_search_urls(&track("Song", &["Artist"], "Offline Library", "0:00"));
        assert!(unverifiable.is_empty());
    }

    #[test]
    fn search_skips_a_wrong_first_result() {
        let source = track("踊り子", &["Vaundy"], "replica", "3:50");
        let context = MatchContext::new(&source);
        let results = vec![
            candidate(
                1,
                "踊り子",
                "Cover Band",
                "replica",
                230.0,
                "[00:10] wrong\n[00:20] result",
            ),
            candidate(
                2,
                "踊り子",
                "Vaundy",
                "replica",
                230.0,
                "[00:10] right\n[00:20] result",
            ),
        ];

        let selected = select_search_candidate(&results, &context).expect("correct candidate");
        assert_eq!(selected[0].text, "right");
    }

    #[test]
    fn search_uses_album_then_duration_to_rank_duplicates() {
        let source = track("踊り子", &["Vaundy"], "replica", "3:50");
        let context = MatchContext::new(&source);
        let results = vec![
            candidate(
                1,
                "踊り子",
                "Vaundy",
                "Compilation",
                230.0,
                "[00:10] compilation\n[00:20] lyrics",
            ),
            candidate(
                2,
                "踊り子",
                "Vaundy",
                "replica",
                231.0,
                "[00:10] album\n[00:20] match",
            ),
        ];

        let selected = select_search_candidate(&results, &context).expect("ranked candidate");
        assert_eq!(selected[0].text, "album");
    }

    #[test]
    fn search_rejects_wrong_versions_and_durations() {
        let source = track("Song (Live)", &["Artist"], "Live Album", "4:00");
        let context = MatchContext::new(&source);
        let results = vec![
            candidate(
                1,
                "Song",
                "Artist",
                "Live Album",
                240.0,
                "[00:10] studio\n[00:20] version",
            ),
            candidate(
                2,
                "Song (Live)",
                "Artist",
                "Live Album",
                230.0,
                "[00:10] wrong\n[00:20] duration",
            ),
        ];
        assert!(select_search_candidate(&results, &context).is_none());
    }

    #[test]
    fn malformed_lrc_falls_through_to_the_next_candidate() {
        let source = track("Song", &["Artist"], "Album", "1:40");
        let context = MatchContext::new(&source);
        let results = vec![
            candidate(1, "Song", "Artist", "Album", 100.0, "not an lrc"),
            candidate(
                2,
                "Song",
                "Artist",
                "Album",
                100.0,
                "[00:10] usable\n[00:20] lyrics",
            ),
        ];
        let selected = select_search_candidate(&results, &context).expect("second candidate");
        assert_eq!(selected[0].text, "usable");
    }

    #[test]
    fn mixed_script_source_accepts_its_latin_catalog_alias() {
        let source = track(
            "Shinunoga E-Wa 死ぬのがいいわ",
            &["Fujii Kaze"],
            "HELP EVER HURT NEVER",
            "3:05",
        );
        let context = MatchContext::new(&source);
        let results = vec![candidate(
            1,
            "Shinunoga E-Wa",
            "Fujii Kaze",
            "HELP EVER HURT NEVER",
            185.5,
            "[00:10] one\n[00:20] two",
        )];
        assert!(select_search_candidate(&results, &context).is_some());
    }

    #[test]
    fn candidate_matching_folds_diacritics_like_lrclib() {
        let source = track("Déjà Vu", &["Beyoncé"], "Album", "3:20");
        let context = MatchContext::new(&source);
        let results = vec![candidate(
            1,
            "Deja Vu",
            "Beyonce",
            "Album",
            200.0,
            "[00:10] one\n[00:20] two",
        )];
        assert!(select_search_candidate(&results, &context).is_some());
    }

    #[test]
    fn aggregate_collaboration_credit_beats_strongly_verified_single_artist_credit() {
        let source = track("Song", &["Artist A", "Artist B"], "Album", "3:20");
        let context = MatchContext::new(&source);
        assert!(
            candidate_match(
                &candidate(
                    0,
                    "Song",
                    "Artist A",
                    "Album",
                    200.0,
                    "[00:10] solo\n[00:20] lyrics",
                ),
                &context,
            )
            .is_some()
        );
        let results = vec![
            candidate(
                1,
                "Song",
                "Artist A",
                "Album",
                200.0,
                "[00:10] partial credit\n[00:20] lyrics",
            ),
            candidate(
                2,
                "Song",
                "Artist A feat. Artist B",
                "Album",
                200.0,
                "[00:10] full credit\n[00:20] lyrics",
            ),
        ];
        let selected = select_search_candidate(&results, &context).expect("aggregate credit");
        assert_eq!(selected[0].text, "full credit");

        let wrong_album = candidate(
            4,
            "Song",
            "Artist A",
            "Different Album",
            200.0,
            "[00:10] unsafe fallback\n[00:20] lyrics",
        );
        assert!(candidate_match(&wrong_album, &context).is_none());

        let source = track(
            "Song",
            &["Artist A", "Artist B", "Artist C"],
            "Album",
            "3:20",
        );
        let context = MatchContext::new(&source);
        assert!(
            candidate_match(
                &candidate(
                    3,
                    "Song",
                    "Artist A, Artist B & Artist C",
                    "Album",
                    200.0,
                    "[00:10] all artists\n[00:20] lyrics",
                ),
                &context,
            )
            .is_some()
        );
    }

    #[test]
    fn candidates_are_ranked_across_separate_responses() {
        let source = track("Song", &["Artist"], "Album", "3:20");
        let context = MatchContext::new(&source);
        let mut best = None;
        retain_better_candidate(
            &mut best,
            candidate_match(
                &candidate(
                    1,
                    "Song",
                    "Artist",
                    "Compilation",
                    200.0,
                    "[00:10] fallback\n[00:20] lyrics",
                ),
                &context,
            )
            .expect("duration-verified fallback"),
        );
        retain_better_candidate(
            &mut best,
            candidate_match(
                &candidate(
                    2,
                    "Song",
                    "Artist",
                    "Album",
                    201.0,
                    "[00:10] exact album\n[00:20] lyrics",
                ),
                &context,
            )
            .expect("exact album"),
        );

        assert_eq!(best.expect("best candidate").1[0].text, "exact album");
    }

    #[test]
    fn test_parse_timestamp_valid() {
        assert_eq!(parse_timestamp("0:00"), Some(Duration::from_millis(0)));
        assert_eq!(parse_timestamp("1:30"), Some(Duration::from_millis(90000))); // 1*60 + 30 seconds
        assert_eq!(
            parse_timestamp("0:45.5"),
            Some(Duration::from_millis(45500))
        );
    }

    #[test]
    fn test_parse_timestamp_invalid() {
        assert_eq!(parse_timestamp("invalid"), None);
        assert_eq!(parse_timestamp("1:"), None);
        assert_eq!(parse_timestamp(":30"), None);
        assert_eq!(parse_timestamp("1:75"), None); // seconds > 60
        assert_eq!(parse_timestamp("1:60"), None);
        assert_eq!(parse_timestamp("1000:00"), None); // > 12 hours
    }

    #[test]
    fn test_parse_lrc_with_timestamps() {
        let lrc = "[00:30] First line\n[01:00] Second line";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "First line");
        assert_eq!(lines[1].text, "Second line");
    }

    #[test]
    fn test_parse_lrc_multiple_timestamps_same_line() {
        let lrc = "[00:10][00:20] Synchronized text";
        let lines = parse_lrc(lrc);
        assert!(lines.iter().all(|l| l.text == "Synchronized text"));
    }

    #[test]
    fn test_parse_lrc_keeps_text_containing_brackets() {
        // splitting on the last ']' used to drop everything before the bracketed aside
        let lines = parse_lrc("[00:12] hey [ok] there");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "hey [ok] there");
    }

    #[test]
    fn test_parse_lrc_skips_metadata_tags() {
        let lines = parse_lrc("[ar:Some Artist]\n[al:An Album]\n[00:05] real lyric");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "real lyric");
    }

    #[test]
    fn test_parse_lrc_handles_bom_and_global_offset() {
        let lines = parse_lrc("\u{feff}[00:01] first\n[offset:+250]\n[00:02] second");
        assert_eq!(lines.len(), 2, "parsed lines: {lines:?}");
        assert_eq!(lines[0].timestamp, Duration::from_millis(1250));
        assert_eq!(lines[1].timestamp, Duration::from_millis(2250));

        let lines = parse_lrc("[offset:-1500]\n[00:01] first\n[00:02] second");
        assert_eq!(lines[0].timestamp, Duration::ZERO);
        assert_eq!(lines[1].timestamp, Duration::from_millis(500));
    }

    #[test]
    fn test_parse_lrc_multi_timestamp_text_with_bracket() {
        let lines = parse_lrc("[00:10][00:20] la [x] la");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.text == "la [x] la"));
    }

    #[test]
    fn test_parse_lrc_invalid_timestamps_skipped() {
        let lrc = "[invalid] Line 1\n[00:30] Line 2";
        let lines = parse_lrc(lrc);
        // Only the valid timestamp should create a line
        assert_eq!(lines.iter().filter(|l| l.text == "Line 2").count(), 1);
    }

    #[test]
    fn test_lrclib_get_url_has_no_literal_braces() {
        let url = lrclib_get_url("Song Name", "Artist Name", Some("Album Name"), Some(201.0));
        assert!(!url.contains('{'));
        assert!(!url.contains('}'));
        assert!(url.starts_with("https://lrclib.net/api/get?"));
    }

    #[test]
    fn test_lrclib_get_url_encodes_spaces() {
        let url = lrclib_get_url("Song Name", "Artist Name", Some("Album Name"), Some(201.0));
        assert!(url.contains("Song%20Name"));
        assert!(url.contains("Artist%20Name"));
        assert!(url.contains("Album%20Name"));
    }

    #[test]
    fn test_lrclib_get_url_uses_album_name_parameter() {
        // lrclib ignores a plain `album=` parameter, so it has to be spelled `album_name=`
        let url = lrclib_get_url("Song", "Artist", Some("Some Album"), Some(201.0));
        assert!(url.contains("album_name=Some%20Album"), "got {}", url);
    }

    #[test]
    fn test_lrclib_get_url_can_omit_album_and_duration() {
        let url = lrclib_get_url("Song", "Artist", None, None);
        assert!(!url.contains("album"), "got {url}");
        assert!(!url.contains("duration"), "got {url}");
    }

    #[test]
    fn test_lrclib_search_url_has_no_literal_braces() {
        let url = lrclib_search_url("Song Name", "Artist Name", None);
        assert!(!url.contains('{'));
        assert!(!url.contains('}'));
        assert!(url.starts_with("https://lrclib.net/api/search?"));
    }

    #[test]
    fn test_lrclib_search_url_encodes_spaces() {
        let url = lrclib_search_url("Song Name", "Artist Name", None);
        assert!(url.contains("Song%20Name"));
        assert!(url.contains("Artist%20Name"));
    }
}
