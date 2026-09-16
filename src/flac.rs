use crate::config;
use base64::prelude::*;
use reqwest::Client;
use serde_json::Value;
use std::error::Error;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// The mirror we are currently using. Replaceable, unlike the `OnceCell` this used to be: a mirror
/// that died mid-session could never be dropped, so every remaining track paid the full 10s request
/// timeout before falling back to YouTube.
static ACTIVE_API: RwLock<Option<String>> = RwLock::new(None);
/// Where the next election starts, so a demoted mirror is not immediately re-elected.
static NEXT_CANDIDATE: AtomicUsize = AtomicUsize::new(0);
static ELECTION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Per-candidate probe timeout during an election.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How many candidates a mid-session re-election may try. Startup walks the whole list, but a song
/// start must not stall for 12 x 3s, so it takes a couple of steps down the list per track instead.
const MID_SESSION_PROBE_LIMIT: usize = 2;

fn active_api() -> Option<String> {
    ACTIVE_API.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Drop the current mirror and make sure the next election starts past it.
fn demote_active_api(failed_api: &str) {
    let mut guard = ACTIVE_API.write().unwrap_or_else(|e| e.into_inner());
    if guard.as_deref() == Some(failed_api) {
        *guard = None;
        NEXT_CANDIDATE.fetch_add(1, Ordering::Relaxed);
    }
}

/// GET `url` and decode JSON, demoting the active mirror on ANY failure — transport error, a non-2xx
/// status, or a body that will not parse as JSON.
///
/// Demoting only on transport errors (the previous behaviour) meant a mirror that answered HTTP 5xx
/// or served an HTML maintenance page was kept as the active one: `send()` returns `Ok` for a 5xx,
/// then `resp.json()` failed and the error propagated without dropping the mirror, so the very same
/// dead mirror was re-selected for every remaining track and lossless was silently off for the whole
/// session even though other healthy mirrors existed.
async fn fetch_json(client: &Client, url: &str) -> Result<Value, Box<dyn Error + Send + Sync>> {
    let failed_api = API_CANDIDATES
        .iter()
        .map(|candidate| candidate.trim_end_matches('/'))
        .find(|candidate| url.starts_with(candidate));
    let demote = || {
        if let Some(api) = failed_api {
            demote_active_api(api);
        }
    };
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                demote();
                return Err(format!("mirror returned HTTP {}", status.as_u16()).into());
            }
            match resp.json::<Value>().await {
                Ok(v) => Ok(v),
                Err(e) => {
                    demote();
                    Err(e.into())
                }
            }
        }
        // A transport error means this mirror is unreachable, not that the track is missing. Drop it
        // so the next track does not pay the same timeout all over again.
        Err(e) => {
            demote();
            Err(e.into())
        }
    }
}

/// Probe up to `limit` candidates starting at the rotating cursor; store and return the first that
/// answers.
async fn elect_api(limit: usize) -> Option<String> {
    let _election = ELECTION_LOCK.lock().await;
    if let Some(existing) = active_api() {
        return Some(existing);
    }
    let client = Client::builder()
        .user_agent(UA)
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;

    let total = API_CANDIDATES.len();
    let start_at = NEXT_CANDIDATE.load(Ordering::Relaxed);

    for step in 0..limit.min(total) {
        let idx = (start_at + step) % total;
        // Every call site builds "{base}/path", so a candidate written with a trailing slash
        // (api.monochrome.tf/) would produce "//path" on every request once selected.
        let url = API_CANDIDATES[idx].trim_end_matches('/');

        if let Ok(resp) = client
            .get(format!("{}/track/?id=204567804", url))
            .send()
            .await
            && resp.status().is_success()
        {
            *ACTIVE_API.write().unwrap_or_else(|e| e.into_inner()) = Some(url.to_string());
            NEXT_CANDIDATE.store(idx, Ordering::Relaxed);
            return Some(url.to_string());
        }
    }

    // remember where to resume so the next attempt does not re-probe the same dead mirrors
    NEXT_CANDIDATE.store((start_at + limit.min(total)) % total, Ordering::Relaxed);
    None
}

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const API_CANDIDATES: &[&str] = &[
    "https://arran.monochrome.tf",
    "https://api.monochrome.tf/",
    "https://triton.squid.wtf",
    "https://wolf.qqdl.site",
    "https://maus.qqdl.site",
    "https://vogel.qqdl.site",
    "https://katze.qqdl.site",
    "https://hund.qqdl.site",
    "https://tidal.kinoplus.online",
    "https://tidal-api.binimum.org",
    "https://hifi-one.spotisaver.net",
    "https://hifi-two.spotisaver.net",
];

