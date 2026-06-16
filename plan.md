# plan.md — Exact Fix Plan for `whytui` Bugs

**Repository:** `https://github.com/DevaOnL/whytui`  
**Prepared for:** deva  
**Purpose:** Detailed implementation plan to fix the confirmed bugs found during manual review, user reproduction, and validation of automated AI findings.  
**Scope:** This plan covers all confirmed and relevant bugs discussed so far, including high-impact product bugs, runtime panics, documentation issues, and testing gaps.

---

## 0. Executive summary

The project has two broad categories of problems:

1. **Product correctness / user-facing behavior bugs**
   - playlist selection breaks for 10+ playlists,
   - stale autoplay tasks mutate the active queue,
   - terminal rendering is unsynchronized,
   - track identity is based on title or filename instead of stable IDs,
   - offline anti-repeat, lyrics, history, and cache behavior are affected,
   - hard-coded mpv socket breaks multi-instance usage,
   - terminal-size gate blocks common terminals like `80x24`.

2. **Robustness / runtime safety bugs**
   - malformed URL strings caused by escaped braces in `format!`,
   - authenticated API errors are ignored,
   - startup uses `.unwrap()` around cookie/client initialization,
   - playback uses `.unwrap()` around `mpv` process spawning,
   - many lock and I/O `.unwrap()` calls can cascade into panics,
   - LRC timestamp parser accepts malformed values,
   - no automated tests currently protect these behaviors.

The recommended order is:

1. Fix the small, high-confidence, high-impact defects first.
2. Introduce a canonical track identity model.
3. Fix queueing, rendering, and input architecture.
4. Clean up resilience issues.
5. Add tests so these bugs do not regress.

---

# 1. Preparation

## 1.1 Create a working branch

```bash
git checkout -b fix/core-bug-cleanup
```

If you want smaller reviewable pull requests, split into these branches:

```bash
git checkout -b fix/url-auth-startup-playback
git checkout -b fix/track-identity
git checkout -b fix/playlist-autoplay-rendering
git checkout -b fix/terminal-lyrics-locks
git checkout -b test/regression-suite
git checkout -b docs/runtime-dependencies
```

Recommended PR order:

1. `fix/url-auth-startup-playback`
2. `fix/track-identity`
3. `fix/playlist-autoplay-rendering`
4. `fix/terminal-lyrics-locks`
5. `test/regression-suite`
6. `docs/runtime-dependencies`

## 1.2 Run baseline commands

Before editing, capture the current state.

```bash
cargo check 2>&1 | tee baseline-cargo-check.log
cargo clippy --all-targets --all-features 2>&1 | tee baseline-clippy.log
cargo test 2>&1 | tee baseline-test.log
```

If external tools are available:

```bash
which mpv || true
which yt-dlp || true
which ffmpeg || true
```

If the app is runnable, also record:

```bash
cargo run -- --help
```

## 1.3 Add a basic test harness if missing

