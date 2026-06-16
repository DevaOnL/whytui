Findings

High: the app hard-blocks on startup unless the terminal is at least 52x37, which makes it unusable in a normal 80x24 terminal. I reproduced this immediately before forcing a larger PTY. See src/main.rs (line 197).

High: playlist selection breaks for 10+ playlists. The UI prints Select (1-N), but the raw input thread sends digits one at a time, and both playlist flows only consume a single message, so 10 is read as 1. See src/ui_common.rs (line 192), src/main.rs (line 610), src/main.rs (line 956), src/main.rs (line 1284).

High: autoplay queueing is race-prone. Old queue_auto_add_online tasks keep writing into the shared queue and related-song cache after the user changes tracks, so stale recommendations can leak into the new session. See src/main.rs (line 344), src/main.rs (line 924), src/main.rs (line 1141).

High: rendering is unsynchronized. The background monitor redraws every 300ms while search and library flows also print directly to stdout, which I reproduced as interleaved prompts, lyrics, and library pages. See src/ui_common.rs (line 241), src/main.rs (line 977), src/main.rs (line 1242).

Medium: offline anti-repeat is broken. Exclusions are built from Track.title, but offline selection filters by filename stem; downloaded files are named title - artist, so recently played songs can be re-queued. I reproduced this with a two-song offline library. See src/offline.rs (line 22), src/offline.rs (line 92), src/player.rs (line 252).

Medium: library shuffle holds a read lock across an .await, which Clippy correctly flags as risky for contention and future deadlocks. See src/main.rs (line 1026).

Medium: lyric state is keyed only by song title. Same-title tracks can reuse or overwrite each other’s lyrics, and the monitor will not restart if only the underlying song changes. See src/ui_common.rs (line 20), src/ui_common.rs (line 276), src/ui1.rs (line 91), src/ui2.rs (line 117), src/ui3.rs (line 47).

Medium: recent-history and previous-track behavior also dedupe by title only, so different songs sharing a title collapse together in history. See src/main.rs (line 1094).

Medium: local cache filenames collide on title + first artist, so remasters, live versions, or album variants from the same lead artist can overwrite or masquerade as one another. See src/player.rs (line 252), src/main.rs (line 859).

Medium: multiple instances will fight over the same mpv IPC socket because it is hard-coded to /tmp/whytui.sock. See src/player.rs (line 186).

Medium: the README is materially stale. It says only mpv is required and describes --download as offline playback, but actual online playback shells out to yt-dlp, saving tracks requires ffmpeg, and true offline playback is --offline. See README.md (line 22), README.md (line 75), src/main.rs (line 127), src/api.rs (line 249), src/player.rs (line 80).

Medium: the lossless resolver is too duration-driven. It can easily pick the wrong FLAC result when the third-party search ranking is noisy but durations are close. See src/flac.rs (line 67).

Low: authenticated API failures are opaque because post_auth ignores non-2xx statuses and immediately attempts JSON parsing. See src/api.rs (line 138).

Low: code hygiene is noisy enough to hide real issues. cargo check and cargo test pass, but there are 33 compiler warnings and 108 Clippy warnings, including ignored terminal I/O results, unreachable code, dead fields, and an unused refactor stub in src/app.rs (line 1). See src/main.rs (line 205), src/main.rs (line 152), src/api.rs (line 130).

Low: there is no automated safety net. cargo test ran 0 tests, so queueing, lyrics, library pagination, offline refill, and auth regressions are all currently manual-only.

Coverage / Assumptions

I ran cargo check, cargo test, cargo build, and cargo clippy --all-targets -- -W clippy::all.
I ran the app in an isolated temporary HOME, copied in your valid cookies.txt, and verified offline playback, online login, search, playback, autoplay queue fill, next-track behavior, lyrics, and library loading.
I did not execute account-mutating actions like Like or Add to playlist, so I wouldn’t change your actual YouTube Music account.
The 10+ playlist bug was not dynamically reproduced only because the tested account exposed 4 playlists, but the code path is clear.
Project Shape

src/main.rs (line 1) is the real application brain: args, globals, input thread, event loop, queue/history, search, library, and playback orchestration.
src/api.rs (line 1) is the YouTube Music/auth layer, with yt-dlp currently acting as the stream URL fallback.
src/player.rs (line 1) owns mpv process control, IPC, ffmpeg downloading, and tag/artwork writing.
src/ui_common.rs (line 1) plus src/ui1.rs (line 1), src/ui2.rs (line 1), and src/ui3.rs (line 1) form a shared-state renderer; src/offline.rs (line 1) handles local library mode; src/flac.rs (line 1) is the third-party lossless path.