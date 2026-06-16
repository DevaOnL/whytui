# final_plan.md — Correct Final Fix Plan for `whytui`

**Repository:** `https://github.com/DevaOnL/whytui`  
**Prepared for:** deva  
**Date:** 2026-06-17  
**Purpose:** Final, corrected, implementation-ready plan to fix the confirmed `whytui` bugs.  
**Inputs considered:** prior source audit, your reproduced findings, validation of `Claude_Findings.md`, the earlier `plan.md`, and the attached `Claude_Plan_Review.md`.

---

## 0. Non-negotiable correctness notes

Before implementation, keep these corrections in mind.

### 0.1 Correct Rust URL format strings

The malformed URL bug is caused by Rust escaped braces.

Wrong:

```rust
format!("{{https://music.youtube.com/watch?v={}}}", video_id)
```

This produces:

```text
{https://music.youtube.com/watch?v=VIDEO_ID}
```

Correct:

```rust
format!("https://music.youtube.com/watch?v={}", video_id)
```

The same applies to all lrclib URLs. They must start with `https://`, not `{https://`.

### 0.2 `split_title_artist` is not a confirmed UTF-8 panic

The automated Claude finding claiming a UTF-8 boundary panic in `split_title_artist` is **not valid as written**. The code slices around ASCII bracket positions, and ASCII bracket byte positions are valid UTF-8 boundaries.

You may still refactor the function for readability, but do not prioritize it as a real high-severity bug unless you can reproduce an actual panic.

### 0.3 Do not migrate to `parking_lot` casually

`parking_lot::RwLock` is not a one-line replacement if current code uses:

```rust
LOCK.read().unwrap();
LOCK.write().unwrap();
```

With `parking_lot`, `.read()` and `.write()` return guards directly. The `.unwrap()` calls must be removed. Use `std::sync::RwLock` recovery helpers first; consider `parking_lot` only later.

### 0.4 Add dependencies only when needed

Use the smallest safe dependency set:

- `urlencoding = "2"` or `percent-encoding` only if no URL encoding helper already exists.
- `tokio-util = "0.7"` only if using cancellation tokens for autoplay. Generation tokens need no new dependency.
- `tempfile`, `assert_cmd`, `wiremock`, etc. only as dev-dependencies when actual tests need them.
- Do not add `parking_lot` in the first pass.

### 0.5 Preserve user data

Track identity and cache filename changes can orphan existing downloaded files if done carelessly. The final plan requires legacy cache lookup and non-destructive migration.

---

# 1. Final priority overview

## P0 — Immediate high-confidence fixes

These are small, clear, high-impact fixes.

1. Fix malformed URL construction caused by escaped braces.
2. Fix authenticated API error handling in `post_auth`.
3. Replace startup cookie/client `.unwrap()` calls with contextual errors.
4. Handle `mpv`/`play_file()` spawn failures without panicking.
5. Add mode-aware dependency checks for `mpv`, `yt-dlp`, and `ffmpeg`.
6. Update README/runtime dependency documentation.

## P1 — Core product correctness fixes

These fix the main user-facing behavioral bugs.

1. Fix playlist selection for 10+ playlists.
2. Prevent stale autoplay tasks from mutating the current queue/cache.
3. Synchronize terminal rendering.
4. Introduce canonical track identity.
5. Apply canonical identity to offline anti-repeat, lyrics, history, and cache behavior.
6. Replace hard-coded mpv IPC socket with per-instance socket paths.

## P2 — Robustness and cleanup

1. Support normal `80x24` terminals or compact mode.
2. Remove terminal-size unwraps and unreachable code.
3. Remove lock-guard-across-`.await` patterns.
4. Improve lossless resolver scoring.
5. Fix duration/LRC parsing.
6. Add lock poisoning recovery helpers.
7. Remove minor stdout/terminal `.unwrap()` panic paths.
8. Clean warnings, dead code, and unused stubs.

## P3 — Tests, migration, docs, and release hardening

1. Add regression tests for all fixed bug classes.
2. Add migration/backward-compatibility tests for cache/state changes.
3. Add changelog/release notes.
4. Add CI/pre-commit checks if desired.
5. Add troubleshooting documentation.

---

# 2. Preparation and workflow

## 2.1 Branching

For a single comprehensive branch:

```bash
git checkout -b fix/whytui-stability-correctness
```

For reviewable branches:

```bash
git checkout -b fix/p0-url-auth-startup-playback
git checkout -b fix/p1-input-autoplay-rendering
git checkout -b fix/p1-track-identity-cache-offline
git checkout -b fix/p2-terminal-lyrics-locks
git checkout -b test/regression-suite
git checkout -b docs/runtime-and-release-notes
```

Recommended merge order:

1. `fix/p0-url-auth-startup-playback`
2. `fix/p1-input-autoplay-rendering`
3. `fix/p1-track-identity-cache-offline`
4. `fix/p2-terminal-lyrics-locks`
5. `test/regression-suite`
6. `docs/runtime-and-release-notes`

## 2.2 Commit message standard

Use Conventional Commits-style messages:

```text
fix(api): remove literal braces from generated URLs
fix(api): return errors for non-2xx authenticated responses
fix(playback): handle mpv spawn failures without panic
fix(input): support multi-digit playlist selection
fix(queue): discard stale autoplay recommendation tasks
fix(identity): use stable track keys for history and lyrics
test(lyrics): add LRC parser regression tests
docs(readme): document mpv yt-dlp and ffmpeg requirements
```