If the crate currently has no tests, start with unit tests inside modules using:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example() {
        assert!(true);
    }
}
```

For cross-module behavior, add integration tests under:

```text
tests/
```

---

# 2. Phase P0 — Small high-impact fixes

These should be fixed first because they are localized, high-confidence, and likely to unblock many other behaviors.

---

## P0.1 Fix malformed URL format strings

### Bug

Some code appears to use Rust format strings like:

```rust
format!("{{https://music.youtube.com/watch?v={}}}", video_id)
```

In Rust formatting:

- `{{` emits a literal `{`,
- `}}` emits a literal `}`.

So the generated URL becomes:

```text
{https://music.youtube.com/watch?v=VIDEO_ID}
```

That is malformed.

The same pattern was observed around `lrclib.net` URL construction.

### Impact

Likely affected:

- YouTube Music stream URL resolution,
- `yt-dlp` invocation,
- lrclib lyric fetching.

### Files to inspect

- `src/api.rs`
- `src/features.rs`

Search:

```bash
rg -n '\{\{https?://|https?://.*\}\}' src
```

Also search for all URL construction:

```bash
rg -n 'format!\(|http|https|lrclib|music.youtube|youtube.com|yt-dlp' src
```

### Exact fix

Replace malformed format strings with ordinary URL strings.

Bad:

```rust
let url = format!("{{https://music.youtube.com/watch?v={}}}", video_id);
```

Good:

```rust
let url = format!("https://music.youtube.com/watch?v={}", video_id);
```

Bad lrclib-style pattern:

```rust
format!(
    "{{https://lrclib.net/api/get?track_name={}}}&artist_name={}&album={}&duration={}",
    track_name,
    artist_name,
    album,
    duration
)
```

Good minimum fix:

```rust
format!(
    "https://lrclib.net/api/get?track_name={}&artist_name={}&album={}&duration={}",
    track_name,
    artist_name,
    album,
    duration
)
```

### Better fix: URL-encode query parameters

Add a dependency if not already present:

```toml
urlencoding = "2"
```

Then:

```rust
let url = format!(
    "https://lrclib.net/api/get?track_name={}&artist_name={}&album={}&duration={}",
    urlencoding::encode(track_name),
    urlencoding::encode(artist_name),
    urlencoding::encode(album),
    duration
);
```

For `music.youtube.com` watch URLs, if `video_id` is always a YouTube video ID, direct insertion is acceptable after validation. If it can contain arbitrary text, encode it too.

### Acceptance criteria

- No URL string begins with literal `{https://`.
- `rg -n '\{\{https?://' src` returns no results.
- Stream URL resolution passes a syntactically valid URL to `yt-dlp`.
- Lyric fetch requests call a syntactically valid `https://lrclib.net/...` URL.
- Query parameters containing spaces, punctuation, or non-ASCII characters are encoded.

### Tests

Add tests for URL builder functions if they are extractable.

Example:

```rust
#[test]
fn builds_plain_youtube_music_url_without_braces() {
    let url = build_youtube_music_url("abc123");
    assert_eq!(url, "https://music.youtube.com/watch?v=abc123");
    assert!(!url.starts_with('{'));
    assert!(!url.ends_with('}'));
}
```

For lrclib:

```rust
#[test]
fn builds_lrclib_url_without_literal_braces() {
    let url = build_lrclib_url("hello world", "a&b", "album", 123);
    assert!(url.starts_with("https://lrclib.net/api/get?"));
    assert!(!url.contains("{https://"));
    assert!(url.contains("hello%20world"));
    assert!(url.contains("a%26b"));
}
```

---

## P0.2 Fix authenticated API error handling in `post_auth`

### Bug

Current behavior is effectively:

```rust
let res = self.auth_client.post(endpoint).json(body).send().await?;
if !res.status().is_success() {}
Ok(res.json().await?)
```

The status check is empty. Non-2xx HTTP responses are treated like success responses.

### Impact

Users get confusing failures when:

- cookies expire,
- auth is invalid,
- API returns 401/403,
- rate limit returns 429,
- server returns 500,
- response body is HTML/text instead of expected JSON.

### File

- `src/api.rs`

Search:

```bash
rg -n 'post_auth|status\(\)\.is_success|res\.json' src/api.rs
```

### Exact fix

Replace the empty status check with an immediate error.

```rust
async fn post_auth(
    &self,
    endpoint: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let res = self.auth_client
        .post(endpoint)
        .json(body)
        .send()
        .await?;

    let status = res.status();

    if !status.is_success() {
        let error_body = res
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());

        return Err(format!(
            "Authenticated API request failed: {} {}: {}",
            status, endpoint, error_body
        ).into());
    }

    Ok(res.json().await?)
}
```

### Better long-term version

Define a typed error:

```rust
#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Forbidden,
    RateLimited,
    Server { status: u16, body: String },
    Transport(reqwest::Error),
    Json(reqwest::Error),
}
```

Then map known statuses:

```rust
match status.as_u16() {
    200..=299 => Ok(res.json().await?),
    401 => Err(ApiError::Unauthorized.into()),
    403 => Err(ApiError::Forbidden.into()),
    429 => Err(ApiError::RateLimited.into()),
    code => {
        let body = res.text().await.unwrap_or_default();
        Err(ApiError::Server { status: code, body }.into())
    }
}
```

### Acceptance criteria

- Non-2xx responses do not proceed to success JSON parsing.
- The returned error includes the HTTP status.
- Ideally, the returned error includes the response body or enough context to debug auth.
- Auth failures are shown to the user as auth failures, not JSON parse failures.

### Tests

Use a mock HTTP server if practical. If not, isolate response handling into a helper function.

Example test target:

```rust
fn classify_status(status: u16, body: String) -> Result<(), ApiError>
```

Test cases:

- 200 succeeds,
- 401 returns Unauthorized,
- 403 returns Forbidden,
- 429 returns RateLimited,
- 500 includes body.

---

## P0.3 Replace startup cookie/client `.unwrap()` calls

### Bug

Startup uses something like:

```rust
let yt_client = api::YTMusic::new_with_cookies(cookies_path.to_str().unwrap()).unwrap();
```

This can panic if:

- cookie path is not valid UTF-8,
- cookie file is missing,
- cookie file is unreadable,
- cookie file is malformed,
- client construction fails.

### Impact

The app crashes at startup with a low-quality panic instead of telling the user what to do.

### File

- `src/main.rs`

Search:

```bash
rg -n 'new_with_cookies|cookies_path|to_str\(\)\.unwrap|unwrap\(\)' src/main.rs src/api.rs
```

### Exact fix

Replace:

```rust
let yt_client = api::YTMusic::new_with_cookies(cookies_path.to_str().unwrap()).unwrap();
```

with:

```rust
let cookies_str = cookies_path
    .to_str()
    .ok_or_else(|| format!("Cookies path is not valid UTF-8: {:?}", cookies_path))?;

let yt_client = api::YTMusic::new_with_cookies(cookies_str)
    .map_err(|e| format!("Failed to load YouTube Music cookies from {}: {}", cookies_str, e))?;
```

If `main` does not currently return a `Result`, change it to:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ...
    Ok(())
}
```

If the app wants to control the exact output, use explicit `eprintln!` + exit:

```rust
if !cookies_path.exists() {
    eprintln!("ERROR: cookies.txt not found at {:?}", cookies_path);
    eprintln!("Please add your YouTube Music cookies.txt file.");
    std::process::exit(1);
}
```

### Important design decision

Decide whether cookies are:

1. mandatory for the app,
2. mandatory only for certain features,
3. optional with guest/degraded mode.

If mandatory, fail early with a clear message.

If optional, represent the client as:

```rust
Option<YTMusic>
```

and disable auth-only features when missing.

Do not invent guest mode unless the API layer actually supports it.

### Acceptance criteria

- Missing cookie file does not cause `.unwrap()` panic.
- Invalid cookie file does not cause `.unwrap()` panic.
- The user sees a clear message describing the cookie problem.
- `cargo clippy` no longer flags this startup path for avoidable unwraps.

### Tests

If the constructor is testable:

```rust
#[test]
fn missing_cookie_file_returns_error() {
    let result = YTMusic::new_with_cookies("/definitely/missing/cookies.txt");
    assert!(result.is_err());
}
```

For startup path, extract validation into a helper:

```rust
fn validate_cookies_path(path: &Path) -> Result<&str, String>
```

Then test:

- missing file,
- unreadable file if practical,
- invalid UTF-8 path on Unix if practical.

---

## P0.4 Handle `play_file()` / `mpv` spawn failures

### Bug

Calls like this can panic:

```rust
player::play_file(&track.url, &track, music_dir).unwrap()
```

`play_file()` spawns `mpv`, and spawning can fail.

### Impact

The app crashes instead of showing:

- `mpv not installed`,
- permission denied,
- process spawn failed,
- IPC socket failed.

### Files

- `src/main.rs`
- `src/player.rs`

Search:

```bash
rg -n 'play_file\(.*unwrap|play_file' src
```

### Exact fix at call sites

Replace:

```rust
let child = player::play_file(&track.url, &track, music_dir).unwrap();
*currently_playing = Some(child);
```

with:

```rust
match player::play_file(&track.url, &track, music_dir) {
    Ok(child) => {
        *currently_playing = Some(child);
    }
    Err(e) => {
        ui_common::set_status_line(Some(format!("Failed to start playback: {}", e)));
        return Ok(());
    }
}
```

### Improve `play_file()` return type

If `play_file()` only fails because of I/O/process spawning, prefer:

```rust
pub fn play_file(...) -> std::io::Result<Child> {
    Command::new("mpv")
        // args...
        .spawn()
}
```

If it can fail for several reasons, use `anyhow` or a custom error type.

Example with context:

```rust
pub fn play_file(...) -> Result<Child, Box<dyn std::error::Error>> {
    let child = Command::new("mpv")
        // args...
        .spawn()
        .map_err(|e| format!("Failed to spawn mpv: {}", e))?;

    Ok(child)
}
```

### Preflight dependency check

Add a startup check for external tools.

```rust
fn command_exists(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
```

Use mode-aware checks:

```rust
if !command_exists("mpv") {
    eprintln!("ERROR: required dependency missing: mpv");
    eprintln!("Install mpv and try again.");
    std::process::exit(1);
}

if !config().offline_mode && !command_exists("yt-dlp") {
    eprintln!("ERROR: required dependency missing for online playback: yt-dlp");
    std::process::exit(1);
}

if config().download_mode && !command_exists("ffmpeg") {
    eprintln!("ERROR: required dependency missing for downloads: ffmpeg");
    std::process::exit(1);
}
```

### Acceptance criteria

- Missing `mpv` does not panic.
- User sees a clear error.
- Playback failure does not leave stale UI state saying a track is playing.
- Autoplay is not triggered after failed playback.
- Previous-track playback also handles failure.

### Tests

Unit-test command checks by injecting a fake checker:

```rust
trait CommandChecker {
    fn exists(&self, cmd: &str) -> bool;
}
```

Integration/manual test:

```bash
PATH=/empty cargo run
```

Expected: clean dependency error, no panic.

---

## P0.5 Update README dependency and CLI documentation

### Bug

README says only `mpv` is required and describes `--download` incorrectly. Actual behavior also needs:

- `yt-dlp` for online playback,
- `ffmpeg` for saving/tagging downloads,
- `mpv` for playback,
- cookies for auth-dependent YouTube Music behavior.

### Files

- `README.md`
- CLI parsing in `src/main.rs`

Search:

```bash
rg -n 'download|offline|mpv|yt-dlp|ffmpeg|cookies|--download|--offline' README.md src/main.rs
```

### Exact fix

Update README sections:

#### Dependencies

Document:

```markdown
## Runtime dependencies

- `mpv` — required for audio playback.
- `yt-dlp` — required for resolving online YouTube Music streams.
- `ffmpeg` — required for downloading/converting/tagging saved tracks.
- YouTube Music cookies — required for authenticated YouTube Music API features.
```

#### CLI options

Clarify:

```markdown
- `-d`, `--download` — download/save selected tracks for offline use.
- `-o`, `--offline` — play from the local offline library.
- `-n`, `--nomix` — disable mix/autoplay behavior.
- `-l`, `--lossless` — use lossless mode where available.
- `-pl`, `--peak-lossless` — use peak lossless behavior where available.
- `-g`, `--guess` — enable guessing behavior where applicable.
```

Adjust descriptions to match actual code.

### Acceptance criteria

- README matches runtime behavior.
- Users know which binaries are needed.
- `--download` and `--offline` are not conflated.
- Setup instructions mention where to place cookies.

---

# 3. Phase P1 — Core product behavior fixes

---

## P1.1 Fix playlist selection for 10+ playlists

### Bug

The UI says the user can select playlist `1-N`, but the raw input thread sends digits one at a time. Playlist flows consume one message, so selecting `10` is read as `1`.

### Files

- `src/ui_common.rs`
- `src/main.rs`

Relevant areas:

- raw input handler,
- playlist browsing,
- playlist selection,
- library/search command flows.

Search:

```bash
rg -n 'spawn_input_handler|KeyCode::Char|is_ascii_digit|playlist|Select|recv\(\)' src
```

### Root cause

Input events represent individual keypresses, but selection code expects complete numbers.

### Exact fix strategy

Introduce a structured input event enum.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Digit(char),
    Enter,
    Esc,
    Backspace,
    Char(char),
    Command(String),
}
```

Update input thread:

```rust
match event.code {
    KeyCode::Char(c) if c.is_ascii_digit() => {
        let _ = tx.send(InputEvent::Digit(c));
    }
    KeyCode::Enter => {
        let _ = tx.send(InputEvent::Enter);
    }
    KeyCode::Esc => {
        let _ = tx.send(InputEvent::Esc);
    }
    KeyCode::Backspace => {
        let _ = tx.send(InputEvent::Backspace);
    }
    KeyCode::Char(c) => {
        let _ = tx.send(InputEvent::Char(c));
    }
    _ => {}
}
```

Then create a helper:

```rust
async fn read_number_selection(
    rx: &mut Receiver<InputEvent>,
    max: usize,
) -> Option<usize> {
    let mut buf = String::new();

    loop {
        match rx.recv().await? {
            InputEvent::Digit(c) => {
                buf.push(c);
                // optionally redraw prompt with typed number
            }
            InputEvent::Backspace => {
                buf.pop();
            }
            InputEvent::Enter => {
                let n: usize = buf.parse().ok()?;
                if (1..=max).contains(&n) {
                    return Some(n);
                } else {
                    return None;
                }
            }
            InputEvent::Esc => return None,
            InputEvent::Char('q') => return None,
            _ => {}
        }
    }
}
```

Use it in both playlist flows.

### Minimal alternative

If changing input event types is too invasive, keep strings but send `"enter"` and accumulate numeric strings:

```rust
let mut buf = String::new();

while let Some(msg) = rx.recv().await {
    if msg.chars().all(|c| c.is_ascii_digit()) {
        buf.push_str(&msg);
    } else if msg == "enter" {
        let selected: usize = buf.parse()?;
        break;
    }
}
```

### Acceptance criteria

- Playlist `1` works.
- Playlist `9` works.
- Playlist `10` works.
- Playlist `12` works.
- Invalid selection gives a message and does not select the wrong playlist.
- Backspace works if implemented.
- Esc/q cancels selection.

### Tests

Extract number parsing:

```rust
fn parse_selection(input: &str, max: usize) -> Option<usize>
```

Test:

```rust
assert_eq!(parse_selection("1", 12), Some(1));
assert_eq!(parse_selection("10", 12), Some(10));
assert_eq!(parse_selection("0", 12), None);
assert_eq!(parse_selection("13", 12), None);
assert_eq!(parse_selection("abc", 12), None);
```

If using event sequences:

```rust
assert_eq!(
    parse_events([Digit('1'), Digit('0'), Enter], 12),
    Some(10)
);
```

---

## P1.2 Fix autoplay stale queue race

### Bug

Old `queue_auto_add_online` tasks can continue after the user changes tracks. When they finish, they may append stale recommendations to the active queue/cache.

### Files

- `src/main.rs`

Search:

```bash
rg -n 'queue_auto_add_online|related|SONG_QUEUE|tokio::spawn|auto|recommend' src/main.rs
```

### Root cause

Asynchronous recommendation tasks do not verify that they still belong to the currently active playback session before mutating shared state.

### Exact fix: generation token

Add:

```rust
use std::sync::atomic::{AtomicU64, Ordering};

static AUTOPLAY_GENERATION: AtomicU64 = AtomicU64::new(0);
```

Every time the active track changes:

```rust
fn bump_autoplay_generation() -> u64 {
    AUTOPLAY_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}
```

When starting playback:

```rust
let generation = bump_autoplay_generation();
```

When spawning recommendation work:

```rust
let task_generation = AUTOPLAY_GENERATION.load(Ordering::SeqCst);

tokio::spawn(async move {
    let recommendations = fetch_recommendations(...).await;

    if AUTOPLAY_GENERATION.load(Ordering::SeqCst) != task_generation {
        return;
    }

    // Only now mutate queue/cache.
});
```

If passing generation explicitly is cleaner:

```rust
async fn queue_auto_add_online(..., generation: u64) {
    let recommendations = fetch_related(...).await?;

    if AUTOPLAY_GENERATION.load(Ordering::SeqCst) != generation {
        return Ok(());
    }

    // append
}
```

### Better fix: cancellation tokens

Use `tokio-util`:

```toml
tokio-util = "0.7"
```

Store the current cancellation token:

```rust
use tokio_util::sync::CancellationToken;

static CURRENT_AUTOPLAY_CANCEL: Lazy<Mutex<Option<CancellationToken>>> = ...
```

On track change:

```rust
if let Some(token) = current.take() {
    token.cancel();
}

let token = CancellationToken::new();
*current = Some(token.clone());

tokio::spawn(async move {
    tokio::select! {
        _ = token.cancelled() => return,
        result = fetch_recommendations(...) => {
            if token.is_cancelled() {
                return;
            }
            // append
        }
    }
});
```

Generation tokens are simpler; cancellation tokens are cleaner.

### Acceptance criteria

- If track A starts an autoplay fetch, then user switches to track B, A’s fetch cannot append to B’s queue.
- Related-song cache cannot be overwritten by stale sessions.
- Rapid next/previous commands do not leave stale recommendations.
- Manual queue operations are not overwritten by old background work.

### Tests

Make recommendation fetch injectable.

```rust
trait RecommendationProvider {
    async fn related(&self, track: &Track) -> Vec<Track>;
}
```

Test scenario:

1. Start generation 1 for track A.
2. Spawn delayed recommendation fetch.
3. Bump to generation 2 for track B.
4. Complete generation 1 fetch.
5. Assert queue does not contain A recommendations.

---

## P1.3 Synchronize terminal rendering

### Bug

The background monitor redraws periodically while search/library/playlist flows also print directly to stdout. Output interleaves.

### Files

- `src/ui_common.rs`
- `src/main.rs`
- `src/ui1.rs`
- `src/ui2.rs`
- `src/ui3.rs`

Search:

```bash
rg -n 'println!|print!|execute!|queue!|stdout|render|draw|monitor|start_monitor_thread' src
```

### Root cause

Multiple code paths own terminal output.

### Correct long-term fix: single render owner

Introduce an app event loop:

```rust
enum AppEvent {
    Input(InputEvent),
    Tick,
    TrackChanged(Track),
    QueueUpdated,
    StatusChanged(String),
    SearchResults(Vec<Track>),
    LibraryPageChanged,
}
```

Only one task/function should render:

```rust
async fn render_loop(mut rx: Receiver<AppEvent>, state: Arc<AppState>) {
    while let Some(event) = rx.recv().await {
        update_state(&state, event);
        render(&state);
    }
}
```

All other tasks should send events, not print directly.

### Short-term fix: output mutex

If full refactor is too large, add a global output lock.

```rust
use once_cell::sync::Lazy;
use std::sync::Mutex;

static OUTPUT_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
```

Wrap all drawing:

```rust
pub fn with_output_lock<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let _guard = OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}
```

Usage:

```rust
ui_common::with_output_lock(|| {
    execute!(stdout, ...)?;
    println!("...");
    Ok::<(), std::io::Error>(())
})?;
```

### Important note

A mutex prevents interleaving but does not fully solve state/render consistency. It is a tactical patch.

### Acceptance criteria

- Search prompts do not interleave with lyric monitor redraw.
- Library pages do not get overwritten by monitor output.
- Rendering functions do not run concurrently.
- All stdout writes are either under a lock or routed through the renderer.

### Tests

Automated terminal rendering tests are difficult, but you can:

- refactor render functions to write into a `Write` buffer,
- assert output snapshots,
- test that all direct printing goes through a render abstraction.

Search check:

```bash
rg -n 'println!|print!|execute!|queue!' src
```

Each match should be reviewed and categorized as:

- render-owner only,
- under output lock,
- test/debug only,
- should be removed.

---

## P1.4 Introduce canonical track identity

### Bug family

Multiple bugs share the same root cause: the app identifies tracks by weak fields like title or filename stem.

Affected bugs:

- offline anti-repeat broken,
- lyric state keyed only by title,
- recent-history / previous-track dedupe by title,
- cache filename collisions,
- autoplay exclusion weakness,
- same-title tracks collapse incorrectly.

### Files

- `src/main.rs`
- `src/player.rs`
- `src/offline.rs`
- `src/ui_common.rs`
- `src/ui1.rs`
- `src/ui2.rs`
- `src/ui3.rs`
- possibly `src/api.rs`

Search:

```bash
rg -n 'title|artist|filename|file_stem|RECENTLY_PLAYED|CURRENT_LYRIC_SONG|history|cache|the_naming_format' src
```

### Exact fix: add `TrackId` or `track_key`

Best option: add a stable identity field to `Track`.

Example:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrackId(String);
```

Or simpler:

```rust
pub fn track_key(track: &Track) -> String {
    if !track.video_id.is_empty() {
        format!("yt:{}", track.video_id)
    } else if !track.url.is_empty() {
        format!("url:{}", track.url)
    } else {
        format!(
            "meta:{}:{}:{}",
            normalize_key_part(&track.title),
            normalize_key_part(&track.artist),
            normalize_key_part(&track.album),
        )
    }
}

fn normalize_key_part(s: &str) -> String {
    s.trim().to_lowercase()
}
```

If `Track` does not have `video_id`, add one or derive from URL where possible.

### Better local/offline identity

For local files, use metadata if available:

- MusicBrainz ID if present,
- embedded title/artist/album/duration,
- file path as fallback,
- content hash as strongest fallback.

Example:

```rust
pub fn offline_track_key(path: &Path, metadata: Option<&Track>) -> String {
    if let Some(track) = metadata {
        return track_key(track);
    }

    format!("file:{}", path.to_string_lossy())
}
```

Avoid filename stem as identity.

---

## P1.5 Fix offline anti-repeat

### Bug

Recent exclusions are built from `Track.title`, but offline random selection filters by filename stem. Downloaded files are named like:

```text
title - artist
```

So recently played tracks can be requeued.

### Files

- `src/offline.rs`
- `src/player.rs`
- `src/main.rs`

Search:

```bash
rg -n 'get_excluded_titles|get_random_batch|file_stem|RECENTLY_PLAYED|recent' src
```

### Exact fix

Replace title-based exclusions with track-key exclusions.

Before:

```rust
let excluded_titles = get_excluded_titles();
```

After:

```rust
let excluded_keys = get_excluded_track_keys();
```

For recently played:

```rust
fn get_excluded_track_keys() -> HashSet<String> {
    RECENTLY_PLAYED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(track_key)
        .collect()
}
```

For offline files:

```rust
let candidate_key = offline_track_key(&path, metadata.as_ref());

if excluded_keys.contains(&candidate_key) {
    continue;
}
```

If metadata reading is expensive, build an offline library index once.

### Acceptance criteria

- Recently played offline tracks are excluded even if filenames include artist.
- Two different tracks with the same title are not incorrectly excluded if they have different identities.
- Exclusion works for downloaded files and manually added local files.

---

## P1.6 Fix lyric state keyed only by title

### Bug

Lyric state uses title-only identity. Same-title tracks can reuse or overwrite lyrics.

### Files

- `src/ui_common.rs`
- `src/ui1.rs`
- `src/ui2.rs`
- `src/ui3.rs`

Search:

```bash
rg -n 'CURRENT_LYRIC_SONG|LYRICS|track.title|lyrics' src
```

### Exact fix

Replace:

```rust
static CURRENT_LYRIC_SONG: RwLock<String>
```

with:

```rust
static CURRENT_LYRIC_TRACK_KEY: RwLock<Option<String>>
```

When track changes:

```rust
let key = track_key(track);

let mut current = CURRENT_LYRIC_TRACK_KEY.write().unwrap_or_else(|e| e.into_inner());

if current.as_ref() != Some(&key) {
    *current = Some(key);
    // reload lyrics
}
```

### Acceptance criteria

- Two tracks with the same title but different artist/video ID get separate lyrics.
- Monitor restarts lyric state when track identity changes, even if title is unchanged.
- Cached lyrics are keyed by track identity, not title.

---

## P1.7 Fix history and previous-track dedupe

### Bug

History dedupe compares title only, and only against the most recent entry.

### Files

- `src/main.rs`

Search:

```bash
rg -n 'add_to_history|get_prev_track|RECENTLY_PLAYED|history|back\(\)' src/main.rs
```

### Exact fix

Use track identity.

Before:

```rust
if list.back().map(|t| &t.title) != Some(&track.title) {
    list.push_back(track.clone());
}
```

After:

```rust
let new_key = track_key(track);

if list
    .back()
    .map(|t| track_key(t) != new_key)
    .unwrap_or(true)
{
    list.push_back(track.clone());
}
```

If global dedupe is desired:

```rust
if !list.iter().any(|t| track_key(t) == new_key) {
    list.push_back(track.clone());
}
```

But preserve current behavior if only consecutive dedupe is intended.

### Acceptance criteria

- Same-title different tracks can appear as separate history entries.
- Consecutive duplicate of the exact same track is still deduped if intended.
- Previous-track command returns the actual previous track identity, not merely previous title.

---

## P1.8 Fix cache filename collisions

### Bug

Local cache filenames use title + first artist. Variants can collide.

### Files

- `src/player.rs`
- `src/main.rs`

Search:

```bash
rg -n 'safe_title|safe_artist|file_name|download|cache|the_naming_format' src
```

### Exact fix

Include stable ID in filename.

```rust
fn cache_filename(track: &Track) -> String {
    let safe_title = sanitize_filename(&track.title);
    let safe_artist = sanitize_filename(&track.artist);
    let id = stable_file_id(track);

    format!("{} - {} [{}].mp3", safe_title, safe_artist, id)
}
```

Where:

```rust
fn stable_file_id(track: &Track) -> String {
    if !track.video_id.is_empty() {
        return sanitize_filename(&track.video_id);
    }

    short_hash(&track.url)
}
```

Hash helper:

```rust
fn short_hash(input: &str) -> String {
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(input.as_bytes());
    let hex = hex::encode(hasher.finalize());
    hex[..10].to_string()
}
```

### Migration plan

Existing files named:

```text
title - artist.ext
```

should still be discoverable. Do not break old libraries.

Options:

1. Keep reading old files, but save new files with IDs.
2. On startup, optionally migrate old files if metadata can determine the ID.
3. Avoid automatic destructive renames unless user confirms.

### Acceptance criteria

- Two tracks with same title and artist but different video IDs save to different files.
- Existing offline files remain playable.
- Cache lookup checks both new filename and legacy filename during transition.

---

## P1.9 Fix hard-coded mpv IPC socket

### Bug

All instances use:

```text
/tmp/whytui.sock
```

Multiple app instances conflict.

### File

- `src/player.rs`

Search:

```bash
rg -n 'whytui.sock|get_ipc_path|input-ipc-server|/tmp' src/player.rs src
```

### Exact fix

Generate per-process socket path.

```rust
pub fn get_ipc_path() -> PathBuf {
    std::env::temp_dir().join(format!("whytui-{}.sock", std::process::id()))
}
```

If multiple mpv processes can exist in the same app process, include a counter:

```rust
static IPC_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn get_ipc_path() -> PathBuf {
    let n = IPC_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "whytui-{}-{}.sock",
        std::process::id(),
        n
    ))
}
```

### Cleanup

Before starting mpv:

```rust
let _ = std::fs::remove_file(&ipc);
```

After mpv exits:

```rust
let _ = std::fs::remove_file(&ipc);
```

Do not remove another instance’s socket.

### Acceptance criteria

- Two instances can run simultaneously.
- One instance does not delete another instance’s socket.
- Socket cleanup happens on normal exit.
- Stale sockets are harmless.

---

# 4. Phase P2 — Terminal, locks, parser, resolver, and warning cleanup

---

## P2.1 Fix terminal-size hard block

### Bug

App hard-blocks unless terminal is at least `52x37`, making normal `80x24` unusable.

### Files

- `src/main.rs`
- UI modules

Search:

```bash
rg -n '52|37|min_width|min_height|terminal::size|RESTRICTION|ENFORCE' src
```

### Exact fix

Replace hard-coded large height requirement with either:

1. adaptive layout,
2. compact mode,
3. lower minimum height.

Example:

```rust
const MIN_WIDTH: u16 = 52;
const MIN_HEIGHT: u16 = 20;
```

Better:

```rust
fn layout_for_size(cols: u16, rows: u16) -> LayoutMode {
    if cols >= 80 && rows >= 24 {
        LayoutMode::Normal
    } else if cols >= 52 && rows >= 16 {
        LayoutMode::Compact
    } else {
        LayoutMode::TooSmall
    }
}
```

### Remove unreachable code

If code exists after `break` in the size loop, move it before `break` or delete it.

Bad pattern:

```rust
break;
execute!(...); // unreachable
```

Good:

```rust
execute!(...)?;
break;
```

### Use safe terminal-size fallback

```rust
let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
```

### Acceptance criteria

- App starts in `80x24`.
- App gives a clean compact warning only if truly too small.
- No unreachable code warning remains around the terminal-size loop.
- All terminal-size queries use graceful fallback or error handling.

---

## P2.2 Fix library shuffle read lock across `.await`

### Bug

A read lock is held across an `.await`, which can cause contention and future deadlock risk.

### File

- `src/main.rs`

Search:

```bash
rg -n 'read\(\).*await|write\(\).*await|shuffle|library' src/main.rs
```

Manual inspection is needed because the lock and await may be on different lines.

### Exact fix pattern

Bad:

```rust
let library = LIBRARY.read().unwrap();
let selected = choose_from(&library);
play(selected).await;
drop(library);
```

Good:

```rust
let selected = {
    let library = LIBRARY.read().unwrap_or_else(|e| e.into_inner());
    choose_from(&library).cloned()
};