/// Elect a mirror at startup, walking the whole candidate list. Returns the chosen base URL.
///
/// Prints nothing: this can now also run mid-session (see `fetch_flac_stream_url`), and by then the
/// terminal is in raw mode with the TUI drawn over it, so a stray println would tear the frame.
pub async fn init_api() -> Result<String, Box<dyn Error + Send + Sync>> {
    if let Some(existing) = active_api() {
        return Ok(existing);
    }

    elect_api(API_CANDIDATES.len())
        .await
        .ok_or_else(|| "No working API servers found".into())
}

pub async fn fetch_flac_stream_url(
    title: &str,
    artists: &[String],
    target_duration: &str,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    // If the mirror was demoted after a failure, take a couple of bounded steps down the candidate
    // list to find a live one instead of giving up on lossless for the rest of the session.
    let api_base = match active_api() {
        Some(base) => base,
        None => elect_api(MID_SESSION_PROBE_LIMIT)
            .await
            .ok_or("No reachable lossless mirror")?,
    };

    // these are third-party mirrors and do go unresponsive; without a timeout the caller (song
    // start, or the autoplay prefetch) blocks indefinitely instead of falling back to YouTube
    let client = Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .build()?;
    let target_secs = parse_to_seconds(target_duration);

    let query = format!("{} {}", title, artists.join(" "));
    let search_url = format!("{}/search/?s={}", api_base, urlencoding::encode(&query));
    let search_resp: Value = fetch_json(&client, &search_url).await?;

    let items = match search_resp["data"]["items"].as_array() {
        Some(items) => items,
        None => {
            demote_active_api(&api_base);
            return Err("Invalid search response format".into());
        }
    };

    if items.is_empty() {
        return Err("No search results found".into());
    }

    let selected_item = select_candidate(items, title, artists, target_secs);

    let item = selected_item.ok_or("No results matched the duration criteria")?;
    let track_id = item["id"].as_i64().ok_or("Selected result has no ID")?;

    let quality = if config().peak_lossless_mode {
        "HI_RES_LOSSLESS"
    } else {
        "LOSSLESS"
    };

    let track_url = format!("{}/track/?id={}&quality={}", api_base, track_id, quality);

    let track_data: Value = fetch_json(&client, &track_url).await?;

    let data = if track_data.get("data").is_some() {
        &track_data["data"]
    } else {
        &track_data
    };

    if let Some(stream_url) = direct_stream_url(data) {
        return Ok(stream_url.to_string());
    }

    if let Some(manifest) = data["manifest"].as_str() {
        return decode_manifest(manifest, track_id);
    }

    if track_data.get("error").is_some() {
        demote_active_api(&api_base);
    }

    Err("No lossless for this track".into())
}

/// Words too generic to identify a song. Without this, "the" alone was enough to make two unrelated
/// titles look like a match.
const GENERIC_WORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "you",
    "with",
    "from",
    "that",
    "this",
    "feat",
    "featuring",
    "remix",
    "version",
    "official",
    "audio",
    "video",
    "live",
    "remaster",
    "remastered",
    "edit",
    "mix",
];

/// Significant words of `s`, lowercased: long enough to matter and not in [`GENERIC_WORDS`].
fn significant_words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 2 && !GENERIC_WORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Deliberately weak sanity check that a mirror result is the song that was asked for.
///
/// Matching was on duration alone, so any track in the catalogue that happened to be the same length
/// was accepted and served as the "lossless" version of the request — occasionally a completely
/// unrelated song. This only rejects a candidate when it shares *no* significant word with the title,
/// so a title spelled differently between YouTube and the mirror still matches; the point is to catch
/// the coincidental-duration case, not to be a scoring function.
fn shares_a_word_with_title(title: &str, item: &Value) -> bool {
    let item_title = item["title"]
        .as_str()
        .or_else(|| item["name"].as_str())
        .unwrap_or("");
    if item_title.is_empty() {
        // nothing to compare against; leave the duration match as the only criterion
        return true;
    }

    let wanted = significant_words(title);
    if wanted.is_empty() {
        return true;
    }
    let have = significant_words(item_title);

    wanted.iter().any(|w| have.contains(w))
}