Each commit should state:

- what changed,
- why it changed,
- what finding/bug it fixes,
- tests added or run.

## 2.3 Capture baseline

Before editing:

```bash
git status
cargo fmt --check 2>&1 | tee baseline-fmt.log
cargo check 2>&1 | tee baseline-cargo-check.log
cargo clippy --all-targets --all-features 2>&1 | tee baseline-clippy.log
cargo test --all-targets --all-features 2>&1 | tee baseline-test.log
```

Record runtime tools:

```bash
which mpv || true
which yt-dlp || true
which ffmpeg || true
mpv --version || true
yt-dlp --version || true
ffmpeg -version || true
```

---

# 3. P0 — Immediate fixes

---

## P0.1 Fix malformed URL construction

### Bug

Some code constructs URLs using escaped braces:

```rust
format!("{{https://music.youtube.com/watch?v={}}}", video_id)
```

This emits a literal `{` and `}`.

### Files

Inspect:

- `src/api.rs`
- `src/features.rs`

Search:

```bash
rg -n '\{\{https?://|\{https?://|https?://.*\}' src
rg -n 'lrclib|music.youtube|youtube.com|yt-dlp|format!' src/api.rs src/features.rs
```

### Required implementation

YouTube Music URL:

```rust
let watch_url = format!("https://music.youtube.com/watch?v={}", video_id);
```

lrclib URL should also be plain and encoded:

```rust
let url = format!(
    "https://lrclib.net/api/get?track_name={}&artist_name={}&album_name={}&duration={}",
    urlencoding::encode(track_name),
    urlencoding::encode(artist_name),
    urlencoding::encode(album_name),
    duration
);
```

If the actual API parameter is `album` instead of `album_name`, keep the API-correct parameter. The important parts are:

- no literal braces,
- URL begins with `https://`,
- query values are URL-encoded.

### Dependency

If no encoder already exists:

```toml
urlencoding = "2"
```

Alternative: `percent-encoding`.

### Acceptance criteria

- No generated URL starts with `{https://`.
- `rg -n '\{\{https?://|\{https?://' src` finds no malformed URL construction.
- YouTube watch URL equals `https://music.youtube.com/watch?v=VIDEO_ID`.
- lrclib URLs encode spaces, ampersands, Unicode, slashes, and punctuation.

### Tests

Extract helpers if necessary:

```rust
fn build_youtube_music_url(video_id: &str) -> String {
    format!("https://music.youtube.com/watch?v={}", video_id)
}
```

Test:

```rust
#[test]
fn youtube_url_has_no_literal_braces() {
    let url = build_youtube_music_url("abc123");
    assert_eq!(url, "https://music.youtube.com/watch?v=abc123");
    assert!(!url.starts_with('{'));
    assert!(!url.ends_with('}'));
}
```

lrclib test:

```rust
#[test]
fn lrclib_url_encodes_query_values() {
    let url = build_lrclib_url("hello world", "a&b", "album/name", 123);
    assert!(url.starts_with("https://lrclib.net/"));
    assert!(!url.contains("{https://"));
    assert!(url.contains("a%26b"));
}
```

---

## P0.2 Fix `post_auth` non-2xx handling

### Bug

Current logic effectively does this:

```rust
let res = self.auth_client.post(endpoint).json(body).send().await?;
if !res.status().is_success() {}
Ok(res.json().await?)
```

The non-success branch is empty.

### File

- `src/api.rs`

Search:

```bash
rg -n 'post_auth|status\(\)\.is_success|res\.json' src/api.rs
```

### Required implementation

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
        let raw_body = res
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());

        let body = sanitize_error_body(&raw_body);

        return Err(format!(
            "Authenticated API request failed: {} {}: {}",
            status,
            endpoint,
            body
        )
        .into());
    }

    Ok(res.json().await?)
}
```

Add conservative sanitization:

```rust
fn sanitize_error_body(body: &str) -> String {
    let mut s = body.to_string();

    for key in ["token", "auth", "authorization", "cookie", "SAPISID", "HSID", "SSID"] {
        s = s.replace(key, "[redacted]");
    }

    const MAX_LEN: usize = 1000;
    if s.len() > MAX_LEN {
        s.truncate(MAX_LEN);
        s.push_str("...[truncated]");
    }

    s
}
```

### Acceptance criteria

- 401/403/429/500 responses return errors immediately.
- Non-JSON error bodies do not become misleading JSON parse failures.
- Error includes status and endpoint context.
- Error body is truncated/sanitized.

### Tests

Test status handling, ideally via a helper or mock HTTP server:

- 200 succeeds.
- 401 returns error containing `401`.
- 403 returns error containing `403`.
- 429 returns error containing `429`.
- 500 with HTML returns useful error.
- long body is truncated.

---

## P0.3 Replace startup cookie/client `.unwrap()` calls

### Bug

Startup may panic on:

```rust
cookies_path.to_str().unwrap()
api::YTMusic::new_with_cookies(...).unwrap()
```

### Files

- `src/main.rs`
- possibly `src/api.rs`

Search:

```bash
rg -n 'cookies_path|new_with_cookies|to_str\(\)\.unwrap|unwrap\(\)' src/main.rs src/api.rs
```

### Required implementation

Prefer a `run()` function returning `Result`:

```rust
#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("ERROR: {}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // app logic
    Ok(())
}
```

Replace unwraps:

```rust
if !cookies_path.exists() {
    return Err(format!(
        "cookies.txt not found at {:?}. Add your YouTube Music cookies file or run a mode that does not require authenticated features.",
        cookies_path
    )
    .into());
}