if let Some(track) = selected {
    play(track).await;
}
```

Rule:

> Never hold a `std::sync` lock guard across `.await`.

### Acceptance criteria

- Clippy no longer flags `await_holding_lock`.
- Lock guard scope ends before `.await`.
- Behavior unchanged.

---

## P2.3 Fix lossless resolver over-reliance on duration

### Bug

The lossless resolver may choose the wrong FLAC result when search ranking is noisy but durations are close.

### File

- `src/flac.rs`

Search:

```bash
rg -n 'duration|flac|search|score|candidate|artist|title|album' src/flac.rs
```

### Exact fix

Introduce weighted scoring:

```rust
struct CandidateScore {
    title_score: f32,
    artist_score: f32,
    album_score: f32,
    duration_score: f32,
    total: f32,
}
```

Example weights:

```rust
total =
    title_score * 0.40 +
    artist_score * 0.30 +
    album_score * 0.15 +
    duration_score * 0.15;
```

Reject candidates with:

- very low title similarity,
- very low artist similarity,
- duration difference beyond threshold.

Example:

```rust
if title_score < 0.75 {
    reject;
}

if artist_score < 0.70 {
    reject;
}

if duration_diff_seconds > 5 {
    reject;
}
```

Use duration as one signal, not the primary signal.

### Acceptance criteria

- Wrong same-duration tracks are rejected if title/artist do not match.
- Live/remaster/acoustic variants are not selected unless metadata strongly matches.
- Resolver logs or exposes why a candidate was chosen.

### Tests

Create fake candidates:

1. exact title/artist, slight duration difference -> choose.
2. wrong title, exact duration -> reject.
3. same title, wrong artist -> reject.
4. same title/artist, album mismatch but close duration -> maybe choose with lower confidence.

---

## P2.4 Fix LRC timestamp parser and multi-timestamp lines

### Bugs

1. Parser accepts malformed timestamps.
2. Parser may not support multi-timestamp LRC lines.

### File

- `src/features.rs`

Search:

```bash
rg -n 'parse_lrc|parse_timestamp|Duration|lyrics|\[.*\]' src/features.rs
```

### Exact timestamp fix

```rust
fn parse_timestamp(ts: &str) -> Option<Duration> {
    let (min_str, sec_str) = ts.split_once(':')?;

    let minutes: u64 = min_str.parse().ok()?;
    let seconds: f64 = sec_str.parse().ok()?;

    if !seconds.is_finite() {
        return None;
    }

    if !(0.0..60.0).contains(&seconds) {
        return None;
    }

    if minutes > 12 * 60 {
        return None;
    }

    let minute_ms = minutes.checked_mul(60_000)?;
    let second_ms = (seconds * 1000.0).round() as u64;
    let total_ms = minute_ms.checked_add(second_ms)?;

    Some(Duration::from_millis(total_ms))
}
```

### Exact multi-timestamp fix

For a line:

```text
[00:10.00][00:20.00]same lyric
```

extract all timestamps and apply the same lyric to each.

Pseudo-code:

```rust
fn parse_lrc_line(line: &str) -> Vec<(Duration, String)> {
    let mut timestamps = Vec::new();
    let mut rest_start = 0;

    while let Some(open_rel) = line[rest_start..].find('[') {
        let open = rest_start + open_rel;

        let Some(close_rel) = line[open..].find(']') else {
            break;
        };

        let close = open + close_rel;
        let ts = &line[open + 1..close];

        if let Some(duration) = parse_timestamp(ts) {
            timestamps.push(duration);
            rest_start = close + 1;
        } else {
            break;
        }
    }

    let lyric = line[rest_start..].trim().to_string();

    timestamps
        .into_iter()
        .map(|ts| (ts, lyric.clone()))
        .collect()
}
```

Sort final lyrics by timestamp.

### Acceptance criteria

- Valid timestamps parse.
- `NaN`, `inf`, negative seconds, and seconds >= 60 are rejected.
- Multi-timestamp lines create multiple lyric entries.
- Lyrics remain sorted.

---

## P2.5 Address `RwLock` poisoning

### Bug

`std::sync::RwLock` returns `PoisonError` if a thread panics while holding a lock. Calling `.unwrap()` on future lock attempts can cascade panics.

### Files

- many files

Search:

```bash
rg -n '\.read\(\)\.unwrap\(\)|\.write\(\)\.unwrap\(\)' src
```

### Short-term fix

Add helpers:

```rust
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