fn normalize_artist_name(artist: &str) -> String {
    artist
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_artist_part(parts: &mut Vec<String>, words: &mut Vec<String>) {
    if !words.is_empty() {
        parts.push(words.join(" "));
        words.clear();
    }
}

/// Return both the complete credit and its explicitly separated artists. Exact normalized equality
/// is used later, so `Ann` cannot match `Joanne` and `Artist A` cannot match `Artist AB`.
fn artist_credit_names(credit: &str) -> Vec<String> {
    let whole = normalize_artist_name(credit);
    if whole.is_empty() {
        return Vec::new();
    }

    let mut separated = String::with_capacity(credit.len());
    for character in credit.chars() {
        if matches!(character, '&' | ',' | ';' | '/' | '+' | '|') {
            separated.push('\n');
        } else {
            separated.push(character);
        }
    }

    let mut parts = vec![whole];
    for section in separated.split('\n') {
        let mut words = Vec::new();
        for word in section
            .to_lowercase()
            .split(|character: char| !character.is_alphanumeric())
            .filter(|word| !word.is_empty())
        {
            if matches!(
                word,
                "and" | "feat" | "featuring" | "ft" | "with" | "x" | "vs" | "versus"
            ) {
                push_artist_part(&mut parts, &mut words);
            } else {
                words.push(word.to_string());
            }
        }
        push_artist_part(&mut parts, &mut words);
    }
    parts.sort_unstable();
    parts.dedup();
    parts
}

fn collect_artist_credits<'a>(value: &'a Value, credits: &mut Vec<&'a str>) {
    match value {
        Value::String(artist) => credits.push(artist),
        Value::Array(artists) => {
            for artist in artists {
                collect_artist_credits(artist, credits);
            }
        }
        Value::Object(artist) => {
            if let Some(name) = artist.get("name").and_then(Value::as_str) {
                credits.push(name);
            }
        }
        _ => {}
    }
}

fn item_artists_match(requested_artists: &[String], item: &Value) -> bool {
    let requested: Vec<String> = requested_artists
        .iter()
        .map(|artist| normalize_artist_name(artist))
        .filter(|artist| !artist.is_empty())
        .collect();
    if requested.is_empty() {
        return true;
    }

    let mut credits = Vec::new();
    if let Some(artist) = item.get("artist") {
        collect_artist_credits(artist, &mut credits);
    }
    if let Some(artist) = item.get("artistName") {
        collect_artist_credits(artist, &mut credits);
    }
    if let Some(artists) = item.get("artists") {
        collect_artist_credits(artists, &mut credits);
    }
    let candidates: Vec<String> = credits.into_iter().flat_map(artist_credit_names).collect();

    // One exact credit is sufficient because mirrors commonly omit featured collaborators.
    candidates.is_empty()
        || requested
            .iter()
            .any(|artist| candidates.iter().any(|candidate| candidate == artist))
}

fn item_matches_request(title: &str, artists: &[String], item: &Value) -> bool {
    shares_a_word_with_title(title, item) && item_artists_match(artists, item)
}

fn select_candidate<'a>(
    items: &'a [Value],
    title: &str,
    artists: &[String],
    target_secs: i64,
) -> Option<&'a Value> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let delta = (item["duration"].as_i64()? - target_secs).abs();
            (delta <= 3 && item_matches_request(title, artists, item))
                .then_some((delta, index, item))
        })
        .min_by_key(|(delta, index, _)| (*delta, *index))
        .map(|(_, _, item)| item)
}

/// Accept a remote media URL only if it is a plain `https://` URL with no embedded credentials.
///
/// Mirror and manifest responses are untrusted, and the string they yield becomes the argument handed
/// to mpv. A `file:` URL would make mpv read a local path, `data:` would inline attacker-chosen bytes,
/// and a value beginning with `-` could be parsed by mpv as an option. Requiring https at this network
/// boundary rejects all of them before the string can reach the player; `play_file` adds a `--` marker
/// as a second, process-boundary backstop.
fn safe_media_url(url: &str) -> Option<&str> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    // Reject embedded control bytes up front. The URL parser silently strips ASCII tab/CR/LF per the
    // WHATWG spec, so a value like "https://ex\nample/a.flac" would parse to a clean https URL — yet we
    // return the *raw* &str, which would still carry the byte into the mpv argument. Refusing them here
    // keeps the returned string exactly what was validated.
    if url.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    let parsed = reqwest::Url::parse(url).ok()?;
    (parsed.scheme() == "https" && parsed.username().is_empty() && parsed.password().is_none())
        .then_some(url)
}