let cookies_str = cookies_path
    .to_str()
    .ok_or_else(|| format!("Cookies path is not valid UTF-8: {:?}", cookies_path))?;

let yt_client = api::YTMusic::new_with_cookies(cookies_str)
    .map_err(|e| format!("Failed to load YouTube Music cookies from {}: {}", cookies_str, e))?;
```

### Unix cookie permission warning

```rust
#[cfg(unix)]
fn warn_if_cookie_permissions_are_unsafe(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(metadata) = std::fs::metadata(path) {
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!("WARNING: cookie file is readable by other users: {:?}", path);
            eprintln!("Recommended: chmod 600 {:?}", path);
        }
    }
}
```

Call after confirming file exists.

### Cross-platform path note

If the project already uses `dirs`, prefer:

```rust
let config_dir = dirs::config_dir()
    .ok_or("Could not determine config directory")?;
let cookies_path = config_dir.join("whytui").join("cookies.txt");
```

If current behavior intentionally stores cookies elsewhere, keep it but document it.

### Acceptance criteria

- Missing cookies produce a clean error.
- Malformed/unreadable cookies produce a clean error.
- Invalid UTF-8 path does not panic.
- Cookie values are never printed.
- Unsafe Unix permissions warn.

---

## P0.4 Handle `play_file()` / `mpv` spawn failures

### Bug

Calls like this can panic:

```rust
player::play_file(&track.url, &track, music_dir).unwrap()
```

### Files

- `src/main.rs`
- `src/player.rs`

Search:

```bash
rg -n 'play_file\(|play_file\(.*unwrap|Command::new\("mpv"\)' src
```

### Required implementation at call sites

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

Ensure failed playback does **not**:

- update history as if playback succeeded,
- trigger autoplay,
- show “playing” status incorrectly.

### Improve `play_file()` error context

```rust
pub fn play_file(...) -> Result<Child, Box<dyn std::error::Error>> {
    let child = Command::new("mpv")
        // args...
        .spawn()
        .map_err(|e| format!("Failed to spawn mpv. Is mpv installed and in PATH? {}", e))?;

    Ok(child)
}
```

If spawning is the only fallible operation, `std::io::Result<Child>` is also good.

### Acceptance criteria

- Missing `mpv` does not panic.
- Playback failure shows a clear status/error.
- Autoplay/history are not updated after failed playback.
- Previous-track playback also handles failure.

---

## P0.5 Add mode-aware dependency checks

### Required helper

Create `src/external_tools.rs` or equivalent:

```rust
pub fn command_exists(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
```

Mode-aware validation:

```rust
fn validate_runtime_dependencies(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    if !command_exists("mpv") {
        return Err("Required dependency missing: mpv. Install mpv and try again.".into());
    }

    if !config.offline_mode && !command_exists("yt-dlp") {
        return Err("Required dependency missing for online playback: yt-dlp.".into());
    }

    if config.download_mode && !command_exists("ffmpeg") {
        return Err("Required dependency missing for downloads: ffmpeg.".into());
    }

    Ok(())
}
```

Adjust field names to the actual config.

### Acceptance criteria

- Online playback checks `mpv` and `yt-dlp`.
- Offline playback checks `mpv`.
- Download mode checks `yt-dlp` and `ffmpeg` if required by the actual path.
- Missing tools produce actionable messages.

---

## P0.6 Update README and CLI docs

### Required README content

```markdown
## Runtime dependencies

- `mpv` — required for audio playback.
- `yt-dlp` — required for resolving online YouTube Music streams.
- `ffmpeg` — required for downloading/converting/tagging saved tracks.
- YouTube Music cookies — required for authenticated YouTube Music features.
```

Clarify options:

```markdown
- `-d`, `--download` — download/save selected tracks for offline use.
- `-o`, `--offline` — play from the local offline library.
- `-n`, `--nomix` — disable mix/autoplay behavior.
- `-l`, `--lossless` — use lossless mode where available.
- `-pl`, `--peak-lossless` — use peak lossless behavior where available.
- `-g`, `--guess` — enable guessing behavior where applicable.
```

Only include options that actually exist and match behavior.

### Acceptance criteria

- README no longer says only `mpv` is required.
- `--download` and `--offline` are not conflated.
- Cookie setup path is documented.
- Supported platforms are stated honestly.

---

# 4. P1 — Core behavior fixes

---

## P1.1 Fix playlist selection for 10+ playlists

### Bug

Input thread sends digits one at a time. Playlist flows consume one message. `10` becomes `1`.

### Files

- `src/ui_common.rs`
- `src/main.rs`

Search:

```bash
rg -n 'spawn_input_handler|KeyCode::Char|is_ascii_digit|playlist|Select|recv\(\)' src
```

### Preferred implementation: structured input events

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

Input handler:

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

Number reader:

```rust
async fn read_number_selection(
    rx: &mut tokio::sync::mpsc::Receiver<InputEvent>,
    max: usize,
) -> Option<usize> {
    let mut buf = String::new();

    loop {
        match rx.recv().await? {
            InputEvent::Digit(c) => buf.push(c),
            InputEvent::Backspace => {
                buf.pop();
            }
            InputEvent::Enter => {
                let n: usize = buf.parse().ok()?;
                return (1..=max).contains(&n).then_some(n);
            }
            InputEvent::Esc | InputEvent::Char('q') => return None,
            _ => {}
        }
    }
}
```

If switching to `InputEvent` is too invasive, keep strings but buffer digits until explicit `"enter"`.

### Acceptance criteria

- Playlist 1, 9, 10, and 12 work.
- `0`, empty, out-of-range, and nonnumeric inputs fail safely.
- Esc/q cancels.
- Typing `10` never selects playlist 1.

---

## P1.2 Prevent stale autoplay queue mutations

### Bug

Old `queue_auto_add_online` tasks can mutate the current queue/cache after track changes.

### File

- `src/main.rs`

Search:

```bash
rg -n 'queue_auto_add_online|related|SONG_QUEUE|tokio::spawn|auto|recommend' src/main.rs
```

### Preferred implementation: generation token

```rust
use std::sync::atomic::{AtomicU64, Ordering};

static AUTOPLAY_GENERATION: AtomicU64 = AtomicU64::new(0);

fn start_new_autoplay_generation() -> u64 {
    AUTOPLAY_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}
```

On track/session change:

```rust
let generation = start_new_autoplay_generation();
```

In task:

```rust
let generation = AUTOPLAY_GENERATION.load(Ordering::SeqCst);

tokio::spawn(async move {
    let recommendations = fetch_recommendations(...).await;

    if AUTOPLAY_GENERATION.load(Ordering::SeqCst) != generation {
        return;
    }

    // mutate queue/cache only here
});
```

Cleaner signature:

```rust
async fn queue_auto_add_online(..., generation: u64) -> Result<(), Box<dyn std::error::Error>> {
    let recommendations = fetch_related(...).await?;

    if AUTOPLAY_GENERATION.load(Ordering::SeqCst) != generation {
        return Ok(());
    }

    // mutate queue/cache
    Ok(())
}
```

### Optional implementation: cancellation tokens

Only add `tokio-util = "0.7"` if choosing cancellation tokens. Generation tokens are simpler and sufficient.

### Acceptance criteria

- Track A’s old recommendation task cannot append after track B starts.
- Related-song cache cannot be overwritten by stale sessions.
- Rapid skip/previous does not leave stale recommendations.
- Manual queue changes are not overwritten by old tasks.

---

## P1.3 Synchronize terminal rendering

### Bug

Background monitor redraws while search/library/playlist flows print directly to stdout.

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

### Short-term implementation: output lock

```rust
use once_cell::sync::Lazy;
use std::sync::Mutex;

static OUTPUT_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

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
with_output_lock(|| {
    // execute!, print!, println!, queue!, flush
});
```

Rules:

- Hold lock only while writing terminal output.
- Never hold lock across `.await`.
- Never hold lock during network/file operations.

### Long-term implementation: single renderer

Create events:

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

Only the renderer writes to stdout.

### Acceptance criteria

- Search prompts do not interleave with monitor redraw.
- Library pages are not overwritten.
- Direct stdout writes are audited.
- No output lock is held across `.await`.

---

## P1.4 Introduce canonical track identity

### Bug family

Title/filename identity causes:

- offline anti-repeat failure,
- lyric state collision,
- recent-history / previous-track title collapse,
- cache filename collisions,
- weak autoplay exclusions.

### Files

- `src/main.rs`
- `src/player.rs`
- `src/offline.rs`
- `src/ui_common.rs`
- `src/ui1.rs`
- `src/ui2.rs`
- `src/ui3.rs`
- `src/api.rs` if `Track` is defined there

Search:

```bash
rg -n 'struct Track|title|artist|filename|file_stem|RECENTLY_PLAYED|CURRENT_LYRIC_SONG|history|cache|the_naming_format' src
```

### Implementation

Add `src/track_identity.rs`:

```rust
use crate::api::Track; // adjust module path

pub fn track_key(track: &Track) -> String {
    if let Some(id) = track_video_id(track) {
        return format!("yt:{}", normalize_key_part(id));
    }

    if !track.url.trim().is_empty() {
        return format!("url:{}", track.url.trim());
    }

    format!(
        "meta:{}:{}:{}",
        normalize_key_part(&track.title),
        normalize_key_part(track_artist(track)),
        normalize_key_part(track_album(track)),
    )
}

fn normalize_key_part(s: &str) -> String {
    s.trim().to_lowercase()
}
```

Adjust `track_video_id`, `track_artist`, and `track_album` to real fields. If `Track` has no `video_id`, add it or derive it from URL.

### Backward compatibility

Track identity must support:

1. new ID-based tracks,
2. old downloaded files named `title - artist`,
3. manually added local files,
4. metadata fallback.

### Acceptance criteria

- Same-title different tracks get different keys if IDs/URLs differ.
- Same track keeps same key across history, lyrics, queue, and cache.
- Offline file identity does not rely solely on filename stem.

---

## P1.5 Fix offline anti-repeat using canonical identity

### Implementation

Replace title exclusions with key exclusions.

```rust
fn get_excluded_track_keys() -> std::collections::HashSet<String> {
    RECENTLY_PLAYED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(track_identity::track_key)
        .collect()
}
```

For offline paths:

```rust
fn offline_file_key(path: &std::path::Path, metadata_track: Option<&Track>) -> String {
    if let Some(track) = metadata_track {
        return track_identity::track_key(track);
    }

    format!("file:{}", path.to_string_lossy())
}
```

### Acceptance criteria

- Recently played offline songs are not immediately requeued.
- `title - artist` filename mismatch no longer bypasses exclusion.
- Same-title different tracks are not incorrectly collapsed.

---

## P1.6 Fix lyric state keyed only by title

### Implementation

Replace:

```rust
static CURRENT_LYRIC_SONG: RwLock<String>
```

with:

```rust
static CURRENT_LYRIC_TRACK_KEY: RwLock<Option<String>>
```

Usage:

```rust
let key = track_identity::track_key(track);
let mut current = CURRENT_LYRIC_TRACK_KEY.write().unwrap_or_else(|e| e.into_inner());

if current.as_ref() != Some(&key) {
    *current = Some(key);
    // reload/reset lyrics
}
```

### Optional: per-track lyric offset

If `LYRIC_OFFSET` exists and is meant as a track sync correction, replace global offset with:

```rust
static LYRIC_OFFSETS: RwLock<HashMap<String, Duration>>;
```

### Acceptance criteria

- Same-title tracks with different artists/IDs do not reuse lyrics.
- Lyric monitor resets when track identity changes even if title is unchanged.
- Lyric offsets are per-track if that matches intended UX.

---

## P1.7 Fix history and previous-track identity

### Implementation

Before:

```rust
if list.back().map(|t| &t.title) != Some(&track.title) {
    list.push_back(track.clone());
}
```

After:

```rust
let new_key = track_identity::track_key(track);

if list
    .back()
    .map(|t| track_identity::track_key(t) != new_key)
    .unwrap_or(true)
{
    list.push_back(track.clone());
}
```

This preserves consecutive dedupe but uses correct identity.

### Acceptance criteria

- Same-title different tracks can appear separately.
- Exact same track is still deduped consecutively if intended.
- Previous-track command returns the actual previous track.

---

## P1.8 Fix cache filename collisions and preserve old files

### New filename format

```rust
fn cache_filename(track: &Track) -> String {
    let safe_title = sanitize_filename(&track.title);
    let safe_artist = sanitize_filename(track_artist(track));
    let id = stable_file_id(track);

    format!("{} - {} [{}].mp3", safe_title, safe_artist, id)
}
```

Stable ID:

```rust
fn stable_file_id(track: &Track) -> String {
    if let Some(id) = track_video_id(track) {
        return sanitize_filename(id);
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

### Legacy lookup order

When looking for cached/downloaded file:

1. Check new ID-based filename.
2. If missing, check legacy `title - artist` filename.
3. If legacy exists, use it.
4. Optionally copy/rename to new filename only after successful metadata match.
5. Do not delete old files automatically in the first release.

### Acceptance criteria

- Same title/artist but different video IDs produce different filenames.
- Existing downloaded files still play.
- Legacy files can be migrated or used in place.
- No destructive rename/delete without safe fallback.

---

## P1.9 Replace hard-coded mpv IPC socket

### Bug

All instances use the same socket path, e.g. `/tmp/whytui.sock`.

### File

- `src/player.rs`

Search:

```bash
rg -n 'whytui.sock|get_ipc_path|input-ipc-server|/tmp' src
```

### Implementation

```rust
use std::sync::atomic::{AtomicU64, Ordering};

static IPC_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn get_ipc_path() -> std::path::PathBuf {
    let n = IPC_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "whytui-{}-{}.sock",
        std::process::id(),
        n
    ))
}
```

`std::env::temp_dir()` is cross-platform. mpv IPC support should still be tested per platform.

### Acceptance criteria

- Two app instances use different sockets.
- One instance does not remove another instance’s socket.
- Socket cleanup happens after mpv exits where practical.

---

# 5. P2 — Robustness and cleanup

---

## P2.1 Support normal terminal sizes

### Bug

App hard-blocks at `52x37`, which breaks normal `80x24`.

### Search

```bash
rg -n '52|37|min_width|min_height|terminal::size|RESTRICTION|ENFORCE|Too small' src
```

### Implementation

Prefer adaptive layout:

```rust
enum LayoutMode {
    Normal,
    Compact,
    TooSmall,
}

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

If that is too large, lower the minimum height so `80x24` works.

Replace:

```rust
terminal::size().unwrap()
```

with:

```rust
terminal::size().unwrap_or((80, 24))
```

Remove unreachable code after `break`.

### Acceptance criteria

- App starts in `80x24`.
- Truly too-small terminals show a clean warning.
- No unreachable terminal-size-loop code remains.

---

## P2.2 Remove lock guards across `.await`

### Search

```bash
rg -n 'read\(\)|write\(\)|await|shuffle|library' src/main.rs
```

Manual inspection required.

### Pattern

Bad:

```rust
let library = LIBRARY.read().unwrap();
let selected = choose(&library);
play(selected).await;
```

Good:

```rust
let selected = {
    let library = LIBRARY.read().unwrap_or_else(|e| e.into_inner());
    choose(&library).cloned()
};

if let Some(track) = selected {
    play(track).await;
}
```

### Acceptance criteria

- No `std::sync` lock guard lives across `.await`.
- Clippy no longer reports `await_holding_lock`.

---

## P2.3 Improve lossless resolver scoring

### File

- `src/flac.rs`

Search:

```bash
rg -n 'duration|flac|search|score|candidate|artist|title|album' src/flac.rs
```

### Implementation

Use weighted scoring:

```rust
let total =
    title_score * 0.40 +
    artist_score * 0.30 +
    album_score * 0.15 +
    duration_score * 0.15;
```

Reject weak candidates:

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

Tune with tests.

### Acceptance criteria

- Exact duration but wrong title/artist is rejected.
- Same title/artist with slight duration difference is accepted.
- Resolver logs or exposes why a candidate was chosen.

---

## P2.4 Fix duration and LRC parsing

### Known issues

- LRC parser accepts malformed timestamps.
- Multi-timestamp LRC lines may not be handled.
- Some duration parser may support only `mm:ss`, not `h:mm:ss`.

### Search

```bash
rg -n 'parse_lrc|parse_timestamp|duration_to_seconds|Duration|lyrics' src
```

### Timestamp parser

```rust
fn parse_timestamp(ts: &str) -> Option<std::time::Duration> {
    let (min_str, sec_str) = ts.split_once(':')?;

    let minutes: u64 = min_str.parse().ok()?;
    let seconds: f64 = sec_str.parse().ok()?;

    if !seconds.is_finite() || !(0.0..60.0).contains(&seconds) {
        return None;
    }

    if minutes > 12 * 60 {
        return None;
    }

    let minute_ms = minutes.checked_mul(60_000)?;
    let second_ms = (seconds * 1000.0).round() as u64;
    let total_ms = minute_ms.checked_add(second_ms)?;

    Some(std::time::Duration::from_millis(total_ms))
}
```

### `h:mm:ss` duration support

```rust
fn duration_to_seconds(s: &str) -> Option<u64> {
    let parts: Vec<_> = s.split(':').collect();

    match parts.as_slice() {
        [m, sec] => {
            let m: u64 = m.parse().ok()?;
            let sec: u64 = sec.parse().ok()?;
            (sec < 60).then_some(m * 60 + sec)
        }
        [h, m, sec] => {
            let h: u64 = h.parse().ok()?;
            let m: u64 = m.parse().ok()?;
            let sec: u64 = sec.parse().ok()?;
            (m < 60 && sec < 60).then_some(h * 3600 + m * 60 + sec)
        }
        _ => None,
    }
}
```

### Multi-timestamp LRC lines

A line like:

```text
[00:10.00][00:20.00]same lyric
```

must produce two lyric entries.

### Acceptance criteria

- Rejects `NaN`, `inf`, negative seconds, seconds >= 60, and absurd minutes.
- Supports multi-timestamp lines.
- Supports `mm:ss` and `h:mm:ss` where applicable.

---

## P2.5 Add lock poisoning recovery helpers

### Search

```bash
rg -n '\.read\(\)\.unwrap\(\)|\.write\(\)\.unwrap\(\)' src
```

### Implementation

```rust
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

pub fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

pub fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}
```

Replace direct lock unwraps where practical.

### Acceptance criteria

- No avoidable lock unwraps remain.
- Poisoned locks do not immediately cascade panic.
- No helper lock guard is held across `.await`.

---

## P2.6 Remove minor I/O and terminal unwraps

### Search

```bash
rg -n 'flush\(\)\.unwrap|terminal::size\(\)\.unwrap|crossterm::terminal::size\(\)\.unwrap' src
```

### Implementation

For stdout flush:

```rust
let _ = io::stdout().flush();
```

or:

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

- No stdout flush unwrap remains.
- No terminal-size unwrap remains.

---

## P2.7 Optional cleanup: `split_title_artist`

### Status

Not a confirmed high-severity bug. Optional readability cleanup.

```rust
pub fn split_title_artist(input: &str) -> (String, String) {
    let input = input.trim();

    if let Some((title, rest)) = input.rsplit_once('[') {
        if let Some(artist) = rest.strip_suffix(']') {
            return (title.trim().to_string(), artist.trim().to_string());
        }
    }

    (input.to_string(), String::new())
}
```

Test:

```rust
#[test]
fn split_title_artist_handles_cjk() {
    let (title, artist) = split_title_artist("日本語Song [アーティスト]");
    assert_eq!(title, "日本語Song");
    assert_eq!(artist, "アーティスト");
}
```

---

## P2.8 Clean warnings and dead code

Run:

```bash
cargo check
cargo clippy --all-targets --all-features
rg -n 'unreachable|todo!|unimplemented!|allow\(dead_code\)|IS_LOSSLESS|unused' src
```

Fix:

- unreachable code after `break`,
- unused `src/app.rs` stub if truly unused,
- unused variables/imports,
- ignored `Result`s where meaningful.

Avoid blanket `#[allow(...)]` unless justified.

### Acceptance criteria

- `cargo check` has no avoidable warnings.
- `cargo clippy --all-targets --all-features` is clean or only has documented intentional warnings.

---

# 6. Migration and backward compatibility

## 6.1 Inventory state/cache before identity changes

Before P1.4/P1.8, determine:

1. Where downloaded tracks are stored.
2. Where cached tracks are stored.
3. Whether history is persisted.
4. Whether lyric state/cache is persisted.
5. Whether playlist/queue state is persisted.

Search:

```bash
rg -n 'config_dir|data_dir|cache|download|history|recent|lyrics|serde_json|toml|write_to|read_to_string' src
```

## 6.2 Cache migration strategy

Do not delete legacy files.

Lookup order:

1. New ID-based filename.
2. Legacy `title - artist` filename.
3. Metadata scan fallback if necessary.

Optional migration on access:

1. If legacy file is found and identity is known,
2. copy/rename to new filename,
3. verify destination exists and is readable,
4. leave old file in place for one release or until explicit cleanup.

## 6.3 State versioning if persistent state exists

If state is persisted, add versioning:

```json
{
  "version": 2,
  "track_identity_model": "video_id_or_url",
  "cache_format": "v2"
}
```

Migration skeleton:

```rust
const CURRENT_STATE_VERSION: u32 = 2;

fn migrate_state(mut state: State) -> Result<State, Box<dyn std::error::Error>> {
    while state.version < CURRENT_STATE_VERSION {
        match state.version {
            1 => {
                migrate_v1_to_v2(&mut state)?;
                state.version = 2;
            }
            _ => return Err(format!("Unknown state version: {}", state.version).into()),
        }
    }

    Ok(state)
}
```

If no persistent state exists, document that no state migration is required.

---

# 7. Testing infrastructure

## 7.1 Minimal dev dependencies

Add only what is used:

```toml
[dev-dependencies]
tempfile = "3"
```

Optional if needed:

```toml
assert_cmd = "2"
predicates = "3"
wiremock = "0.6"
```

Avoid adding mocking frameworks unless the code is refactored to use them.

## 7.2 Unit tests

Add tests for:

### URL construction

- YouTube URL has no braces.
- lrclib URL has no braces.
- query values are encoded.

### Auth handling

- 401/403/429/500 return errors.
- non-JSON error body does not get parsed as success.
- long/sensitive body is sanitized/truncated.

### Startup/cookies

- missing cookie path returns error.
- invalid cookie format returns error.
- unsafe Unix permissions produce warning if testable.

### Playlist selection

- `1`, `9`, `10`, `12` parse correctly.
- `0`, out-of-range, and nonnumeric input fail safely.

### Autoplay race

Use fake recommendation provider:

1. generation 1 starts,
2. generation 2 starts,
3. generation 1 completes late,
4. queue has no stale generation 1 recommendations.

### Track identity

- same title + different video ID gives different keys.
- same video ID gives same key.
- URL fallback works.
- metadata fallback works.

### Offline anti-repeat

- recently played file is excluded using canonical key.
- filename stem mismatch does not bypass exclusion.

### Cache filename

- same title/artist but different IDs gives different filenames.
- legacy filename still resolves.

### LRC parser

- valid timestamps.
- invalid seconds.
- `NaN`/`inf`.
- huge minutes.
- multi-timestamp lines.
- `h:mm:ss` duration parsing.

## 7.3 Manual regression tests

### Terminal size

```bash
resize -s 24 80
cargo run
```

Expected: app starts or clean compact UI appears.

### Missing dependencies

```bash
PATH=/tmp/empty cargo run
```

Expected: clean dependency error, no panic.

### Multi-instance IPC

Run two app instances.

Expected: separate IPC sockets; neither kills the other.

### Rendering

While playback monitor is active:

- open library,
- search,
- change pages,
- view lyrics.

Expected: no interleaved prompts/output.

### Playlist 10+

Use an account/library with at least 12 playlists. Select playlist 10.

Expected: playlist 10 selected, not playlist 1.

### Autoplay race

Rapidly skip tracks while recommendations are loading.

Expected: queue contains recommendations only for the current session.

---

# 8. Documentation and release notes

## 8.1 README updates

Include:

- runtime dependencies,
- install commands for Linux/macOS where appropriate,
- cookie setup,
- correct CLI option descriptions,
- supported platform statement,
- troubleshooting section.

## 8.2 Add `CHANGELOG.md` if missing

```markdown
# Changelog

## [Unreleased]

### Fixed

- Fixed malformed generated URLs containing literal braces.
- Fixed authenticated API error handling for non-2xx responses.
- Fixed startup panics on missing or invalid cookies.
- Fixed playback panic when `mpv` cannot be spawned.
- Fixed playlist selection for 10+ playlists.
- Fixed stale autoplay recommendation tasks mutating the active queue.
- Fixed terminal output interleaving between monitor and command flows.
- Fixed title-only identity bugs in lyrics/history/offline/cache behavior.
- Fixed hard-coded mpv IPC socket collisions.
- Fixed normal `80x24` terminal support.

### Changed

- New downloads use stable ID-based cache filenames.
- Legacy downloaded files remain supported.
- Runtime dependency errors are now user-friendly.
```

## 8.3 Add code comments for architectural rules

Near identity helper:

```rust
// Track identity rule:
// Never use title alone as track identity. Use video_id, URL, or metadata fallback.
```

Near rendering helper:

```rust
// Terminal output rule:
// Only the renderer or code inside with_output_lock may write to stdout.
```

Near autoplay generation:

```rust
// Autoplay lifecycle rule:
// Background recommendation tasks must verify generation before mutating queue/cache.
```

---

# 9. Security and privacy considerations

## 9.1 Cookies

- Cookies are sensitive.
- Do not print cookie contents.
- Warn on unsafe Unix permissions.
- Document recommended permissions:

```bash
chmod 600 ~/.config/whytui/cookies.txt
```

## 9.2 Error bodies

When surfacing HTTP error bodies:

- truncate long bodies,
- redact obvious token/cookie strings,
- avoid dumping request headers.

## 9.3 HTTPS

Confirm all network calls use HTTPS unless intentional local IPC/file usage.

Search:

```bash
rg -n 'http://' src
```

Any `http://` usage should be justified or changed to `https://`.

---

# 10. Performance considerations

## 10.1 Rendering lock

The output mutex is a short-term correctness fix.

Rules:

- hold only while writing terminal output,
- never hold across `.await`,
- never hold during network/file operations,
- consider replacing with single render loop later.

## 10.2 Track identity lookups

If library size is small, computing keys on demand is fine.

If library can exceed roughly 10k tracks, add indexes:

```rust
HashMap<String, Track>
HashSet<String>
```

Use precomputed keys in history/recent sets if needed.

## 10.3 Offline library scans

Avoid scanning the whole filesystem repeatedly for cache lookup. Build an in-memory index once per library refresh.

---

# 11. Final validation commands

Run after each phase:

```bash
cargo fmt --check
cargo check
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features
```

Before final merge, aim for:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

Manual checks:

```bash
cargo run -- --help
cargo run
cargo run -- --offline
cargo run -- --download
```

Dependency failure check:

```bash
PATH=/tmp/empty cargo run
```

Search checks:

```bash
rg -n '\{\{https?://|\{https?://' src
rg -n '\.read\(\)\.unwrap\(\)|\.write\(\)\.unwrap\(\)' src
rg -n 'flush\(\)\.unwrap|terminal::size\(\)\.unwrap' src
rg -n 'title.*==|==.*title|CURRENT_LYRIC_SONG|get_excluded_titles' src
```

Each remaining result must be reviewed and justified.

---

# 12. Final definition of done

The work is complete when:

1. No generated URL contains literal `{https://...}` braces.
2. lrclib query parameters are URL-encoded.
3. `post_auth` returns clear errors for non-2xx responses.
4. Startup missing/malformed cookies produce clear errors, not panics.
5. Cookie contents are never printed; unsafe permissions warn on Unix.
6. Missing `mpv`, `yt-dlp`, or `ffmpeg` produces clear dependency errors.
7. `play_file()` failures do not panic or update playback state incorrectly.
8. README documents real runtime dependencies and correct CLI behavior.
9. Playlist 10+ selection works.
10. Stale autoplay tasks cannot mutate current queue/cache.
11. Terminal output is serialized or owned by a single renderer.
12. App supports `80x24` or gives a clean compact-mode experience.
13. Track identity is stable and not title-only.
14. Offline anti-repeat uses canonical identity.
15. Lyrics are keyed by track identity, not title.
16. History/previous-track behavior uses track identity.
17. Cache filenames cannot collide on title + first artist alone.
18. Legacy downloaded files still work.
19. mpv IPC socket is per-process/per-instance.
20. No lock guard is held across `.await`.
21. LRC parser rejects malformed timestamps and supports multi-timestamp lines.
22. `h:mm:ss` duration parsing works where applicable.
23. Avoidable `.unwrap()` panics are removed from startup, playback, lock, stdout, and terminal-size paths.
24. Compiler/clippy warnings are cleaned or explicitly justified.
25. Regression tests cover URL building, auth errors, playlist selection, autoplay race, track identity, offline anti-repeat, cache naming, and LRC parsing.
26. `cargo fmt`, `cargo check`, `cargo test`, and `cargo clippy` pass.
27. CHANGELOG/release notes explain user-facing changes and migration behavior.

---

# 13. Exact implementation sequence

1. Create branch and capture baseline logs.
2. Fix malformed URL strings and add URL tests.
3. Fix `post_auth` and add error tests.
4. Fix startup cookie/client errors and add cookie validation tests.
5. Fix `play_file()` error handling and dependency checks.
6. Update README runtime/CLI docs.
7. Fix playlist input buffering and tests.
8. Add autoplay generation token and stale-task tests.
9. Add terminal output lock and audit direct stdout writes.
10. Add canonical track identity helper and tests.
11. Apply canonical identity to offline anti-repeat.
12. Apply canonical identity to lyrics.
13. Apply canonical identity to history/previous-track.
14. Update cache filename format with legacy fallback.
15. Replace hard-coded mpv IPC socket.
16. Fix terminal-size policy and safe terminal-size fallback.
17. Remove lock-across-await.
18. Improve lossless resolver scoring.
19. Fix LRC parser and duration parsing.
20. Add lock poisoning helpers.
21. Remove minor I/O unwraps.
22. Clean warnings/dead code.
23. Add/finish regression tests.
24. Add CHANGELOG and migration notes.
25. Run final validation commands.
26. Manually verify key runtime flows.
27. Merge/release.

---

# 14. Summary

This final plan keeps the useful structure from the earlier plan and incorporates the valid points from `Claude_Plan_Review.md`:

- workflow and commit standards,
- dependency caution,
- state/cache migration,
- testing infrastructure,
- documentation/release notes,
- performance considerations,
- error-handling standards,
- cross-platform notes,
- security considerations,
- rollback/backward compatibility thinking,
- and known lower-priority issues.

It also corrects the most important mistakes and ambiguities:

- the URL fix must remove escaped braces entirely,
- `split_title_artist` is not a confirmed UTF-8 panic,
- `parking_lot` should not be introduced casually,
- cache/identity changes must preserve old user files,
- and new dependencies should be added only when actually needed.

Follow the P0 → P1 → P2 → P3 order. That sequence fixes the most obvious breakages first, then the core architectural causes, then the resilience and testing gaps.