pub fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

pub fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}
```

Replace:

```rust
SOME_LOCK.read().unwrap()
SOME_LOCK.write().unwrap()
```

with:

```rust
read_lock(&SOME_LOCK)
write_lock(&SOME_LOCK)
```

### Medium-term fix

Migrate to `parking_lot`, but remember:

```rust
parking_lot::RwLock::read()
```

returns the guard directly. There is no `.unwrap()`.

So change:

```rust
SOME_LOCK.read().unwrap()
```

to:

```rust
SOME_LOCK.read()
```

### Acceptance criteria

- No `.read().unwrap()` or `.write().unwrap()` remains.
- A poisoned lock does not cause an immediate cascade panic.
- No lock guard is held across `.await`.

---

## P2.6 Fix stdout and terminal-size unwraps

### Bugs

Minor panic paths:

```rust
io::stdout().flush().unwrap();
terminal::size().unwrap();
```

### Files

- `src/main.rs`
- `src/ui1.rs`
- `src/ui2.rs`
- `src/ui3.rs`
- `src/ui_common.rs`

Search:

```bash
rg -n 'flush\(\)\.unwrap|terminal::size\(\)\.unwrap|crossterm::terminal::size\(\)\.unwrap' src
```

### Exact fixes

For flush:

```rust
let _ = io::stdout().flush();
```

Or:

```rust
if let Err(e) = io::stdout().flush() {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return;
    }
}
```

For terminal size:

```rust
let (cols, rows) = terminal::size().unwrap_or((80, 24));
```

### Acceptance criteria

- No terminal-size unwrap remains.
- No stdout flush unwrap remains.
- App degrades gracefully when terminal query fails.

---

## P2.7 Fix `split_title_artist` readability, but do not classify as confirmed bug

### Background

Claude claimed this had a UTF-8 boundary panic. That claim is not valid as written because the indices come from ASCII bracket positions.

### Optional cleanup

Replace byte slicing with clearer string methods:

```rust
pub fn split_title_artist(input: &str) -> (String, String) {
    let input = input.trim();

    if let Some((title, rest)) = input.rsplit_once('[') {
        if let Some(artist) = rest.strip_suffix(']') {
            return (
                title.trim().to_string(),
                artist.trim().to_string(),
            );
        }
    }

    (input.to_string(), String::new())
}
```

### Tests

```rust
#[test]
fn split_title_artist_handles_cjk() {
    let (title, artist) = split_title_artist("日本語Song [アーティスト]");
    assert_eq!(title, "日本語Song");
    assert_eq!(artist, "アーティスト");
}