fn direct_stream_url(data: &Value) -> Option<&str> {
    data["OriginalTrackUrl"].as_str().and_then(safe_media_url)
}

fn dash_has_only_blank_base_urls(content: &[u8]) -> bool {
    let Ok(xml) = std::str::from_utf8(content) else {
        return false;
    };

    let mut rest = xml;
    let mut found = false;
    while let Some(offset) = rest.find("<BaseURL") {
        let tag = &rest[offset + "<BaseURL".len()..];
        if !tag
            .chars()
            .next()
            .is_some_and(|character| character.is_whitespace() || matches!(character, '>' | '/'))
        {
            rest = tag;
            continue;
        }
        let Some(open_end) = tag.find('>') else {
            return false;
        };
        found = true;
        if tag[..open_end].trim_end().ends_with('/') {
            rest = &tag[open_end + 1..];
            continue;
        }

        let body = &tag[open_end + 1..];
        let Some(close_start) = body.find("</BaseURL>") else {
            return false;
        };
        if !body[..close_start].trim().is_empty() {
            return false;
        }
        rest = &body[close_start + "</BaseURL>".len()..];
    }
    found
}

/// Whether the manifest carries at least one absolute http(s) `<BaseURL>`.
///
/// The manifest is written to a local file, so mpv/libavformat resolve relative BaseURL and segment
/// URLs against that `file://` location UNLESS an absolute BaseURL re-roots them onto the network. A
/// hostile mirror can otherwise ship a relative (`../../etc/x`) or protocol-relative (`//host/x`)
/// reference that carries no scheme — so it slips past `dash_manifest_has_unsafe_url`, which only
/// rejects an explicit non-http scheme — and have mpv resolve it against `file://` to reach a local
/// path. Requiring an absolute http(s) BaseURL guarantees the resolution root is the network: every
/// relative or protocol-relative reference then resolves to http(s), and any absolute non-http
/// reference is still rejected separately. A genuine mirror manifest, loaded from a local file, needs
/// this BaseURL to play at all, so this rejects nothing that would have worked.
fn dash_has_absolute_http_base_url(content: &[u8]) -> bool {
    let Ok(xml) = std::str::from_utf8(content) else {
        return false;
    };

    let mut rest = xml;
    while let Some(offset) = rest.find("<BaseURL") {
        let tag = &rest[offset + "<BaseURL".len()..];
        // Only a real tag boundary counts, so `<BaseURLFoo>` does not match `<BaseURL>`.
        if !tag
            .chars()
            .next()
            .is_some_and(|character| character.is_whitespace() || matches!(character, '>' | '/'))
        {
            rest = tag;
            continue;
        }
        let Some(open_end) = tag.find('>') else {
            return false;
        };
        // A self-closing `<BaseURL/>` has no content to inspect.
        if tag[..open_end].trim_end().ends_with('/') {
            rest = &tag[open_end + 1..];
            continue;
        }

        let body = &tag[open_end + 1..];
        let Some(close_start) = body.find("</BaseURL>") else {
            return false;
        };
        let value = body[..close_start].trim().to_ascii_lowercase();
        if value.starts_with("https://") || value.starts_with("http://") {
            return true;
        }
        rest = &body[close_start + "</BaseURL>".len()..];
    }
    false
}

