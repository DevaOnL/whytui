# whytui — Consolidated Bug Report & Validation
​
> Repository: https://github.com/DevaOnL/whytui (Rust TUI music player)
> Review date: 2026-06-04
> Method: full static source reading of every file at HEAD. **No build/run** was possible (sandbox had no network or Rust toolchain access), so any claim that depends on compiler/clippy/runtime output is explicitly marked as *unverified-by-execution*.
> Purpose: validate a set of pre-existing findings, re-validate from scratch, and record everything (including fixes) for later evaluation.
​
---
​
## 0. How to read this document
​
- **Section 1** = verdicts on the 15 pre-existing findings (the "My FINDINGS" list), each with evidence and a fix.
- **Section 2** = NEW findings surfaced during re-validation that were in neither the original AI analysis nor the pre-existing list.
- **Section 3** = additional findings from the first AI pass (volume, unicode width, scroll state, LRC parsing, etc.).
- **Section 4** = fix priority.
- **Section 5** = what still needs a real toolchain to confirm.
- **Section 6** = project/runtime reference for reproduction.
​
Severity legend: 🔴 high · 🟠 medium · 🟡 low · ⚪ nit. “Verdict” is one of VALID / VALID (with nuance) / PARTLY VERIFIABLE / NOT A BUG.
​
---
​
## 1. Validation of pre-existing findings
​
### F1 — 🔴 Hard block unless terminal ≥ 52×37 — **VALID (with nuance)**
`main.rs` PART 3:
```rust
let min_width = 52;
let min_height = 37;
loop {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
    if cols >= min_width && rows >= min_height {
        break;
        execute!(stdout(), Clear(ClearType::All)); // unreachable
    }
    execute!(stdout(), Clear(ClearType::All));
    println!("Breh Terminal too small!");
    ...
    std::thread::sleep(Duration::from_millis(500));
}
```
- A standard 80×24 terminal has 24 rows < 37 → the app spins forever on “Breh Terminal too small!”. Confirmed.
- **Nuance:** the size gate is *intentional* (labelled `RESTRICTION: ENFORCE MINIMUM TERMINAL SIZE`). So the *enforcement* is by design; what's buggy is (a) the 37-row minimum being larger than the universal 80×24 default, and (b) the **unreachable** `execute!` after `break`.
- **Fix:** lower/justify the minimum (e.g. 24 rows), and move the clear before the break: `execute!(stdout(), Clear(ClearType::All)).ok(); break;`
​
### F2 — 🔴 Playlist selection breaks for 10+ playlists — **VALID**
The raw input handler sends each digit as its own message:
```rust
KeyCode::Char(c) if c.is_ascii_digit() => { let _ = tx.send(c.to_string()); }
```
Both playlist selection flows consume exactly **one** message:
- `handle_global_commands` → `"a"`/add flow: `if let Ok(sel_str) = rx.recv() { let sel = sel_str.trim().parse::<usize>()... }`
- `handle_library_browsing`: `let sel_str = match rx.recv() {...}; let sel = sel_str.trim().parse::<usize>()...`
So typing `10` sends `"1"` then `"0"`: the flow reads `"1"` (selects playlist #1) and the stray `"0"` is processed later as a separate command. Any library/playlist with ≥10 entries is unreachable past #9.
- Note: in-page **song** selection is safe because `PAGE_SIZE = 5` (always single-digit), and search shows only 5 results. The bug is specific to the **playlist list**, which is unbounded.
- **Fix:** buffer digits into a line (or read until a terminator/timeout) before parsing the selection, or render a single-key selector.
​
### F3 — 🔴 Autoplay queueing is race-prone (stale leak) — **VALID**
`queue_auto_add_online` is always launched detached:
```rust
tokio::spawn(async move { queue_auto_add_online(yt, v).await; });
```
Its guards (`q.len() < 2`, `cache_empty`) are evaluated **once at the top**, then it `.await`s network calls and per-track `yt-dlp` resolution, and finally `queue_add(...)`s. There is **no cancellation token or generation counter**. When the user picks a new song, `handle_song_selection` does:
```rust
SONG_QUEUE.write().unwrap().clear();
RELATED_SONG_LIST.write().unwrap().clear();
tokio::spawn(async move { queue_auto_add_online(yt, vid).await; });
```
but a **previously-spawned** task that is mid-`.await` can still push its now-stale tracks into `SONG_QUEUE`/`RELATED_SONG_LIST` after the clear → recommendations from the old song leak into the new session.
- **Fix:** add a monotonic “play generation” (AtomicU64); capture it at spawn and re-check before every `queue_add`; or hold a `JoinHandle`/`AbortHandle` and abort the previous task before spawning a new one.
​
### F4 — 🔴 Rendering is unsynchronized — **VALID**
The monitor thread (`start_monitor_thread`) redraws on a timer:
```rust
thread::spawn(move || {
    while !stop_clone.load(Ordering::Relaxed) {
        ... draw_callback(...);
        thread::sleep(Duration::from_millis(300));
    }
});
```
Meanwhile the main thread writes directly to stdout via `show_songs`, `show_playlists`, the library pager `println!`s, and the `/` search prompt loop. There is no shared stdout mutex or single render owner, so prompts, lyric frames, and library pages interleave. Confirmed.
- **Fix:** funnel all terminal writes through one renderer/`Mutex<Stdout>`, or pause the monitor while interactive prompts are active.
​
### F5 — 🟠 Offline anti-repeat is broken — **VALID**
Exclusions are built from the tag title:
```rust
// offline.rs::get_excluded_titles
titles.extend(history.iter().map(|t| t.title.clone()));
titles.extend(queue.iter().map(|t| t.title.clone()));
```
but selection filters by **filename stem**:
```rust
// offline.rs::get_random_batch
let title = p.file_stem().unwrap_or_default().to_string_lossy();
!exclude_titles.contains(&title.to_string())
```
Downloaded files are named `"{title} - {artist}"` (see `player.rs::the_naming_format_in_which_i_have_saved_the_track_locally`). When tags are present, `Track.title` is the bare tag title (e.g. `Song`), while the file stem is `Song - Artist`, so `contains` never matches → recently played songs can be re-queued. Confirmed.
- **Fix:** compare on the same key — either exclude by file stem (derive the `"title - artist"` form) or filter offline candidates by their parsed tag title.
​
### F6 — 🟠 Library shuffle holds a read lock across `.await` — **VALID**
```rust
"s" => {
    let list = LIBRARY_SONG_LIST.read().unwrap();   // std::sync::RwLock guard
    if let Some(song) = list.choose(&mut rand::rng()) {
        handle_song_selection("1".into(), &[song.clone()], ...).await?;  // held across await
        break;
    }
}
```
The numeric branch correctly clones + drops first (`...get(song_idx).cloned()`); the shuffle branch does not. Clippy's `await_holding_lock` applies. No deadlock today (the awaited path doesn't lock `LIBRARY_SONG_LIST`), but it blocks other readers/writers across a long async call and is fragile.
- **Fix:** `let song = { let l = LIBRARY_SONG_LIST.read().unwrap(); l.choose(&mut rand::rng()).cloned() };` then await outside the guard.
​
### F7 — 🟠 Lyric state keyed only by song title — **VALID**
`CURRENT_LYRIC_SONG: RwLock<String>` holds just the title. The UIs gate the monitor on title equality:
```rust
// ui1.rs / ui2.rs / ui3.rs
if *current_song_guard != track.title {
    *current_song_guard = track.title.clone();
    ... start_monitor_thread(track.clone(), draw_*);
}
```
and the fetcher dedups on title too (`if *CURRENT_LYRIC_SONG.read().unwrap() != track.title { ...clear; return; }`). Consequences: two different tracks that share a title won't trigger a monitor restart / lyric refetch, and they can reuse or overwrite each other's lyrics. Confirmed across `ui1`, `ui2`, `ui3`, `ui_common`.
- **Fix:** key by a stable identity (video_id / url), not the display title.
​
### F8 — 🟠 Recent-history / previous-track dedupe by title — **VALID (with nuance)**
```rust
fn add_to_history(track: Track) {
    let mut list = RECENTLY_PLAYED.write().unwrap();
    if let Some(last) = list.back() {
        if last.title == track.title { return; }   // dedupe vs MOST RECENT only
    }
    ...
}
```
- **Nuance:** this is **consecutive** dedupe (only compares against `back()`), not global. So two different songs sharing a title collapse **only when played back-to-back**; non-adjacent same-title songs both remain. Still wrong because it keys on title rather than identity.
- `get_prev_track` just `pop_back()`s, so “previous” inherits the same title-identity weakness.
- **Fix:** dedupe/compare on video_id/url.
​
### F9 — 🟠 Local cache filenames collide on title + first artist — **VALID**
```rust
// player.rs
pub fn the_naming_format_in_which_i_have_saved_the_track_locally(title: &str, artists: &[String]) -> String {
    let safe_title = title.replace(['/', '\\'], "-");
    let primary_artist = artists.get(0).map(...).unwrap_or_else(|| "Unknown".to_string());
    format!("{} - {}", safe_title, primary_artist)   // only artists[0]
}
```
Used both for the cache-hit check (`handle_song_selection` builds `{safe_title}.opus`/`.flac`) and for the saved file path (`background_download`). Remasters, live versions, or different album variants with the same title and lead artist map to the same filename → one masquerades as / overwrites the other. Confirmed.
- **Fix:** include a disambiguator (album and/or video_id) in the filename.
​
### F10 — 🟠 Hard-coded mpv IPC socket — **VALID**
```rust
pub fn get_ipc_path() -> String {
    if cfg!(unix) { "/tmp/whytui.sock".to_string() } else { r"\\.\pipe\whytui.sock".to_string() }
}
```
And `play_file` does `let _ = std::fs::remove_file(&ipc);` on every start. Two instances share one socket path and delete/recreate it under each other → IPC (seek/volume/pause/time) gets misrouted between processes. Confirmed.
- **Fix:** make the socket path per-process (PID/random suffix) or per-instance.
​
### F11 — 🟠 README is materially stale — **VALID**
From `README.md`:
- “**Requirements:** `mpv` installed and in PATH” — but online playback shells out to **`yt-dlp`** (`api.rs::fetch_stream_url`) and saving/tagging needs **`ffmpeg`** (`player.rs::background_download`). Neither is mentioned.
- Argument docs are wrong/misleading: it lists `-d | --download to just play offline songs` **and** `-o | --offline to just play offline songs`. In code, `-d/--download` is *download-while-playing-online* (`download_mode`), and only `-o/--offline` is true offline playback.
- Also undocumented: `-pl/--peak-lossless`.
- **Fix:** document `yt-dlp` + `ffmpeg` requirements; correct the `--download` description; add `--peak-lossless`.
​
### F12 — 🟠 Lossless resolver is too duration-driven — **VALID**
```rust
// flac.rs::fetch_flac_stream_url
let first = &items[0];
let first_dur = first["duration"].as_i64().unwrap_or(0);
if (first_dur - target_secs).abs() <= 3 {        // first: +/-3s
    Some(first)
} else {
    items.iter().skip(1).find(|item| {
        (item["duration"].as_i64().unwrap_or(0) - target_secs).abs() <= 1   // rest: +/-1s
    })
}
```
Selection relies purely on duration proximity with **no title/artist verification**, and the tolerance is **asymmetric** (±3s for the first result, ±1s for fallbacks). On noisy third-party search ranking this readily picks the wrong FLAC. Confirmed.
- **Fix:** verify title/artist similarity alongside duration; use one consistent tolerance.
​
### F13 — 🟡 `post_auth` ignores non-2xx statuses — **VALID**
```rust
async fn post_auth(&self, endpoint: &str, body: &Value) -> Result<Value, ...> {
    let res = self.auth_client.post(endpoint).json(body).send().await?;
    if !res.status().is_success() {}        // empty block: does nothing
    Ok(res.json().await?)
}
```
Error statuses are detected and discarded; it then tries to parse the error body as the expected JSON. Affects search, like, add-to-playlist, account name, library, continuation, related. Confirmed.
- **Fix:** `if !res.status().is_success() { return Err(format!("{} -> HTTP {}", endpoint, res.status()).into()); }`
​
### F14 — 🟡 Noisy hygiene (33 compiler / 108 clippy warnings) — **PARTLY VERIFIABLE**
- The **categories** are confirmed by reading: unreachable code (F1), ignored `#[must_use]` on bare `execute!(...)` calls, dead `static IS_LOSSLESS` (real one is `PLAYING_LOSSLESS`), unused `let title = ...` in the `"l"` like-flow, unused bound `e` in `"Error in Library: {}"`, unused reads `status_line`/`current_status_line` in `get_banner_art`, `useless format!` (`format!("QUEUE CLEARED")` etc.), and the unused `app.rs` module (`App`/`InputMode`/`CurrentScreen`).
- The **exact counts (33 and 108) cannot be confirmed here** — no `cargo`/`clippy` run was possible. Treat the numbers as the author's local measurement; the kinds of warnings are real.
- **Fix:** clear warnings; `#![deny(warnings)]` in CI; run `cargo clippy --all-targets -- -D warnings`.
​
### F15 — 🟡 No automated tests — **VALID**
No `#[test]`, `#[cfg(test)]`, or `tests/` directory exists anywhere in the source tree, so `cargo test` runs 0 tests. Queueing, lyric parsing, pagination, offline refill, and auth are all manual-only. Confirmed structurally.
- **Fix:** add unit tests for pure logic first: `parse_lrc`, `parse_timestamp`, `duration_to_seconds`, `the_naming_format...`, `blindly_trim`, `get_excluded_titles` matching, and `parse_to_seconds`.
​
---
​
## 2. NEW findings from this re-validation pass (not in either prior list)
​
### N1 — 🔴 URLs wrapped in literal `{ }` braces via ``/`` mis-escaping — **VALID (execution-unverified)**
In a Rust `format!` string, `{{` renders a literal `{` and `}}` a literal `}`. Several URLs are accidentally wrapped:
```rust
// features.rs::fetch_synced_lyrics (all three lrclib URLs)
format!("{{https://lrclib.net/api/get?track_name={}}}&artist_name={}&album={}&duration={}", ...)
// => "{https://lrclib.net/api/get?track_name=SONG}&artist_name=..."   (leading '{', stray '}')
format!("{{https://lrclib.net/api/search?track_name={}}}&artist_name={}", ...)
// => "{https://lrclib.net/api/search?track_name=SONG}&artist_name=..."
​
// api.rs::fetch_stream_url
let video_url = format!("{{https://music.youtube.com/watch?v={}}}", video_id);
// => "{https://music.youtube.com/watch?v=VIDEO_ID}"  (passed to yt-dlp)
```
By `format!` semantics these strings literally begin with `{` and contain a stray `}`. A URL starting with `{` is not a valid URL for `reqwest`, and `{https://...watch?v=ID}` is not a URL `yt-dlp` will accept. This would break lyric fetching and likely YouTube stream resolution.
- **Caveat:** I could not execute the binary to confirm runtime failure; this is derived from the unambiguous string-formatting rules. If the app does play audio in practice, this commit may be mid-refactor, the braces may be tolerated by some normalization step I can't see, or these paths are bypassed — worth a 1-line runtime check.
- **Fix:** remove the wrapping braces:
  - `format!("https://lrclib.net/api/get?track_name={}&artist_name={}&album={}&duration={}", ...)`
  - `format!("https://music.youtube.com/watch?v={}", video_id)`