#[test]
fn split_title_artist_without_artist_returns_input() {
    let (title, artist) = split_title_artist("Plain Song");
    assert_eq!(title, "Plain Song");
    assert_eq!(artist, "");
}
```

### Acceptance criteria

- Behavior unchanged for normal ASCII.
- Non-ASCII cases work.
- No manual byte arithmetic needed.

---

## P2.8 Clean up warnings and dead code

### Bugs / hygiene issues

Previously reported:

- compiler warnings,
- clippy warnings,
- unreachable code,
- unused refactor stub in `src/app.rs`,
- dead `IS_LOSSLESS` / related state,
- unused variables,
- ignored terminal I/O results.

### Files

- `src/app.rs`
- `src/main.rs`
- `src/api.rs`
- possibly all modules

Search:

```bash
cargo check
cargo clippy --all-targets --all-features

rg -n 'unreachable|todo!|unimplemented!|allow\(dead_code\)|IS_LOSSLESS|let _ = execute|unused' src
```

### Exact fix process

1. Run `cargo check`.
2. Fix compiler warnings first.
3. Run `cargo clippy --all-targets --all-features`.
4. Fix meaningful clippy warnings.
5. Avoid blanket `#[allow(...)]` unless there is a real reason.
6. Remove unused `src/app.rs` if it is truly a stub.
7. Remove unreachable code after `break`.
8. Replace ignored `Result`s with either:
   - `?`,
   - explicit logging,
   - or `let _ = ...` only when intentionally safe.