/// Whether a DASH manifest references any URL scheme other than http/https.
///
/// mpv/libavformat resolve BaseURL and segment URLs out of the manifest we hand them, and `file`
/// remains in the demuxer's protocol whitelist because it is needed to open cached/offline files and
/// the local `.mpd` itself. Rather than remove `file` (which breaks that legitimate use), the manifest
/// content is validated here: any absolute reference whose scheme is not http/https — `file://`,
/// `data:`, `ftp://`, `pipe:` and the like — is treated as hostile. Relative segment URLs carry no
/// scheme and are unaffected, so genuine mirror manifests (absolute https, or relative against an
/// https BaseURL) still pass.
fn dash_manifest_has_unsafe_url(content: &[u8]) -> bool {
    let Ok(xml) = std::str::from_utf8(content) else {
        // Not valid UTF-8: refuse rather than hand unpredictable bytes to the demuxer.
        return true;
    };
    let lower = xml.to_ascii_lowercase();

    // `scheme://...` for any scheme that is not http/https.
    for (idx, _) in lower.match_indices("://") {
        // Walk back to the char before the scheme. `char_indices().rev()` keeps `scheme_start` on a
        // char boundary; `rfind` returns the *start* byte of the matched char, so a naive `+ 1` would
        // land inside a multibyte char (any non-ASCII byte before `://`) and panic on the slice below.
        let scheme_start = lower[..idx]
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map(|(position, c)| position + c.len_utf8())
            .unwrap_or(0);
        let scheme = &lower[scheme_start..idx];
        if scheme != "http" && scheme != "https" {
            return true;
        }
    }

    // `data:` URIs carry no `//` authority, so they are caught separately — but only where `data:`
    // begins an attribute or element value (preceded by a quote, `>`, or whitespace), so incidental
    // substrings such as `metadata:` do not trip it.
    lower.match_indices("data:").any(|(idx, _)| {
        idx == 0
            || lower[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| matches!(c, '"' | '\'' | '>' | ' ' | '\t' | '\n' | '\r'))
    })
}