​
---
​
## 3. Additional findings from the first AI pass (for completeness)
​
These were in the original AI analysis and remain valid after re-reading; they are not in the pre-existing list, except where they overlap (noted).
​
### A1 — 🔴 `usize` underflow on song index `(num - 1)` — VALID
`handle_library_browsing` numeric arm: `let song_idx = (page - 1) * PAGE_SIZE + (num - 1);`. Input `"0"` underflows `num - 1`. Debug build panics (`subtract with overflow`); release wraps to `usize::MAX` and is saved only by the later `.get()`. Digits are reachable individually from the input thread.
- **Fix:** guard `if num >= 1 { ... }`.
​
### A2 — 🔴 `"Error in Library: {}"` printed literally — VALID
`set_status_line(Some("Error in Library: {}".to_string()))` — not a `format!`, so `{}` is literal and the bound error `e` is unused. Fix: `format!("Error in Library: {}", e)`.
​
### A3 — 🔴 Autoplay stall when a track ends before the async fill completes — VALID (same subsystem as F3)
On natural end with empty queue and autoplay on, CASE 2.2 stops playback; the refill runs in a detached task that may not have finished → silence. Fix: enter a short buffering/re-poll state instead of stopping when autoplay is enabled.
​
### A4 — 🔴 Volume desync: clamped state vs relative mpv command — VALID
```rust
let new_vol = (current + delta).clamp(0, 150);
VOLUME.store(new_vol, Ordering::Relaxed);
player::vol_change(delta);   // sends mpv: ["add","volume",delta] (relative, unclamped)
```
Internal volume clamps to 0..=150 but mpv receives a relative add, so repeated `+` at the clamp boundary drifts; also mpv's default `volume-max` is 130, so 150 may be silently capped. Fix: send absolute `set_property volume <new_vol>` and pass `--volume-max=150`.
​
### A5 — 🟡 `duration_to_seconds` in `features.rs` ignores `h:mm:ss` — VALID
```rust
if parts.len() == 2 { return (mins*60+secs).to_string(); }
d.to_string()   // 3-part "h:mm:ss" passes through unconverted
```
Tracks ≥1h send `"h:mm:ss"` to lrclib's `duration` param → no match. Inconsistent with `ui_common.rs::duration_to_seconds`, which handles the 3-part case. Fix: handle 3 parts; unify the two copies.
​
### A6 — 🟡 `parse_lrc` ignores multi-timestamp LRC lines — VALID
Only the first `[..]` per line is parsed; extra `[mm:ss.xx]` tags on the same line leak into `text`. Fix: parse all leading timestamp tags, emit one `LrcLine` per timestamp.
​
### A7 — 🟡 Fragile translate/romanize alignment in `romanize_lyrics_google` — VALID
Lines are joined with `" / "`, sent to Google Translate, then the response is split on `'/'` and zipped back by index. Any `/` in a lyric, or delimiter merging by the translator, desynchronizes every subsequent line in the 40-line chunk. Fix: use a delimiter that survives MT (numbered markers) or translate line-by-line.
​
### A8 — 🟢 `get_visual_width` uses a byte-length heuristic — VALID
```rust
s.chars().map(|c| if c.len_utf8() > 1 { 2 } else { 1 }).sum()
```
Assumes every multi-byte char is width 2 (wrong for accented Latin, Cyrillic, Greek, combining marks). `ui1/ui2/ui3` use the proper `unicode-width` crate, so `ui_common`'s `truncate_safe`/`word_wrap_cjk` disagree with the renderers → centering/truncation drift. Fix: use `unicode_width` in `ui_common` (already a dependency).
​
### A9 — 🟢 Shared global scroll state corrupts multi-field scrolling — VALID
`get_scrolling_text` uses single globals `TITLE_SCROLL`/`LAST_SCROLL`. `ui3::draw_minimal_ui` calls it for **both** title and artist in the same frame; they share one counter/timer, so the second field renders with the first's offset. Also the advance modulo (`text.chars().count() + 2`) doesn't match the padded buffer length (`2*count + 1`). Fix: per-field scroll state.
​
### A10 — 🟢 `truncate_safe` ellipsis overflow at tiny widths — VALID
`max_width.saturating_sub(3)` becomes 0 for small widths, so `"..."` is appended even with no room; result can exceed `max_width`. Fix: clamp the ellipsis budget.
​
### A11 — 🟠 Trailing-slash API candidate yields double-slash URLs — VALID
`flac.rs` `API_CANDIDATES` includes `"https://api.monochrome.tf/"`; `format!("{}/track/?id=...", url)` then yields `...//track/...` (and later `//search/...`), which some servers 404. Fix: `url.trim_end_matches('/')` before formatting.
​
### A12 — ⚪ Minor / cleanliness (confirmed)
- `offline.rs::get_random_batch`: `&mut candidates.clone()` clones an already-filtered vec and `&mut`-borrows a temporary. Prefer owning the vec.
- Dead `static IS_LOSSLESS` (unused); `app.rs` entirely unused; unused reads in `get_banner_art`.
- `useless format!`: `format!("QUEUE CLEARED")`, `format!("PLAYING PREVIOUS")`, `format!("No local songs found!")`, etc. → `.to_string()`.
- Many bare `execute!(stdout(), ...)` drop their `Result` (`unused_must_use`).
- Stringly-typed `VIEW_MODE` (`"queue"`/`"recent"`) → prefer an enum.
- `api.rs`: large commented-out `fetch_stream_url` (youtubei `player`) + `debug_req.json`/`debug_res.json` writes → dead code.
- `fetch_account_name().await.unwrap_or("Error".to_string())` discards the error.
- **NOT a bug:** mpv `--demuxer-lavf-o=protocol_whitelist=[file,http,https,...]` uses `[...]` — this is correct mpv list syntax for comma-containing option values; do not “fix” it.
​
---
​
## 4. Suggested fix priority
​
1. **N1** — brace-wrapped URLs (could break lyrics + stream resolution; verify at runtime first).
2. **A1** — `num - 1` underflow (debug crash / release wrap).
3. **F13 / A2** — surface API + library errors instead of swallowing them.
4. **F3 / A3 / A4** — autoplay race + stall + volume desync (most user-visible runtime issues).
5. **F2** — multi-digit playlist selection.
6. **F5 / F7 / F8 / F9** — identity-vs-title/filename mismatches (anti-repeat, lyrics, history, cache).
7. **F4 / A8 / A9 / A10** — rendering synchronization + width/scroll/truncation correctness.
8. **F6, F10, F11, F12, A5–A7, A11** — robustness + docs.
9. **F14 / F15 / A12** — hygiene + tests.
​
---
​
## 5. Needs a real toolchain to confirm (could not run here)
```bash
cargo build 2>&1            # F14 counts; unreachable_statement (F1); unused e (A2), dead statics (A12)
cargo clippy --all-targets  # await_holding_lock (F6), useless_format, must_use on execute!, needless clone
cargo build --release       # panic="abort": confirm debug-vs-release behavior of A1
cargo test                  # F15: expect 0 tests
# Runtime smoke test for N1:
#   add a temporary eprintln! of the formatted URL in fetch_synced_lyrics / fetch_stream_url
#   and confirm whether it begins with '{'.
```
Open runtime questions: N1 actual failure, F3 race timing under slow network/cold yt-dlp, F12 mis-selection frequency, F13 real error bodies from youtubei endpoints.
​
---
​
## 6. Project & runtime reference (for reproduction)
​
- **What it is:** Rust 2024 terminal music player. YouTube Music (youtubei) + optional Tidal/FLAC mirrors; plays via **mpv** over an IPC socket; resolves YouTube stream URLs via **yt-dlp**; downloads/tags via **ffmpeg** + `lofty`; synced lyrics from **lrclib.net** with optional Google-Translate romanize/translate; offline mode plays local `.opus`/`.flac`.
- **Release profile:** `opt-level="z"`, `lto=true`, `codegen-units=1`, `panic="abort"`, `strip=true` (so A1 panic behavior differs debug vs release).
- **External binaries required on PATH:** `mpv`, `yt-dlp`, `ffmpeg`.
- **Key crates:** reqwest 0.12 (rustls, blocking+async, cookies, json), tokio 1, serde/serde_json, crossterm 0.29, colored, unicode-width 0.1, urlencoding, dirs 6, rand 0.9, sha1+hex (SAPISIDHASH), base64 0.22 (FLAC manifest), lofty 0.22, lazy_static.
- **Auth:** Netscape `cookies.txt` at `<audio_dir>/whytui/config/cookies.txt`; parses tab fields, name=parts[5], value=parts[6], requires ≥7 fields; keeps `#HttpOnly_` lines; builds `SAPISIDHASH <ts>_<sha1(ts SP SAPISID SP origin)>`.
- **CLI flags:** `-d/--download` (save while playing), `-o/--offline`, `-n/--nomix` (disable autoplay), `-l/--lossless`, `-pl/--peak-lossless`, `-g/--guess`.
- **Min terminal enforced:** 52×37 (see F1).
- **IPC socket:** unix `/tmp/whytui.sock`, windows `\\.\pipe\whytui.sock` (see F10).
- **Source files:** `main.rs` (~1382 ln), `api.rs` (~832 ln), `ui_common.rs`, `ui1.rs`/`ui2.rs`/`ui3.rs`, `features.rs`, `flac.rs`, `player.rs`, `offline.rs`, `app.rs` (unused).
​
---
​
## 7. Validation summary table
​
| ID | Finding | Verdict |
|----|---------|---------|
| F1 | Terminal ≥ 52×37 hard block | VALID (gate intentional; threshold + unreachable line are bugs) |
| F2 | Playlist select breaks 10+ | VALID |
| F3 | Autoplay stale-leak race | VALID |
| F4 | Unsynchronized rendering | VALID |
| F5 | Offline anti-repeat broken | VALID |
| F6 | Read lock across await (shuffle) | VALID |
| F7 | Lyric state keyed by title | VALID |
| F8 | History dedupe by title | VALID (consecutive-only nuance) |
| F9 | Cache filename collision | VALID |
| F10 | Hard-coded mpv socket | VALID |
| F11 | README stale | VALID |
| F12 | Lossless resolver duration-driven | VALID |
| F13 | post_auth opaque on non-2xx | VALID |
| F14 | 33/108 warning counts | PARTLY VERIFIABLE (categories real; counts not runnable here) |
| F15 | No automated tests | VALID |
| N1 | Brace-wrapped URLs (``/``) | VALID, NEW (execution-unverified) |
| A1 | `num-1` underflow | VALID, extra |
| A2 | Literal `{}` in error string | VALID, extra |
| A3 | Autoplay stall on track end | VALID, extra |
| A4 | Volume desync | VALID, extra |
| A5–A11 | Lyrics/UI/flac robustness | VALID, extra |
| — | mpv `protocol_whitelist=[...]` | NOT A BUG (correct mpv syntax) |
​