### Acceptance criteria

- `cargo check` has no warnings or only documented intentional warnings.
- `cargo clippy --all-targets --all-features` has no high-value warnings.
- Dead stubs are removed or wired into the app.

---

# 5. Phase P3 — Testing plan

The project currently has no meaningful automated safety net. Add tests as part of the bug fixes, not after.

---

## P3.1 Unit tests

### URL builders

Test:

- YouTube Music URL has no literal braces.
- lrclib URL has no literal braces.
- query parameters are encoded.

### Auth response handling

Test:

- 200 success,
- 401 unauthorized,
- 403 forbidden,
- 429 rate-limited,
- 500 server error with body.

### Cookie path validation

Test:

- missing path,
- invalid path if practical,
- valid path.

### Track identity

Test:

- same title + different video ID -> different keys,
- same title + same video ID -> same key,
- local path fallback works.

### Cache filename

Test:

- same title/artist + different IDs -> different filenames,
- filenames are sanitized,
- legacy lookup still works.

### LRC parser

Test:

- valid timestamp,
- invalid seconds,
- NaN,
- infinity,
- huge minutes,
- multi-timestamp lines,
- sorted output.

### Selection parser

Test:

- `1`,
- `9`,
- `10`,
- out of range,
- `0`,
- empty,
- nonnumeric.