fn parse_to_seconds(duration_str: &str) -> i64 {
    let parts: Vec<&str> = duration_str.split(':').collect();
    match parts.as_slice() {
        [h, m, s] => (h.parse::<i64>().unwrap_or(0).saturating_mul(3600))
            .saturating_add(m.parse::<i64>().unwrap_or(0).saturating_mul(60))
            .saturating_add(s.parse::<i64>().unwrap_or(0)),
        [m, s] => m
            .parse::<i64>()
            .unwrap_or(0)
            .saturating_mul(60)
            .saturating_add(s.parse::<i64>().unwrap_or(0)),
        [s] => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn decode_manifest(encoded: &str, track_id: i64) -> Result<String, Box<dyn Error + Send + Sync>> {
    let decoded_bytes = BASE64_STANDARD.decode(encoded)?;
    let content = &decoded_bytes[decoded_bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(decoded_bytes.len())..];

    if content.first().map(|&b| b == b'{').unwrap_or(false)
        && let Ok(json) = serde_json::from_slice::<Value>(content)
        && let Some(url) = json["urls"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .and_then(safe_media_url)
    {
        return Ok(url.to_string());
    }

    if content.first().map(|&b| b == b'<').unwrap_or(false) {
        if dash_has_only_blank_base_urls(content) {
            return Err("Bad lossless manifest".into());
        }

        // The manifest is written to a local file and mpv/libavformat resolve its BaseURL and segment
        // URLs from there, with `file` still in the demuxer protocol whitelist (needed for cached and
        // offline files). A hostile mirror could otherwise smuggle a `file://` or `data:` reference in
        // to make the player read a local path — so refuse any manifest that names a non-http(s)
        // scheme, before it is ever written.
        if dash_manifest_has_unsafe_url(content) {
            return Err("Bad lossless manifest".into());
        }

        // The scheme check above only catches *absolute* non-http references. Relative and
        // protocol-relative references carry no scheme, so they resolve against the manifest's own
        // `file://` location — reaching local paths anyway. Require an absolute http(s) BaseURL to
        // re-root all resolution onto the network before writing anything to disk.
        if !dash_has_absolute_http_base_url(content) {
            return Err("Bad lossless manifest".into());
        }

        // Written into this process's own scratch directory. In the shared temp/ directory two
        // instances playing the same track collided on "<track_id>.mpd", and either one's startup
        // sweep could delete the file the other's mpv was streaming from.
        let mut music_dir = dirs::audio_dir().ok_or("Could not find audio directory")?;
        music_dir.push("whytui");

        let mut path = crate::player::session_temp_dir(&music_dir);
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        path.push(format!("{}.mpd", track_id));
        std::fs::write(&path, content).map_err(|e| e.to_string())?;

        return Ok(path.to_string_lossy().to_string());
    }

    Err("Bad lossless manifest".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coincidental_duration_match_is_rejected() {
        // the failure this guards: same length, entirely different song, served as "the lossless one"
        let unrelated = serde_json::json!({"title": "Bohemian Rhapsody", "duration": 210});
        assert!(!shares_a_word_with_title("Blinding Lights", &unrelated));
    }

    #[test]
    fn the_same_song_still_matches() {
        let same = serde_json::json!({"title": "Blinding Lights", "duration": 200});
        assert!(shares_a_word_with_title("Blinding Lights", &same));

        // partial / differently-punctuated titles must still pass
        let punctuated = serde_json::json!({"title": "Blinding Lights (Remix)"});
        assert!(shares_a_word_with_title("Blinding Lights", &punctuated));
        let subtitle = serde_json::json!({"title": "Lights"});
        assert!(shares_a_word_with_title("Blinding Lights", &subtitle));
    }

    #[test]
    fn missing_title_falls_back_to_duration_only() {
        // never make lossless unreachable just because a mirror omits the field
        let no_title = serde_json::json!({"duration": 200});
        assert!(shares_a_word_with_title("Blinding Lights", &no_title));
        let alt_field = serde_json::json!({"name": "Blinding Lights"});
        assert!(shares_a_word_with_title("Blinding Lights", &alt_field));
    }

    #[test]
    fn short_words_alone_do_not_count_as_a_match() {
        let item = serde_json::json!({"title": "The Man Who Sold The World"});
        // "the" and "man" are noise/short; nothing significant is shared with this query
        assert!(!shares_a_word_with_title("Ode to the Sun", &item));
    }

    #[test]
    fn artist_metadata_rejects_a_same_title_cover() {
        let cover = serde_json::json!({
            "title": "Hello",
            "artist": {"name": "Someone Else"},
            "duration": 295
        });
        let original = serde_json::json!({
            "title": "Hello",
            "artist": {"name": "Adele"},
            "duration": 295
        });
        assert!(!item_matches_request(
            "Hello",
            &["Adele".to_string()],
            &cover
        ));
        assert!(item_matches_request(
            "Hello",
            &["Adele".to_string()],
            &original
        ));
    }

    #[test]
    fn title_words_cannot_satisfy_artist_validation() {
        let items = [serde_json::json!({
            "id": 1,
            "title": "Adele",
            "artist": "Adele",
            "duration": 180
        })];

        assert!(select_candidate(&items, "Adele", &["Someone Else".to_string()], 180).is_none());
    }

    #[test]
    fn reordered_and_partial_artist_credits_match() {
        let requested = ["Artist A".to_string(), "Artist B".to_string()];
        let reordered = [serde_json::json!({
            "title": "Shared Song",
            "artist": "artist b & ARTIST-A",
            "duration": 200
        })];
        let partial = [serde_json::json!({
            "title": "Shared Song",
            "artists": [{"name": "Another Artist"}, {"name": "Artist B"}],
            "duration": 200
        })];

        assert!(select_candidate(&reordered, "Shared Song", &requested, 200).is_some());
        assert!(select_candidate(&partial, "Shared Song", &requested, 200).is_some());
    }

    #[test]
    fn unrelated_artist_credits_are_rejected_without_substring_matches() {
        let requested = ["Artist A".to_string(), "Ann".to_string()];
        let unrelated = [serde_json::json!({
            "title": "Shared Song",
            "artist": "Artist AB & Joanne",
            "duration": 200
        })];

        assert!(select_candidate(&unrelated, "Shared Song", &requested, 200).is_none());
    }

    #[test]
    fn best_duration_match_wins_even_when_it_is_not_first() {
        let items = [
            serde_json::json!({"title": "Hello", "artist": "Adele", "duration": 298}),
            serde_json::json!({"title": "Hello", "artist": "Adele", "duration": 295}),
        ];
        let selected = select_candidate(&items, "Hello", &["Adele".to_string()], 295).unwrap();
        assert_eq!(selected["duration"], 295);
    }

    #[test]
    fn manifest_allows_leading_whitespace() {
        let encoded = BASE64_STANDARD.encode("\n  {\"urls\":[\"https://example/audio.flac\"]}");
        assert_eq!(
            decode_manifest(&encoded, 1).unwrap(),
            "https://example/audio.flac"
        );
    }

    #[test]
    fn empty_direct_and_json_stream_urls_are_rejected() {
        let valid_direct = serde_json::json!({"OriginalTrackUrl": "https://example/audio.flac"});
        assert_eq!(
            direct_stream_url(&valid_direct),
            Some("https://example/audio.flac")
        );

        let direct = serde_json::json!({"OriginalTrackUrl": " \n\t "});
        assert_eq!(direct_stream_url(&direct), None);

        for url in ["", " \n\t "] {
            let encoded = BASE64_STANDARD.encode(format!("{{\"urls\":[{url:?}]}}"));
            assert!(decode_manifest(&encoded, 1).is_err());
        }
    }

    #[test]
    fn empty_dash_base_urls_are_rejected() {
        for manifest in [
            "<MPD><Period><BaseURL></BaseURL></Period></MPD>",
            "<MPD><Period><BaseURL> \n\t </BaseURL></Period></MPD>",
            "<MPD><Period><BaseURL /></Period></MPD>",
        ] {
            let encoded = BASE64_STANDARD.encode(manifest);
            assert!(decode_manifest(&encoded, 1).is_err());
        }

        assert!(!dash_has_only_blank_base_urls(
            b"<MPD><Period><BaseURL>https://example/audio.flac</BaseURL></Period></MPD>"
        ));
        assert!(!dash_has_only_blank_base_urls(
            b"<MPD><Period><SegmentTemplate media=\"https://example/$Number$\" /></Period></MPD>"
        ));
    }

    #[test]
    fn only_https_media_urls_are_accepted() {
        // The happy path: a signed https media URL with query parameters survives intact.
        assert_eq!(
            safe_media_url("https://cdn.example/audio.flac?token=abc&exp=123"),
            Some("https://cdn.example/audio.flac?token=abc&exp=123")
        );
        assert_eq!(
            safe_media_url("  https://example/a.flac  "),
            Some("https://example/a.flac")
        );

        // Everything an attacker could smuggle through a mirror response is refused.
        for hostile in [
            "file:///etc/passwd",
            "data:audio/flac;base64,AAAA",
            "javascript:alert(1)",
            "http://example/a.flac", // http is not permitted for remote media
            "ftp://example/a.flac",
            "--playlist=/tmp/attacker-list",
            "--input-ipc-server=/tmp/socket",
            "https://user:pass@example/a.flac", // embedded credentials
            "https://ex\nample/a.flac",         // embedded control byte the URL parser would strip
            "https://example/a\tflac",
            "",
            "   ",
        ] {
            assert_eq!(
                safe_media_url(hostile),
                None,
                "accepted hostile source {hostile:?}"
            );
        }
    }

    #[test]
    fn direct_stream_url_rejects_non_https_schemes() {
        assert_eq!(
            direct_stream_url(&serde_json::json!({"OriginalTrackUrl": "https://example/a.flac"})),
            Some("https://example/a.flac")
        );
        assert_eq!(
            direct_stream_url(&serde_json::json!({"OriginalTrackUrl": "file:///etc/passwd"})),
            None
        );
        assert_eq!(
            direct_stream_url(&serde_json::json!({"OriginalTrackUrl": "data:text/plain,x"})),
            None
        );
    }

    #[test]
    fn json_manifest_url_must_be_https() {
        for scheme_url in [
            "file:///etc/passwd",
            "data:text/plain,x",
            "http://example/a.flac",
        ] {
            let encoded = BASE64_STANDARD.encode(format!("{{\"urls\":[{scheme_url:?}]}}"));
            assert!(
                decode_manifest(&encoded, 1).is_err(),
                "accepted non-https JSON manifest url {scheme_url:?}"
            );
        }
    }

    #[test]
    fn dash_manifest_with_a_local_or_data_reference_is_rejected() {
        // A safe manifest: absolute https BaseURL, plus a relative segment template.
        assert!(!dash_manifest_has_unsafe_url(
            b"<MPD><Period><BaseURL>https://cdn.example/audio/</BaseURL><SegmentTemplate media=\"seg-$Number$.m4s\" /></Period></MPD>"
        ));
        // A `data:` reference in an attribute or element must not be treated as incidental text.
        assert!(dash_manifest_has_unsafe_url(
            b"<MPD><Period><BaseURL>data:audio/mp4;base64,AAAA</BaseURL></Period></MPD>"
        ));
        assert!(dash_manifest_has_unsafe_url(
            b"<MPD><Period><SegmentTemplate initialization=\"data:audio/mp4,AAAA\" /></Period></MPD>"
        ));
        // file:// / ftp:// segment references are refused.
        assert!(dash_manifest_has_unsafe_url(
            b"<MPD><Period><BaseURL>file:///etc/passwd</BaseURL></Period></MPD>"
        ));
        assert!(dash_manifest_has_unsafe_url(
            b"<MPD><Period><BaseURL>ftp://example/a</BaseURL></Period></MPD>"
        ));
        // An incidental `metadata:` substring (preceded by a letter) is not a data URI and must not
        // trip the check.
        assert!(!dash_manifest_has_unsafe_url(
            b"<MPD><Period><Label>see metadata:notes</Label><BaseURL>https://cdn.example/a/</BaseURL></Period></MPD>"
        ));
    }

    #[test]
    fn dash_manifest_with_multibyte_before_scheme_does_not_panic() {
        // A non-ASCII char immediately before `://` used to make the scheme scan slice on a non-char
        // boundary and panic. The manifest is untrusted, so it must return a verdict, not abort:
        // a multibyte char glued straight onto `://` leaves an empty (non-http) scheme -> unsafe.
        assert!(dash_manifest_has_unsafe_url(
            "<MPD><BaseURL>字://host/seg</BaseURL></MPD>".as_bytes()
        ));
        // A multibyte char elsewhere in the manifest does not disturb a genuine https BaseURL.
        assert!(!dash_manifest_has_unsafe_url(
            "<MPD><Label>曲</Label><BaseURL>https://cdn.example/a/</BaseURL></MPD>".as_bytes()
        ));
    }

    #[test]
    fn dash_requires_an_absolute_http_base_url() {
        // A genuine mirror manifest anchors its segments to an absolute http(s) BaseURL.
        assert!(dash_has_absolute_http_base_url(
            b"<MPD><BaseURL>https://cdn.example/a/</BaseURL></MPD>"
        ));
        assert!(dash_has_absolute_http_base_url(
            b"<MPD><BaseURL> HTTP://cdn.example/a/ </BaseURL></MPD>"
        ));

        // No BaseURL, only relative ones, or a blank/self-closing tag: nothing anchors the segments
        // to a remote host, so the manifest is not accepted as a lossless source.
        assert!(!dash_has_absolute_http_base_url(b"<MPD></MPD>"));
        assert!(!dash_has_absolute_http_base_url(
            b"<MPD><BaseURL>segments/</BaseURL></MPD>"
        ));
        assert!(!dash_has_absolute_http_base_url(
            b"<MPD><BaseURL></BaseURL></MPD>"
        ));
        assert!(!dash_has_absolute_http_base_url(b"<MPD><BaseURL/></MPD>"));
        // A tag that merely starts with `BaseURL` must not count as `<BaseURL>`.
        assert!(!dash_has_absolute_http_base_url(
            b"<MPD><BaseURLFoo>https://cdn.example/</BaseURLFoo></MPD>"
        ));
        // A non-http absolute scheme is not an http(s) anchor (and is independently rejected as unsafe).
        assert!(!dash_has_absolute_http_base_url(
            b"<MPD><BaseURL>file:///etc/passwd</BaseURL></MPD>"
        ));
    }

    #[test]
    fn test_parse_to_seconds_seconds_only() {
        assert_eq!(parse_to_seconds("45"), 45);
        assert_eq!(parse_to_seconds("0"), 0);
        assert_eq!(parse_to_seconds("3600"), 3600);
    }

    #[test]
    fn parse_to_seconds_saturates_instead_of_overflowing() {
        // An absurd duration (crafted or corrupt metadata) must not overflow — a debug build would
        // panic and a release build would wrap to a garbage value; it saturates to i64::MAX instead.
        assert_eq!(parse_to_seconds("999999999999999999:0:0"), i64::MAX);
        assert_eq!(parse_to_seconds("999999999999999999:0"), i64::MAX);
    }

    #[test]
    fn test_parse_to_seconds_mm_ss() {
        assert_eq!(parse_to_seconds("3:21"), 201); // 3*60 + 21
        assert_eq!(parse_to_seconds("0:30"), 30);
        assert_eq!(parse_to_seconds("1:00"), 60);
    }

    #[test]
    fn test_parse_to_seconds_h_mm_ss() {
        assert_eq!(parse_to_seconds("1:02:03"), 3723); // 1*3600 + 2*60 + 3
        assert_eq!(parse_to_seconds("0:03:21"), 201); // Same as 3:21
        assert_eq!(parse_to_seconds("2:00:00"), 7200);
    }

    #[test]
    fn test_parse_to_seconds_invalid() {
        assert_eq!(parse_to_seconds("invalid"), 0);
        assert_eq!(parse_to_seconds("a:b:c"), 0);
        assert_eq!(parse_to_seconds(""), 0);
    }
}