---

## P3.2 Integration tests

### Playlist selection

Simulate input event sequence:

```text
Digit('1'), Digit('0'), Enter
```

Expected: select playlist 10.

### Autoplay race

Use fake recommendation provider with delayed responses.

Expected:

- stale task does not mutate active queue.

### Offline anti-repeat

Build temporary offline library:

```text
Song A - Artist.mp3
Song B - Artist.mp3
```

Mark Song A recently played. Confirm Song A is not immediately requeued.

### Cache collision

Two tracks:

```text
title = "Song"
artist = "Artist"
video_id = "id1"

title = "Song"
artist = "Artist"
video_id = "id2"
```

Expected:

- two different cache paths.

### Dependency failure

Run with fake PATH:

```bash
PATH=/tmp/empty cargo run
```

Expected:

- clean dependency error,
- no panic.

---

## P3.3 Manual tests

### Terminal

Run in:

```bash
resize -s 24 80
cargo run
```

Expected:

- app starts or shows compact UI,
- no hard block at 37 rows.

### Multi-instance mpv socket

Start two instances.

Expected:

- each uses different IPC socket,
- neither kills the other’s socket.

### Rendering

While monitor is active:

- open library,
- search,
- change pages,
- view lyrics.

Expected:

- no interleaved prompts,
- no overwritten display.

### Lyrics

Play two same-title tracks with different artists.

Expected:

- lyrics reload correctly for each.

### Autoplay

Rapidly skip tracks while related-song fetches are in flight.

Expected:

- queue only contains recommendations for current session.

---

# 6. Recommended code organization improvements

These are not strictly required for the first bug-fix pass, but they will make the project much easier to maintain.

---

## 6.1 Add `track_identity.rs`

Create:

```text
src/track_identity.rs
```

Responsibilities:

- `track_key(&Track) -> String`
- `offline_track_key(...)`
- filename-safe stable IDs
- normalization helpers

Example:

```rust
pub fn normalize_key_part(s: &str) -> String {
    s.trim().to_lowercase()
}
```

---

## 6.2 Add `external_tools.rs`

Create:

```text
src/external_tools.rs
```

Responsibilities:

- detect `mpv`,
- detect `yt-dlp`,
- detect `ffmpeg`,
- mode-aware dependency validation,
- user-facing install messages.

---

## 6.3 Add `terminal_output.rs`

Create:

```text
src/terminal_output.rs
```

Responsibilities:

- output lock or render abstraction,
- safe terminal size,
- safe flush,
- common drawing helpers.

---

## 6.4 Add `errors.rs`

Create:

```text
src/errors.rs
```

Responsibilities:

- app error enum,
- API error enum,
- playback error enum,
- conversion from `std::io::Error` and `reqwest::Error`.

This avoids scattered `Box<dyn Error>` and stringly-typed errors.

---

# 7. Full prioritized checklist

## P0 checklist

- [ ] Search for malformed URL format strings.
- [ ] Replace escaped-brace URLs with plain URLs.
- [ ] URL-encode lrclib query parameters.
- [ ] Fix `post_auth` non-2xx handling.
- [ ] Replace startup cookie/client `.unwrap()`.
- [ ] Add clear missing-cookie error.
- [ ] Replace `play_file().unwrap()` at all call sites.
- [ ] Add mode-aware dependency checks for `mpv`, `yt-dlp`, and `ffmpeg`.
- [ ] Update README dependencies.
- [ ] Update README CLI option descriptions.

## P1 checklist

- [ ] Introduce structured input events or numeric input buffering.
- [ ] Fix playlist selection for 10+ playlists.
- [ ] Add autoplay generation token or cancellation token.
- [ ] Prevent stale recommendation tasks from mutating queue/cache.
- [ ] Add output lock as short-term rendering fix.
- [ ] Plan/implement single render owner as long-term fix.
- [ ] Introduce canonical `track_key`.
- [ ] Replace title-only lyric identity.
- [ ] Replace title-only history identity.
- [ ] Replace title/filename mismatch in offline anti-repeat.
- [ ] Add unique ID/hash to cache filenames.
- [ ] Preserve legacy offline filename compatibility.
- [ ] Replace hard-coded `/tmp/whytui.sock`.

## P2 checklist

- [ ] Support `80x24` terminal.
- [ ] Remove or lower `52x37` hard block.
- [ ] Remove unreachable terminal-size code.
- [ ] Remove lock-across-await in library shuffle.
- [ ] Improve lossless resolver scoring.
- [ ] Validate LRC timestamps.
- [ ] Support multi-timestamp LRC lines.
- [ ] Add lock poison recovery helpers or migrate to `parking_lot`.
- [ ] Replace stdout flush unwraps.
- [ ] Replace terminal-size unwraps.
- [ ] Optionally rewrite `split_title_artist` for clarity.
- [ ] Clean compiler warnings.
- [ ] Clean clippy warnings.
- [ ] Remove unused `src/app.rs` stub if truly unused.

## P3 checklist

- [ ] Add URL builder tests.
- [ ] Add auth error tests.
- [ ] Add cookie validation tests.
- [ ] Add track identity tests.
- [ ] Add cache filename tests.
- [ ] Add LRC parser tests.
- [ ] Add playlist selection tests.
- [ ] Add autoplay stale-task test.
- [ ] Add offline anti-repeat test.
- [ ] Add dependency failure tests.
- [ ] Add multi-instance IPC manual test instructions.
- [ ] Add rendering manual test instructions.

---

# 8. Suggested issue breakdown

If opening GitHub issues, use this split.

## Issue 1 — Fix malformed URL construction

Labels:

```text
bug, high-priority, networking
```

Includes:

- YouTube Music URL,
- lrclib URLs,
- URL encoding.

## Issue 2 — Fix auth and startup error handling

Labels:

```text
bug, high-priority, error-handling
```

Includes:

- `post_auth`,
- cookie/client startup unwrap,
- missing cookie messages.

## Issue 3 — Fix playback dependency and spawn errors

Labels:

```text
bug, medium-priority, playback
```

Includes:

- `play_file().unwrap()`,
- `mpv` preflight,
- dependency messages.

## Issue 4 — Fix playlist input handling

Labels:

```text
bug, high-priority, input
```

Includes:

- multi-digit selection,
- Enter handling,
- invalid selection feedback.

## Issue 5 — Fix autoplay stale task race

Labels:

```text
bug, high-priority, concurrency
```

Includes:

- generation token,
- cancellation,
- queue/cache mutation guard.

## Issue 6 — Fix terminal rendering synchronization

Labels:

```text
bug, high-priority, tui
```

Includes:

- output lock,
- single renderer plan,
- direct stdout audit.

## Issue 7 — Introduce canonical track identity

Labels:

```text
bug, architecture, medium-priority
```

Includes:

- lyrics,
- history,
- offline anti-repeat,
- cache filenames,
- autoplay exclusions.

## Issue 8 — Fix terminal sizing

Labels:

```text
bug, tui, medium-priority
```

Includes:

- support `80x24`,
- compact layout,
- safe terminal size fallback.

## Issue 9 — Improve lyrics parser

Labels:

```text
bug, lyrics, low-priority
```

Includes:

- timestamp validation,
- multi-timestamp lines.

## Issue 10 — Test suite and warning cleanup

Labels:

```text
testing, maintenance
```

Includes:

- unit tests,
- integration tests,
- clippy cleanup,
- dead code removal.

---

# 9. Final validation commands

After all fixes:

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

If `-D warnings` is too aggressive at first, use:

```bash
cargo clippy --all-targets --all-features
```

Then incrementally fix warnings until `-D warnings` is possible.

Manual runtime checks:

```bash
cargo run
cargo run -- --offline
cargo run -- --download
cargo run -- --help
```

Dependency checks:

```bash
PATH=/tmp/empty cargo run
```

Terminal size:

```bash
resize -s 24 80
cargo run
```

Multi-instance:

```bash
cargo run
# In another terminal:
cargo run
```

---

# 10. Definition of done

The bug-fix effort is complete when:

1. No malformed `{https://...}` URLs can be generated.
2. Non-2xx authenticated API responses return useful errors.
3. Missing/malformed cookies produce a clear startup message.
4. Missing `mpv`, `yt-dlp`, or `ffmpeg` produces a clear dependency message.
5. Playlist 10+ selection works.
6. Stale autoplay tasks cannot mutate the current queue/cache.
7. Terminal output is serialized or owned by one renderer.
8. `80x24` terminal usage is supported or gracefully handled.
9. Track identity is stable and not title-only.
10. Offline anti-repeat uses the same identity model as history/cache.
11. Same-title tracks do not share lyric state.
12. Cache filenames cannot collide on title + first artist alone.
13. mpv IPC socket is per-process/per-instance.
14. No lock guard is held across `.await`.
15. LRC parser rejects malformed timestamps and supports multi-timestamp lines.
16. Avoidable `.unwrap()` panics are removed from startup, playback, terminal, and lock paths.
17. README accurately documents dependencies and CLI behavior.
18. Regression tests cover the major fixed bugs.
19. `cargo check`, `cargo fmt`, `cargo clippy`, and `cargo test` pass.

---

# 11. Notes on rejected / lower-confidence findings

## `split_title_artist` UTF-8 panic

The automated Claude finding claiming a UTF-8 boundary panic in `split_title_artist` is not valid as written.

Reason:

- the code slices around ASCII bracket positions,
- ASCII bracket positions are UTF-8 boundaries,
- `start + 1` after ASCII `[` is also a UTF-8 boundary.

It can be cleaned up for readability, but it should not be treated as a confirmed high-severity bug.

## Exact compiler/clippy warning counts

The categories of warnings are real, but exact counts should be regenerated locally:

```bash
cargo check
cargo clippy --all-targets --all-features
```

Do not rely on stale warning counts after fixes begin.

---

# 12. Recommended implementation order in one sentence

Fix malformed URLs, auth/startup/playback panics, then playlist/autoplay/rendering, then canonical track identity and terminal/offline/lyrics cleanup, and finally add tests and warning cleanup.