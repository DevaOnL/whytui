use crate::Track;
use crate::config;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::Tag;
use serde_json::json;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// Ask the kernel to kill this child if whytui dies, for any reason at all — SIGKILL, SIGTERM, a
/// panic under the release profile's `panic = "abort"`, or the process simply being closed.
///
/// Without it, killing whytui left mpv running and the music playing with no window and no way to
/// stop it short of finding the pid; an interrupted download left an orphan ffmpeg writing into the
/// temp dir. `Child::kill` only covers the paths where we actually reach our own cleanup code.
#[cfg(target_os = "linux")]
pub(crate) fn die_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // whytui's pid, captured before the fork so the child can tell if the parent has already died by
    // the time its pre_exec runs.
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: pre_exec runs in the forked child before exec. prctl/getppid/setpgid/_exit are all
    // async-signal-safe, and this closure allocates nothing and touches no shared state.
    unsafe {
        cmd.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            // Close the classic PDEATHSIG race: if whytui died between fork() and the prctl above, the
            // death signal was armed too late (or never) and the child would be orphaned forever.
            // getppid() no longer returning our pid means exactly that reparenting happened, so exit
            // now instead of leaking a player.
            if libc::getppid() != parent {
                libc::_exit(0);
            }
            // Put the child in its own process group so stop_process can killpg the whole tree —
            // sweeping any helper processes mpv spawns (grandchildren), which PDEATHSIG, tracking only
            // the direct child, does not cover. Safe here because these children have null stdin and
            // never do terminal job control.
            libc::setpgid(0, 0);
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn die_with_parent(_cmd: &mut Command) {
    // No portable equivalent; stop_process()/quit_app() still clean up on the normal paths.
}

static IPC_COUNTER: AtomicU64 = AtomicU64::new(0);
static DOWNLOAD_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Makes each play attempt's raw `--stream-record` file distinct. Without it the path was derived
/// only from the track, so replaying the same streamed track (repeat, or picking it again) reused the
/// identical filename — and the previous play's finalize thread, still probing/remuxing that file,
/// then raced mpv writing the new recording into it: the remux read a half-written file and failed,
/// or finish_recording unlinked the file mpv was actively recording, losing the copy entirely.
static RECORD_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Correlates an IPC command with its reply. Starts at 1 so it never collides with mpv's default 0.
static IPC_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
#[derive(Clone)]
struct ActiveIpc {
    generation: u64,
    path: String,
}

static ACTIVE_IPC_PATH: RwLock<Option<ActiveIpc>> = RwLock::new(None);
static PLAYBACK_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Where mpv is writing a copy of the stream it is playing, when caching is enabled.
static ACTIVE_RECORDING: RwLock<Option<PathBuf>> = RwLock::new(None);
static RECORDING_WORKERS: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());
static RECORDING_SEEKED: AtomicBool = AtomicBool::new(false);
/// Furthest playback position seen for the current track, and its duration, both in milliseconds.
/// Sampled from mpv rather than measured as wall-clock so pausing does not count as listening.
static FURTHEST_POSITION_MS: AtomicU64 = AtomicU64::new(0);
static REPORTED_DURATION_MS: AtomicU64 = AtomicU64::new(0);

/// How much of a track has to be heard before its copy is kept. "Almost fully listened" — the last
/// few seconds are usually an outro nobody waits through, and mpv is often stopped a beat early.
const LISTENED_FRACTION_TO_CACHE: f64 = 0.90;
/// How complete the recovered audio must be to be worth keeping. A recording that stops well short of
/// the track would put a truncated song in the library, which is worse than caching nothing.
const RECORDING_COMPLETENESS_REQUIRED: f64 = 0.97;

/// Containers whytui writes into the cache and recognises when reading it back.
pub const CACHED_AUDIO_EXTENSIONS: &[&str] = &["flac", "opus", "m4a", "ogg", "mp3", "mka"];
/// The subset that is actually lossless, for when the user asked for lossless specifically.
pub const LOSSLESS_AUDIO_EXTENSIONS: &[&str] = &["flac"];

/// Whether `source` is something being streamed — and therefore worth keeping a copy of — as opposed
/// to a file already in the cache.
fn is_streamed_source(source: &str) -> bool {
    source.starts_with("http") || source.ends_with(".mpd")
}

/// The generation of the mpv instance that is currently the active one.
///
/// Bumped every time a new mpv is spawned (see `play_file`), so it uniquely identifies "which track's
/// player is live right now". Callers stamp a deferred track-relative command with this value and
/// re-check it before replaying the command, so a pause/seek meant for one track cannot land on the
/// next one after an asynchronous transition.
pub fn current_playback_generation() -> u64 {
    PLAYBACK_GENERATION.load(Ordering::Relaxed)
}

/// Record how far mpv has actually got through the current track. Called from the monitor thread.
pub fn note_progress(generation: u64, position_secs: f64, duration_secs: f64) {
    let active = ACTIVE_IPC_PATH.read().unwrap_or_else(|e| e.into_inner());
    if active
        .as_ref()
        .map(|current| current.generation != generation)
        .unwrap_or(true)
    {
        return;
    }
    // Keep the read guard through the counter writes. A new playback needs the write lock before it
    // resets those counters, so an old sample can only land before that reset, never after it.
    if position_secs.is_finite() && position_secs > 0.0 {
        let ms = (position_secs * 1000.0) as u64;
        FURTHEST_POSITION_MS.fetch_max(ms, Ordering::Relaxed);
    }
    if duration_secs.is_finite() && duration_secs > 0.0 {
        REPORTED_DURATION_MS.store((duration_secs * 1000.0) as u64, Ordering::Relaxed);
    }
}

/// Whether mpv reported any real playback position for the current track.
///
/// A more reliable "did this actually play?" signal than wall-clock time: a refused stream can take
/// several seconds to fail (mpv retries through its ytdl hook first), so an elapsed-time threshold
/// misclassified those as normal playback and skipped the track with no explanation.
pub fn heard_any_audio() -> bool {
    FURTHEST_POSITION_MS.load(Ordering::Relaxed) > 500
}

fn reset_progress() {
    FURTHEST_POSITION_MS.store(0, Ordering::Relaxed);
    REPORTED_DURATION_MS.store(0, Ordering::Relaxed);
}

/// How much of `track` was heard, as a fraction of its length.
///
/// Prefers the duration mpv reported, falling back to the metadata duration, because a stream's real
/// length and the value the API returned do not always agree.
fn listened_fraction(track: &Track) -> Option<f64> {
    let position = FURTHEST_POSITION_MS.load(Ordering::Relaxed) as f64 / 1000.0;

    let mut total = REPORTED_DURATION_MS.load(Ordering::Relaxed) as f64 / 1000.0;
    if total <= 0.0 {
        total = crate::ui_common::duration_to_seconds(&track.duration);
    }
    if total <= 0.0 {
        return None;
    }

    Some(position / total)
}

fn take_active_recording() -> Option<PathBuf> {
    ACTIVE_RECORDING
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// A fresh `--input-ipc-server` address for one mpv instance.
///
/// Platform-specific on purpose. mpv on Windows creates a *named pipe* and prefixes `\\.\pipe\` when
/// the argument does not already have it, so handing it a temp-directory path produced
/// `\\.\pipe\C:\Users\...\whytui-123-0.sock` — illegal, because a pipe name cannot contain `\` or
/// `:` — and mpv silently started no IPC server at all, leaving pause/seek/volume and the whole
/// progress bar dead.
#[cfg(unix)]
fn new_ipc_path() -> String {
    let n = IPC_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!("whytui-{}-{}.sock", std::process::id(), n))
        .to_string_lossy()
        .to_string()
}

#[cfg(windows)]
fn new_ipc_path() -> String {
    let n = IPC_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(r"\\.\pipe\whytui-{}-{}", std::process::id(), n)
}

/// Where mpv's stderr for the current track is kept.
fn mpv_log_path(music_dir: &Path) -> PathBuf {
    session_temp_dir(music_dir).join("mpv-last.log")
}

/// A short, human-readable reason the last track failed to play, from mpv's own output.
///
/// Turns a silent skip into something the user can act on. mpv's messages are long and full of the
/// whole signed URL, so only the recognisable causes are mapped to a phrase that fits the status line.
pub fn last_playback_error(music_dir: &Path) -> Option<String> {
    let log = fs::read_to_string(mpv_log_path(music_dir)).ok()?;
    if log.trim().is_empty() {
        return None;
    }
    let lower = log.to_lowercase();

    let reason = if lower.contains("403") || lower.contains("forbidden") {
        "Stream refused by YouTube (403)"
    } else if lower.contains("404") || lower.contains("not found") {
        "Stream gone (404)"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "Stream timed out"
    } else if lower.contains("name or service not known") || lower.contains("failed to resolve") {
        "No network"
    } else if lower.contains("unrecognized file format") || lower.contains("failed to recognize") {
        "Unplayable stream format"
    } else if lower.contains("ytdl_hook") || lower.contains("youtube-dl failed") {
        "yt-dlp could not open stream"
    } else {
        return None;
    };
    Some(reason.to_string())
}

/// Append the playback source to an mpv command behind a `--` end-of-options marker.
///
/// Factored out so the marker ordering can be asserted in a test: mpv parses any bare argument
/// beginning with `-` as an option, so a source such as `--script=/tmp/evil.lua` from a hostile
/// mirror would be executed as configuration unless it follows `--`.
fn append_media_source(cmd: &mut Command, source: &str) {
    cmd.arg("--").arg(source);
}

pub fn play_file(
    source: &str,
    track: &Track,
    music_dir: &Path,
) -> Result<Child, Box<dyn std::error::Error>> {
    let ipc = new_ipc_path();
    let current_vol = crate::VOLUME.load(Ordering::Relaxed);
    #[cfg(unix)]
    let _ = std::fs::remove_file(&ipc);

    if source_is_lossless(source) {
        crate::PLAYING_LOSSLESS.store(true, Ordering::SeqCst);
    } else {
        crate::PLAYING_LOSSLESS.store(false, Ordering::SeqCst);
    }
    let user_agent = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36,gzip(gfe)";
    let mut cmd = Command::new("mpv");
    cmd.arg("--no-video")
        // NOT --really-quiet: that silences mpv completely and cannot be re-enabled by a later
        // --msg-level, which is why a refused stream left no trace anywhere and was indistinguishable
        // from a track that simply ended. Errors only, and they are captured to a file below rather
        // than the terminal, so they cannot tear the TUI.
        .arg("--msg-level=all=error")
        .arg("--force-window=no")
        .arg(format!("--input-ipc-server={}", ipc))
        .arg(format!("--volume={}", current_vol))
        // whytui lets the user go to 150, but mpv's own default ceiling is volume-max=130, so
        // anything above that was silently clamped and the displayed number stopped matching what
        // was actually playing
        .arg("--volume-max=150")
        // `file` stays: mpv/libavformat need it to open cached/offline files AND the local DASH `.mpd`
        // we write for lossless. `data` is dropped — no legitimate source uses a `data:` URI, and it
        // was a way for a hostile manifest to inline attacker bytes. A manifest that references any
        // non-http(s) scheme (including `file:`) is rejected in flac.rs before it is ever written, so
        // keeping `file` here cannot be abused through an untrusted manifest.
        .arg("--demuxer-lavf-o=protocol_whitelist=[file,http,https,tcp,tls,crypto]")
        .arg(format!("--user-agent={}", user_agent))
        .arg("--http-header-fields=Referer: https://music.youtube.com/,Origin: https://music.youtube.com")
        // mpv is driven entirely over the IPC socket and must never read the terminal: with an
        // inherited tty stdin it defaults --input-terminal=yes and races the TUI for keystrokes, so
        // presses meant for whytui went missing. Null stdin removes it from the contest for fd 0.
        .stdin(Stdio::null());

    // Keep mpv's diagnostics. They used to go to /dev/null, so when a stream was refused (an expired
    // URL, a region block, googlevideo answering 403) the track was simply skipped with nothing
    // anywhere to say why — for the user or for anyone debugging it. Overwritten per track, and
    // `last_playback_error` reads it back to put a real reason on the status line.
    //
    // mpv writes its messages to STDOUT, so that is the stream that has to be captured; redirecting
    // only stderr left the log empty.
    let log = File::create(mpv_log_path(music_dir)).ok();
    match log.as_ref().and_then(|f| f.try_clone().ok()) {
        Some(err_handle) => {
            cmd.stdout(Stdio::from(log.expect("cloned from Some")))
                .stderr(Stdio::from(err_handle));
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    // Caching works by having mpv keep a copy of the stream it is already fetching, rather than
    // downloading the track a second time afterwards. That matters because googlevideo refuses a
    // second, open-ended GET for the same URL with HTTP 403 — it only serves the small ranged reads a
    // player makes — so the old "re-download it with ffmpeg when the song ends" approach could never
    // succeed. mpv is already reading the bytes; this just writes them out on the way past.
    let recording = if config().download_mode && is_streamed_source(source) {
        // The counter is what keeps a replay of the same track from colliding with the previous
        // play's still-running finalize thread on an identically-named file.
        let n = RECORD_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = session_temp_dir(music_dir).join(format!(
            "{}.{}.record.mkv",
            cache_file_stem(track),
            n
        ));
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::remove_file(&path);
        // Matroska because it accepts every codec YouTube or the lossless mirrors might hand us;
        // finish_recording remuxes it into a proper container once the codec is known.
        cmd.arg(format!("--stream-record={}", path.display()));
        Some(path)
    } else {
        None
    };

    // The source has to stay last, after every option — and behind a `--` end-of-options marker so a
    // value beginning with `-` (from a compromised mirror or manifest) can never be parsed by mpv as
    // an option. Scheme validation happens at the network boundary in flac.rs; this is the
    // process-boundary backstop that holds even if that validation ever regresses.
    append_media_source(&mut cmd, source);

    die_with_parent(&mut cmd);

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn mpv. Is mpv installed and in PATH? {}", e))?;

    *ACTIVE_RECORDING.write().unwrap_or_else(|e| e.into_inner()) = recording;

    // When a track ends on its own, stop_process() never runs, so the previous socket would be
    // left behind in the temp dir for every song played.
    {
        let mut active = ACTIVE_IPC_PATH.write().unwrap_or_else(|e| e.into_inner());
        let generation = PLAYBACK_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        reset_progress();
        RECORDING_SEEKED.store(false, Ordering::Relaxed);
        if let Some(previous) = active.replace(ActiveIpc {
            generation,
            path: ipc,
        }) {
            #[cfg(unix)]
            let _ = std::fs::remove_file(previous.path);
        }
    }

    crate::IS_PLAYING.store(true, Ordering::SeqCst);
    Ok(child)
}

/// Ask ffprobe what audio codec a file holds.
fn probe_audio_codec(path: &Path) -> Option<String> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "a:0",
        "-show_entries",
        "stream=codec_name",
        "-of",
        "default=nw=1:nk=1",
    ])
    .arg(path)
    .stderr(Stdio::null());
    // Sweep this probe if whytui is hard-killed mid-finalise, like every other child we spawn.
    die_with_parent(&mut cmd);
    let out = cmd.output().ok()?;

    if !out.status.success() {
        return None;
    }
    let codec = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if codec.is_empty() { None } else { Some(codec) }
}

/// How long the audio in `path` actually is, per ffprobe.
fn probe_duration_secs(path: &Path) -> Option<f64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=nw=1:nk=1",
    ])
    .arg(path)
    .stderr(Stdio::null());
    // Sweep this probe if whytui is hard-killed mid-finalise, like every other child we spawn.
    die_with_parent(&mut cmd);
    let out = cmd.output().ok()?;

    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// The container to store `codec` in, so the file is both valid and recognised on the way back in.
fn container_for_codec(codec: &str) -> &'static str {
    match codec {
        "opus" => "opus",
        "flac" => "flac",
        "vorbis" => "ogg",
        "aac" | "alac" => "m4a",
        "mp3" => "mp3",
        // Matroska takes anything; better a playable .mka than a mislabelled .opus
        _ => "mka",
    }
}

/// Finalise the copy mpv recorded while playing: keep it only if the track was essentially listened
/// through, then remux it into a real container, tag it, and move it into the music dir.
///
/// Returns the cached path, or None when there was nothing to keep.
pub fn finish_recording(
    recording: &Path,
    heard: f64,
    track: &Track,
    music_dir: &Path,
) -> Result<CacheOutcome, Box<dyn std::error::Error>> {
    // Always drop the raw recording on the way out of this function, whatever happens.
    let discard = |outcome: CacheOutcome| -> Result<CacheOutcome, Box<dyn std::error::Error>> {
        let _ = fs::remove_file(recording);
        Ok(outcome)
    };

    if !recording.exists() {
        // mpv never wrote anything — most likely the stream failed to open at all
        return Ok(CacheOutcome::NothingRecorded);
    }

    if heard < LISTENED_FRACTION_TO_CACHE {
        return discard(CacheOutcome::NotListenedEnough { heard });
    }

    let Some(codec) = probe_audio_codec(recording) else {
        let _ = fs::remove_file(recording);
        return Err("unknown recorded codec".into());
    };
    let ext = container_for_codec(&codec);

    let stem = cache_file_stem(track);
    let final_path = music_dir.join(format!("{}.{}", stem, ext));

    let id = match &track.video_id {
        Some(v) => v.clone(),
        None => short_hash(&track.url),
    };
    if find_cached_file(music_dir, &track.title, &track.artists, Some(&id), &[ext]).is_some() {
        return discard(CacheOutcome::AlreadyCached);
    }

    // Remux, not re-download: purely local, so it cannot be refused by the server.
    let temp_path = session_temp_dir(music_dir).join(format!(
        "{}.part{}.{}",
        stem,
        DOWNLOAD_COUNTER.fetch_add(1, Ordering::Relaxed),
        ext
    ));
    if let Some(parent) = temp_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let mut ffmpeg_cmd = Command::new("ffmpeg");
    ffmpeg_cmd
        .arg("-y")
        .arg("-i")
        .arg(recording)
        .arg("-vn")
        .arg("-c")
        .arg("copy")
        .arg(&temp_path)
        // Null stdin so this background remux cannot read (and steal) the TUI's keystrokes; ffmpeg
        // otherwise treats an inherited tty as an interactive console.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    die_with_parent(&mut ffmpeg_cmd);

    let status = ffmpeg_cmd.spawn()?.wait()?;
    if !status.success() {
        let _ = fs::remove_file(&temp_path);
        let _ = fs::remove_file(recording);
        return Err("remux failed".into());
    }

    // A recording that stops well short of the track would cache a truncated song, which is worse
    // than caching nothing. mpv reads ahead of playback, so a track heard to the end is normally
    // complete here; this catches the cases where it is not.
    let expected = crate::ui_common::duration_to_seconds(&track.duration);
    let got = probe_duration_secs(&temp_path).unwrap_or(0.0);
    if expected > 0.0 && got / expected < RECORDING_COMPLETENESS_REQUIRED {
        let _ = fs::remove_file(&temp_path);
        return discard(CacheOutcome::Incomplete { got, expected });
    }

    if let Err(e) = tag_and_publish(&temp_path, &final_path, track) {
        let _ = fs::remove_file(&temp_path);
        let _ = fs::remove_file(recording);
        return Err(e);
    }

    let _ = fs::remove_file(recording);
    Ok(CacheOutcome::Saved(final_path))
}

/// What happened to a track's recorded copy.
#[derive(Debug)]
pub enum CacheOutcome {
    Saved(PathBuf),
    NotListenedEnough { heard: f64 },
    Incomplete { got: f64, expected: f64 },
    AlreadyCached,
    NothingRecorded,
}

/// Claim the recording for the track that just stopped, along with how much of it was heard.
///
/// Both have to be read on the caller's thread, before the next track starts: `play_file` installs a
/// new recording path and resets the progress counters, so doing this inside a worker thread raced
/// with it — the worker would finalise the *next* track's half-written file against this track's
/// metadata, then leave the new track with no recording to finalise. Nothing was ever cached.
fn claim_recording(track: &Track) -> Option<(PathBuf, f64, f64)> {
    let recording = take_active_recording()?;
    let heard = if RECORDING_SEEKED.load(Ordering::Relaxed) {
        0.0
    } else {
        listened_fraction(track).unwrap_or(0.0)
    };
    let reported_duration = REPORTED_DURATION_MS.load(Ordering::Relaxed) as f64 / 1000.0;
    Some((recording, heard, reported_duration))
}

/// Finalise the recording off the main thread, reporting the outcome on the status line.
pub fn finish_recording_async(track: &Track, music_dir: &Path) {
    let Some((recording, heard, reported_duration)) = claim_recording(track) else {
        return;
    };

    let mut track = track.clone();
    if reported_duration > 0.0 {
        track.duration = seconds_to_duration(reported_duration);
    }
    let music_dir = music_dir.to_path_buf();
    let worker = std::thread::spawn(move || {
        let message = match finish_recording(&recording, heard, &track, &music_dir) {
            Ok(CacheOutcome::Saved(path)) => Some(format!(
                "Saved {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
            Ok(CacheOutcome::NotListenedEnough { heard }) => Some(format!(
                "Not saved: only {}% played",
                (heard * 100.0).round() as i64
            )),
            Ok(CacheOutcome::Incomplete { got, expected }) => Some(format!(
                "Not saved: got {}s of {}s",
                got.round() as i64,
                expected.round() as i64
            )),
            Ok(CacheOutcome::AlreadyCached) | Ok(CacheOutcome::NothingRecorded) => None,
            // Reported on the status line, not with eprintln!, which would tear the raw-mode frame.
            Err(e) => Some(format!("Couldn't save track: {}", e)),
        };
        if let Some(message) = message {
            crate::ui_common::set_status_line(Some(message));
        }
    });

    let mut workers = RECORDING_WORKERS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut index = workers.len();
    while index > 0 {
        index -= 1;
        if workers[index].is_finished() {
            let finished = workers.swap_remove(index);
            let _ = finished.join();
        }
    }
    workers.push(worker);
}

/// Finalise a claimed recording before process exit. The asynchronous path cannot be used here:
/// `process::exit` terminates worker threads immediately and used to discard an otherwise cacheable
/// track whenever the user quit near its end.
pub fn finish_recording_on_exit(track: &Track, music_dir: &Path) {
    if let Some((recording, heard, reported_duration)) = claim_recording(track) {
        let mut track = track.clone();
        if reported_duration > 0.0 {
            track.duration = seconds_to_duration(reported_duration);
        }
        let _ = finish_recording(&recording, heard, &track, music_dir);
    }
}

pub fn wait_for_recording_workers() {
    let workers = {
        let mut workers = RECORDING_WORKERS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *workers)
    };
    for worker in workers {
        let _ = worker.join();
    }
}

/// Write `track`'s metadata into the finished temp file and move it into the cache.
fn tag_and_publish(
    temp_path: &Path,
    final_path: &Path,
    track: &Track,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tagged_file = lofty::read_from_path(temp_path)?;
    let tag = if let Some(t) = tagged_file.primary_tag_mut() {
        t
    } else {
        let tag_type = tagged_file.primary_tag_type();
        tagged_file.insert_tag(Tag::new(tag_type));
        tagged_file
            .primary_tag_mut()
            .ok_or("Could not create tags")?
    };

    tag.set_title(track.title.clone());

    tag.set_artist(track.artists.join(", "));

    if !track.album.is_empty() {
        tag.set_album(track.album.clone());
    }

    if let Some(url) = &track.thumbnail_url {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        if let Ok(resp) = client.get(url).send() {
            // The status was never checked and the mime type was hardcoded to JPEG, so a 404's
            // HTML body would be embedded verbatim as the track's cover art.
            if resp.status().is_success()
                && let Ok(data) = resp.bytes()
            {
                let mime = if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
                    Some(MimeType::Jpeg)
                } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
                    Some(MimeType::Png)
                } else {
                    None
                };

                if let Some(mime) = mime {
                    tag.push_picture(Picture::new_unchecked(
                        PictureType::CoverFront,
                        Some(mime),
                        None,
                        data.to_vec(),
                    ));
                }
            }
        }
    }

    tag.save_to_path(temp_path, lofty::config::WriteOptions::default())?;

    std::fs::rename(temp_path, final_path)?;

    Ok(())
}

pub fn stop_process(proc: &mut Option<Child>, _song_name: &str, _music_dir: &PathBuf) {
    crate::IS_PLAYING.store(false, Ordering::SeqCst);

    if let Some(mut child) = proc.take() {
        // The child's process group id equals its pid (setpgid in die_with_parent). Captured before
        // any wait, so it cannot be a recycled pid.
        #[cfg(target_os = "linux")]
        let pgid = child.id() as libc::pid_t;

        #[cfg(target_os = "windows")]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &child.id().to_string()])
                .creation_flags(0x08000000)
                .output();
        }

        // SIGTERM the whole group first as a courtesy (mpv can flush and reap its own helpers), then
        // SIGKILL the leader and finally the group — while the leader is still an unreaped zombie, so
        // its pid/group id is guaranteed valid and unrecycled. This sweeps mpv's grandchildren, which
        // a bare child.kill() left orphaned. SAFETY: killpg only sends a signal; an empty group is a
        // harmless ESRCH.
        #[cfg(target_os = "linux")]
        unsafe {
            libc::killpg(pgid, libc::SIGTERM);
        }
        let _ = child.kill();
        #[cfg(target_os = "linux")]
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
        let _ = child.wait();
    }

    if let Some(active) = ACTIVE_IPC_PATH
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        #[cfg(unix)]
        let _ = std::fs::remove_file(active.path);
    }
}

pub fn prepare_music_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut d = dirs::audio_dir().ok_or("No audio dir")?;
    d.push("whytui");
    fs::create_dir_all(&d)?;
    fs::create_dir_all(session_temp_dir(&d))?;
    let config_dir = d.join("config");
    fs::create_dir_all(&config_dir)?;
    prepare_cookies_file(&config_dir.join("cookies.txt"))?;
    Ok(d)
}

/// Ensure the cookies file exists as a private, regular file.
///
/// It holds YouTube authentication cookies (SAPISID, LOGIN_INFO, ...), so on Unix it must be mode
/// 0600 and must never be a symlink: a group/world-readable file leaks the session to other local
/// users, and a symlink planted at the path could redirect the create/truncate to another file or make
/// the app trust a foreign cookie source. The old code created it with a bare `File::create` under the
/// ambient umask (typically 0644) and never repaired an existing file's permissions.
pub fn prepare_cookies_file(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                if !meta.file_type().is_file() {
                    return Err(format!(
                        "{} is not a regular file; refusing to use it for cookies",
                        path.display()
                    )
                    .into());
                }
                // Tighten a loosely-permissioned existing file. `mode()` on OpenOptions only affects
                // newly created files, and the umask can only remove bits, so an already-present file
                // has to be repaired explicitly.
                if meta.permissions().mode() & 0o077 != 0 {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // create_new => O_CREAT|O_EXCL, and O_NOFOLLOW refuses a symlink at the final
                // component — so a symlink squatting on the path (even one planted between the stat
                // above and here) fails the open rather than being followed.
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)?;
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // No portable per-user permission model here; at least refuse a non-regular file and create
        // the file when it is absent.
        match fs::symlink_metadata(path) {
            Ok(meta) if !meta.file_type().is_file() => {
                Err(format!("{} is not a regular file", path.display()).into())
            }
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                File::create(path)?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// This process's private scratch directory, `<music_dir>/temp/<pid>`.
///
/// Every instance used to share `<music_dir>/temp` and wipe all of it at startup, so launching a
/// second whytui deleted the first one's in-flight downloads and the DASH manifests its mpv was
/// still reading from.
pub fn session_temp_dir(music_dir: &Path) -> PathBuf {
    music_dir.join("temp").join(std::process::id().to_string())
}

/// Best-effort "is this pid still running?", used to decide whether a leftover session directory
/// belongs to a live instance or to one that crashed.
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> bool {
    // signal 0 performs the permission/existence check without delivering anything. EPERM means the
    // process exists but belongs to someone else, which still counts as alive.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> bool {
    // no cheap portable check; assume alive and leave the directory behind rather than risk
    // deleting a running instance's scratch files
    true
}

/// Clear this instance's scratch directory, and reap the ones left behind by instances that are no
/// longer running.
///
/// It deliberately does NOT touch a live instance's directory: deleting everything under
/// `<music_dir>/temp` meant a second whytui destroyed the first one's partial downloads and the DASH
/// manifests its mpv still had open.
pub fn clear_temp(music_dir: &Path) {
    let temp_root = music_dir.join("temp");
    let mine = session_temp_dir(music_dir);

    let Ok(entries) = std::fs::read_dir(&temp_root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Classify by the entry's own type, which — unlike Path::is_dir()/is_file() — does NOT follow
        // symlinks. A symlink planted under temp/ (e.g. one named like a dead pid but pointing outside
        // the tree) is therefore removed as a link and never recursed through. std's remove_dir_all is
        // already symlink-safe on this toolchain; this makes the guarantee explicit and holds even if
        // the traversal is ever hand-rolled.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_symlink() {
            let _ = std::fs::remove_file(&path);
            continue;
        }

        if path == mine {
            // our own directory: safe to empty (we have not written anything yet). Only unlink regular
            // files, and never follow a symlink that somehow appeared inside it.
            if let Ok(inner) = std::fs::read_dir(&path) {
                for f in inner.flatten() {
                    if f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        let _ = std::fs::remove_file(f.path());
                    }
                }
            }
            continue;
        }

        if file_type.is_dir() {
            // a pid-named directory from another run; only reap it if that pid is gone
            let owner = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.parse::<i32>().ok());

            match owner {
                Some(pid) if !pid_is_alive(pid) => {
                    let _ = std::fs::remove_dir_all(&path);
                }
                Some(_) => {} // still running, leave it alone
                None => {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
            continue;
        }

        // Loose file directly under temp/ — from a build before per-session directories existed.
        if file_type.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Pick out the reply to `request_id`, skipping mpv's asynchronous event broadcasts.
///
/// mpv delivers events (`seek`, `playback-restart`, `file-loaded`, `pause`, ...) to every connected
/// client on the same socket. Reading exactly one line therefore consumed whichever of those happened
/// to arrive first as if it were the reply, so `get_time_info` found no `data` field and returned
/// None — freezing the elapsed time and the progress bar until a later tick got lucky.
fn read_ipc_reply<R: BufRead>(reader: &mut R, request_id: u64) -> Option<String> {
    // A few events may already be queued; the socket read timeout bounds the wait regardless.
    for _ in 0..16 {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return None, // connection closed
            Ok(_) => {}
            Err(_) => return None, // read timeout or I/O error
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("event").is_some() {
            continue; // an event, not our reply
        }
        match value.get("request_id").and_then(|v| v.as_u64()) {
            Some(id) if id == request_id => return Some(line),
            Some(_) => continue, // reply to a different request
            // no request_id echoed back: assume it is ours rather than hanging
            None => return Some(line),
        }
    }
    None
}

pub fn send_ipc(cmd: serde_json::Value) -> Option<String> {
    let path = ACTIVE_IPC_PATH
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|active| active.path.clone())?;

    send_ipc_to(&path, cmd)
}

fn send_ipc_to(path: &str, cmd: serde_json::Value) -> Option<String> {
    // Tag the request so its reply can be told apart from event broadcasts.
    let request_id = IPC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let mut cmd = cmd;
    if let Some(obj) = cmd.as_object_mut() {
        obj.insert("request_id".to_string(), json!(request_id));
    }
    let msg = format!("{}\n", cmd);

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        if let Ok(mut stream) = UnixStream::connect(path) {
            let _ = stream.write_all(msg.as_bytes());
            let _ = stream.flush();
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .ok();
            let mut reader = BufReader::new(&stream);
            return read_ipc_reply(&mut reader, request_id);
        }
    }

    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        if let Ok(mut file) = OpenOptions::new().read(true).write(true).open(&path) {
            let _ = file.write_all(msg.as_bytes());
            let _ = file.flush();
            let mut reader = BufReader::new(&file);
            return read_ipc_reply(&mut reader, request_id);
        }
    }
    None
}

pub fn get_time_info() -> Option<(u64, f64, f64)> {
    let active = ACTIVE_IPC_PATH
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let get = |p| {
        send_ipc_to(&active.path, json!({"command": ["get_property", p]}))
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v["data"].as_f64())
    };
    let position = get("time-pos")?;
    let duration = get("duration")?;
    let still_current = ACTIVE_IPC_PATH
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|current| current.generation == active.generation && current.path == active.path)
        .unwrap_or(false);
    still_current.then_some((active.generation, position, duration))
}

pub fn toggle_pause() {
    send_ipc(json!({"command": ["cycle", "pause"]}));
}

pub fn seek(s: i64) {
    RECORDING_SEEKED.store(true, Ordering::Relaxed);
    send_ipc(json!({"command": ["seek", s, "relative"]}));
}

fn seconds_to_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

pub fn vol_change(s: i64) {
    send_ipc(json!({ "command": ["add", "volume", s] }));
}

pub fn the_naming_format_in_which_i_have_saved_the_track_locally(
    title: &str,
    artists: &[String],
) -> String {
    let safe_title = title.replace(['/', '\\'], "-");
    let primary_artist = artists
        .first()
        .map(|s| s.replace(['/', '\\'], "-"))
        .unwrap_or_else(|| "Unknown".to_string());
    format!("{} - {}", safe_title, primary_artist)
}

fn short_hash(input: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(input.as_bytes());

    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()[..10]
        .to_string()
}

pub fn cache_file_stem(track: &Track) -> String {
    let id = match &track.video_id {
        Some(video_id) => video_id.clone(),
        None => short_hash(&track.url),
    };

    cache_file_stem_with_id(&track.title, &track.artists, Some(&id), None)
}

pub fn cache_file_stem_with_id(
    title: &str,
    artists: &[String],
    video_id: Option<&str>,
    url: Option<&str>,
) -> String {
    // Sanitise the WHOLE stem. This used to be applied to the id alone, which for a YouTube video id
    // is a no-op, while the title and artist — the parts that actually carry ':', '?', '*' and '"' —
    // went in untouched. On Windows that makes the path illegal, so caching a track called
    // "Song: Part II" failed outright and it was re-fetched from the network every single play.
    const MAX_STEM_BYTES: usize = 200;
    let stem = sanitize_filename(&unsanitized_cache_file_stem(title, artists, video_id, url));
    if stem.len() <= MAX_STEM_BYTES {
        return stem;
    }

    let id = match (video_id, url) {
        (Some(value), _) => sanitize_filename(value),
        (None, Some(value)) => short_hash(value),
        _ => "unknown".to_string(),
    };
    let suffix = format!(" [{}]", id);
    let mut end = MAX_STEM_BYTES.saturating_sub(suffix.len()).min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", stem[..end].trim_end(), suffix)
}

/// The stem exactly as older builds wrote it, kept so files already in the cache are still found
/// rather than silently re-downloaded under a new name.
fn unsanitized_cache_file_stem(
    title: &str,
    artists: &[String],
    video_id: Option<&str>,
    url: Option<&str>,
) -> String {
    let legacy = the_naming_format_in_which_i_have_saved_the_track_locally(title, artists);

    let id = match (video_id, url) {
        (Some(v), _) => v.to_string(),
        (None, Some(u)) => short_hash(u),
        _ => "unknown".to_string(),
    };

    format!("{} [{}]", legacy, sanitize_filename(&id))
}

/// Every filename stem a cached copy of this track could be under, current scheme first.
///
/// Three naming schemes have shipped, so a lookup has to try all of them; checking only the newest
/// meant an already-cached track was downloaded again under a second name.
///
/// The oldest scheme ("Title - Artist", no id) is the exception: a file named that way carries no
/// proof of *which* recording it is, so when the request has a stable video id it is deliberately NOT
/// offered as a candidate. Otherwise two different videos that happen to share a title and artist would
/// resolve to each other's cached audio — the wrong-song bug identity keys exist to prevent. A request
/// with no stable id has only title+artist to go on, so the legacy stem is still tried there.
pub fn cache_lookup_stems(title: &str, artists: &[String], video_id: Option<&str>) -> Vec<String> {
    let mut stems = vec![
        cache_file_stem_with_id(title, artists, video_id, None),
        unsanitized_cache_file_stem(title, artists, video_id, None),
    ];
    if video_id.is_none() {
        stems.push(the_naming_format_in_which_i_have_saved_the_track_locally(
            title, artists,
        ));
    }
    stems.dedup();
    stems
}

/// Path of an already-cached copy of this track in one of `extensions`, if there is one.
///
/// The accepted formats are passed in rather than read from the global config so callers can be
/// precise: under `--lossless` only `flac` counts, because reusing a previously downloaded `.opus`
/// handed the user lossy audio while the lossless request was quietly ignored.
pub fn find_cached_file(
    music_dir: &Path,
    title: &str,
    artists: &[String],
    video_id: Option<&str>,
    extensions: &[&str],
) -> Option<PathBuf> {
    cache_lookup_stems(title, artists, video_id)
        .into_iter()
        .flat_map(|stem| {
            extensions
                .iter()
                .map(|ext| music_dir.join(format!("{}.{}", stem, ext)))
                .collect::<Vec<_>>()
        })
        .find(|p| p.is_file())
}

fn source_is_lossless(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    lower.contains(".tidal") || path.ends_with(".flac") || path.ends_with(".mpd")
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn clear_temp_never_follows_a_symlink_out_of_the_temp_root() {
        let root = std::env::temp_dir().join(format!("whytui-cleartemp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let music = root.join("music");
        let temp = music.join("temp");
        fs::create_dir_all(&temp).unwrap();

        // An external sentinel that cleanup must never reach.
        let external = root.join("external");
        fs::create_dir_all(&external).unwrap();
        let sentinel = external.join("precious.txt");
        fs::write(&sentinel, b"do not delete").unwrap();

        // A reapable (non-pid-named -> unconditionally removed) session dir containing a symlink that
        // escapes the tree, plus a real file.
        let stale = temp.join("stale");
        fs::create_dir_all(&stale).unwrap();
        std::os::unix::fs::symlink(&external, stale.join("escape")).unwrap();
        fs::write(stale.join("real.tmp"), b"x").unwrap();

        // A temp entry that is itself a symlink pointing outside the tree.
        std::os::unix::fs::symlink(&external, temp.join("escape-link")).unwrap();

        // Our own session dir with a loose file, which should be emptied but kept.
        let mine = session_temp_dir(&music);
        fs::create_dir_all(&mine).unwrap();
        fs::write(mine.join("keep.sock"), b"y").unwrap();

        clear_temp(&music);

        // The external target and its file are untouched.
        assert!(external.is_dir(), "external dir was deleted");
        assert!(sentinel.is_file(), "external sentinel file was deleted");
        // The reapable real dir is gone (including the symlink inside it, unlinked not followed).
        assert!(!stale.exists(), "a reapable session dir should be removed");
        // The escaping symlink is removed as a link.
        assert!(
            !temp.join("escape-link").exists(),
            "the escaping symlink should be removed"
        );
        // Our own dir survives; its loose file is cleared.
        assert!(mine.is_dir(), "our own session dir should survive");
        assert!(
            !mine.join("keep.sock").exists(),
            "our own loose files should be cleared"
        );

        // Idempotent on a tree with no temp dir at all.
        let empty = root.join("empty-music");
        fs::create_dir_all(&empty).unwrap();
        clear_temp(&empty); // must not panic

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_player_child_and_its_grandchildren_share_a_killable_group() {
        // A child that spawns a grandchild, both sleeping. die_with_parent must put the child in its
        // own process group so stop_process's killpg tears down the whole tree, not just the child.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 60 & sleep 60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        die_with_parent(&mut cmd);
        let child = cmd.spawn().expect("spawn test child");
        let pid = child.id() as libc::pid_t;

        // setpgid(0,0) makes the child its own group leader, so its pgid equals its pid.
        // SAFETY: getpgid only reads scheduler state.
        let pgid = unsafe { libc::getpgid(pid) };
        assert_eq!(pgid, pid, "child was not placed in its own process group");

        let mut proc = Some(child);
        stop_process(&mut proc, "test", &std::env::temp_dir());

        // The whole group (child + both grandchildren) must be gone. killpg(pgid, 0) returns ESRCH
        // once no member remains; poll briefly to let the orphaned grandchildren be reaped.
        let mut group_gone = false;
        for _ in 0..40 {
            // SAFETY: signal 0 only probes for the group's existence.
            if unsafe { libc::killpg(pgid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                group_gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            group_gone,
            "the child's process group survived stop_process"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cookies_file_is_a_private_regular_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("whytui-cookie-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        // 1. absent -> created as a 0600 regular file
        let p = root.join("cookies.txt");
        prepare_cookies_file(&p).unwrap();
        assert!(p.is_file());
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // 2. existing file with loose perms -> tightened to 0600, content NOT truncated
        fs::write(&p, b"SECRET=1").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        prepare_cookies_file(&p).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read(&p).unwrap(),
            b"SECRET=1",
            "existing cookies were truncated"
        );

        // 3. a symlink squatting on the path -> rejected, and its target left untouched
        let target = root.join("target.txt");
        fs::write(&target, b"innocent").unwrap();
        let link = root.join("cookies-link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            prepare_cookies_file(&link).is_err(),
            "a symlink was accepted"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"innocent",
            "the symlink target was modified"
        );

        // 4. a non-regular file (directory) at the path -> rejected
        let dir_path = root.join("cookies-dir.txt");
        fs::create_dir(&dir_path).unwrap();
        assert!(
            prepare_cookies_file(&dir_path).is_err(),
            "a directory was accepted"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mpv_source_is_placed_behind_an_end_of_options_marker() {
        // A source beginning with `-` from a hostile mirror must not be parsed by mpv as an option.
        let mut cmd = Command::new("mpv");
        append_media_source(&mut cmd, "--script=/tmp/evil.lua");
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec!["--".to_string(), "--script=/tmp/evil.lua".to_string()]
        );
    }

    #[test]
    fn test_sanitize_filename_strips_forbidden_chars() {
        assert_eq!(
            sanitize_filename("title/with/slashes"),
            "title_with_slashes"
        );
        assert_eq!(sanitize_filename("a:b*c?d"), "a_b_c_d");
        assert_eq!(sanitize_filename("no-special-chars"), "no-special-chars");
        assert_eq!(sanitize_filename(r#"path\to\file"#), "path_to_file");
    }

    #[test]
    fn test_sanitize_filename_preserves_normal_chars() {
        assert_eq!(
            sanitize_filename("Song Title 2024.mp3"),
            "Song Title 2024.mp3"
        );
        assert_eq!(sanitize_filename("Artist - (Remix)"), "Artist - (Remix)");
    }

    #[test]
    fn test_the_naming_format_basic() {
        let result = the_naming_format_in_which_i_have_saved_the_track_locally(
            "Song Title",
            &["Artist One".to_string()],
        );
        assert_eq!(result, "Song Title - Artist One");
    }

    #[test]
    fn test_the_naming_format_handles_slashes_in_title() {
        let result = the_naming_format_in_which_i_have_saved_the_track_locally(
            "Song / With / Slashes",
            &["Artist".to_string()],
        );
        assert!(result.contains('-'));
        assert!(!result.contains('/'));
    }

    #[test]
    fn test_the_naming_format_unknown_artist_fallback() {
        let result = the_naming_format_in_which_i_have_saved_the_track_locally("Title", &[]);
        assert_eq!(result, "Title - Unknown");
    }

    #[test]
    fn test_cache_file_stem_with_id_uses_video_id() {
        let result =
            cache_file_stem_with_id("Songname", &["Artist".to_string()], Some("vid123"), None);
        assert!(result.contains("vid123"));
        assert!(result.contains("Songname"));
    }

    #[test]
    fn test_cache_file_stem_with_id_hash_fallback_on_url() {
        let result = cache_file_stem_with_id(
            "Song",
            &["Artist".to_string()],
            None,
            Some("https://example.com/stream/abc"),
        );
        // Should have a hash, not "unknown"
        assert!(!result.contains("unknown"));
        assert!(result.len() > "Song - Artist []".len()); // Has hash suffix
    }

    #[test]
    fn lossless_source_detection_handles_case_and_url_queries() {
        assert!(source_is_lossless("/music/SONG.FLAC"));
        assert!(source_is_lossless("https://example/song.flac?token=abc"));
        assert!(source_is_lossless("/tmp/manifest.MPD"));
        assert!(!source_is_lossless("https://example/song.opus?format=flac"));
    }

    #[test]
    fn cache_stem_is_bounded_and_keeps_its_identity() {
        let stem = cache_file_stem_with_id(
            &"長い曲名".repeat(100),
            &["Artist".to_string()],
            Some("video123"),
            None,
        );
        assert!(stem.len() <= 200);
        assert!(stem.ends_with(" [video123]"));
        assert!(stem.is_char_boundary(stem.len()));
    }

    /// End-to-end proof that caching works: mpv records a real HTTP stream, and the recording is
    /// probed, remuxed, tagged and filed into the cache.
    ///
    /// Served from a local socket on purpose. The googlevideo path cannot be exercised from CI (or,
    /// as of this writing, from here — every stream is answered with 403), and "the feature is
    /// implemented" is not the same claim as "the feature works".
    #[test]
    fn a_recorded_stream_is_probed_remuxed_and_filed_into_the_cache() {
        use std::io::Read as _;
        use std::net::TcpListener;

        for tool in ["mpv", "ffmpeg", "ffprobe"] {
            if !crate::command_exists(tool) {
                eprintln!("skipping: {tool} not installed");
                return;
            }
        }

        let root = std::env::temp_dir().join(format!("whytui-cache-e2e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        // a genuine 6-second opus file to stream
        let src = root.join("source.opus");
        let made = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=6",
                "-c:a",
                "libopus",
            ])
            .arg(&src)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(made, "could not build the test audio file");
        let audio = fs::read(&src).unwrap();

        // minimal HTTP server; mpv opens more than one connection, so keep serving for a while
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = audio.clone();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(false).expect("blocking listener");
            let deadline = std::time::Instant::now() + Duration::from_secs(25);
            while std::time::Instant::now() < deadline {
                let Ok((mut sock, _)) = listener.accept() else {
                    break;
                };
                // drain the request headers
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/ogg\r\nContent-Length: {}\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if sock.write_all(header.as_bytes()).is_err() {
                    continue;
                }
                let _ = sock.write_all(&body);
                let _ = sock.flush();
            }
        });

        let url = format!("http://127.0.0.1:{}/t.opus", port);
        let recording = root.join("take.record.mkv");

        // exactly the flags play_file uses for recording
        let played = Command::new("mpv")
            .arg("--no-video")
            .arg("--msg-level=all=error")
            .arg("--force-window=no")
            .arg(format!("--stream-record={}", recording.display()))
            .arg(&url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(played, "mpv could not play the local stream");
        assert!(
            recording.exists() && fs::metadata(&recording).unwrap().len() > 0,
            "mpv wrote no recording"
        );

        let track = Track::new(
            "E2E Cache Test".to_string(),
            vec!["SelfTest".to_string()],
            String::new(),
            "0:06".to_string(),
            None,
            Some("e2evid".to_string()),
            url.clone(),
        );

        // a track heard right through must be kept
        let outcome = finish_recording(&recording, 1.0, &track, &root).unwrap();
        let saved = match outcome {
            CacheOutcome::Saved(p) => p,
            other => panic!("expected the track to be cached, got {other:?}"),
        };
        assert!(saved.exists(), "cached file missing: {saved:?}");
        assert!(
            fs::metadata(&saved).unwrap().len() > 0,
            "cached file is empty"
        );
        // it must be real, playable audio in a container matching its codec
        let codec = probe_audio_codec(&saved).expect("cached file has no audio stream");
        assert_eq!(
            saved.extension().and_then(|e| e.to_str()),
            Some(container_for_codec(&codec)),
            "cached {saved:?} does not match its codec {codec}"
        );
        // and the raw recording is not left lying around
        assert!(!recording.exists(), "raw recording was not cleaned up");

        // a track barely listened to must NOT be kept
        let recording2 = root.join("take2.record.mkv");
        fs::copy(&saved, &recording2).unwrap();
        let track2 = Track::new(
            "E2E Skipped Test".to_string(),
            vec!["SelfTest".to_string()],
            String::new(),
            "0:06".to_string(),
            None,
            Some("e2evid2".to_string()),
            url,
        );
        match finish_recording(&recording2, 0.10, &track2, &root).unwrap() {
            CacheOutcome::NotListenedEnough { .. } => {}
            other => panic!("a 10%-played track should not be cached, got {other:?}"),
        }
        assert!(!recording2.exists(), "discarded recording was left behind");

        drop(server);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn container_for_codec_keeps_audio_in_something_that_can_hold_it() {
        assert_eq!(container_for_codec("opus"), "opus");
        assert_eq!(container_for_codec("flac"), "flac");
        assert_eq!(container_for_codec("aac"), "m4a");
        assert_eq!(container_for_codec("alac"), "m4a");
        assert_eq!(container_for_codec("vorbis"), "ogg");
        assert_eq!(container_for_codec("mp3"), "mp3");
        // unknown codecs must not be mislabelled as opus; matroska takes anything
        assert_eq!(container_for_codec("wibble"), "mka");
        // and everything we can emit has to be readable back out of the cache
        for codec in ["opus", "flac", "aac", "vorbis", "mp3", "wibble"] {
            assert!(
                CACHED_AUDIO_EXTENSIONS.contains(&container_for_codec(codec)),
                "{codec} maps to a container the offline library ignores"
            );
        }
    }

    /// Builds a real opus-in-matroska file standing in for what mpv's --stream-record produces.
    fn fake_recording(path: &Path, seconds: u32) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={}", seconds))
            .args(["-c:a", "libopus", "-f", "matroska"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("ffmpeg is required to run this test (see README dependencies)");
        assert!(status.success(), "could not build the fake recording");
    }

    /// One test on purpose: these all drive the same process-wide recording/progress statics, so
    /// running them as separate #[test]s would let cargo's parallel runner interleave them.
    #[test]
    fn finish_recording_keeps_only_tracks_that_were_listened_through() {
        let root = std::env::temp_dir().join(format!("whytui-rec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let music = root.join("music");
        fs::create_dir_all(&music).unwrap();

        let make_track = |id: &str, duration: &str| {
            Track::new(
                format!("RecTest {}", id),
                vec!["Tester".to_string()],
                "Album".to_string(),
                duration.to_string(),
                None, // no thumbnail: keeps the test off the network
                Some(id.to_string()),
                "https://example.invalid/stream".to_string(),
            )
        };

        let arm = |track: &Track, secs: u32, position_ms: u64, duration_ms: u64| -> PathBuf {
            let rec =
                session_temp_dir(&music).join(format!("{}.record.mkv", cache_file_stem(track)));
            fake_recording(&rec, secs);
            *ACTIVE_RECORDING.write().unwrap() = Some(rec.clone());
            FURTHEST_POSITION_MS.store(position_ms, Ordering::Relaxed);
            REPORTED_DURATION_MS.store(duration_ms, Ordering::Relaxed);
            rec
        };

        // Goes through claim_recording, the same way the app does, so this covers reading the
        // recording path and the listened fraction together before anything can replace them.
        let finalise = |track: &Track| -> CacheOutcome {
            let (rec, heard, _) = claim_recording(track).expect("a recording should be armed");
            finish_recording(&rec, heard, track, &music).expect("finalising should not error")
        };

        // 1. heard to the end -> cached as .opus, raw recording cleaned up
        let played = make_track("recfull", "0:10");
        let rec = arm(&played, 10, 10_000, 10_000);
        let cached = match finalise(&played) {
            CacheOutcome::Saved(p) => p,
            other => panic!("a fully played track should be cached, got {other:?}"),
        };
        assert!(cached.exists(), "cached file missing: {cached:?}");
        assert_eq!(cached.extension().and_then(|e| e.to_str()), Some("opus"));
        assert!(
            !rec.exists(),
            "raw recording should be removed once published"
        );
        // and it is findable again, so it is not re-fetched next time
        assert!(
            find_cached_file(
                &music,
                &played.title,
                &played.artists,
                Some("recfull"),
                CACHED_AUDIO_EXTENSIONS
            )
            .is_some()
        );

        // 2. barely played -> nothing kept
        let skipped = make_track("recskip", "0:10");
        let rec = arm(&skipped, 10, 1_000, 10_000);
        assert!(
            matches!(finalise(&skipped), CacheOutcome::NotListenedEnough { .. }),
            "a track skipped after 10% should not be cached"
        );
        assert!(
            !rec.exists(),
            "raw recording should be removed when discarded"
        );

        // 3. threshold reached but the recording is truncated -> refuse, don't cache a partial song
        let truncated = make_track("rectrunc", "1:00");
        let rec = arm(&truncated, 6, 60_000, 60_000);
        assert!(
            matches!(finalise(&truncated), CacheOutcome::Incomplete { .. }),
            "a recording holding only 6s of a 60s track should be rejected"
        );
        assert!(!rec.exists());

        // 4. nothing armed -> claim finds nothing, rather than grabbing someone else's recording
        assert!(claim_recording(&played).is_none());

        // 5. the race that broke this in the real app: the next track installs its own recording
        //    while the previous one is still being finalised. Claiming must have already taken the
        //    old path, so the new track's file is left completely alone.
        let first = make_track("recrace1", "0:10");
        let first_rec = arm(&first, 10, 10_000, 10_000);
        let (claimed, heard, _) = claim_recording(&first).unwrap();
        assert_eq!(claimed, first_rec);

        let second = make_track("recrace2", "0:10");
        let second_rec = arm(&second, 10, 10_000, 10_000); // stands in for the next play_file()
        assert!(matches!(
            finish_recording(&claimed, heard, &first, &music),
            Ok(CacheOutcome::Saved(_))
        ));
        assert!(
            second_rec.exists(),
            "finalising the previous track must not consume the next track's recording"
        );
        assert!(matches!(finalise(&second), CacheOutcome::Saved(_)));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_ipc_reply_skips_events_and_matches_the_request() {
        // mpv interleaves event broadcasts with replies on the same socket
        let stream = concat!(
            r#"{"event":"seek"}"#,
            "\n",
            r#"{"event":"playback-restart"}"#,
            "\n",
            r#"{"data":12.5,"request_id":7,"error":"success"}"#,
            "\n"
        );
        let mut r = std::io::Cursor::new(stream);
        let reply = read_ipc_reply(&mut r, 7).expect("reply should be found past the events");
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["data"].as_f64(), Some(12.5));
    }

    #[test]
    fn read_ipc_reply_skips_a_reply_for_a_different_request() {
        let stream = concat!(
            r#"{"data":1.0,"request_id":5,"error":"success"}"#,
            "\n",
            r#"{"data":2.0,"request_id":6,"error":"success"}"#,
            "\n"
        );
        let mut r = std::io::Cursor::new(stream);
        let reply = read_ipc_reply(&mut r, 6).unwrap();
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["data"].as_f64(), Some(2.0));
    }

    #[test]
    fn read_ipc_reply_accepts_a_reply_without_a_request_id() {
        // older mpv builds do not echo request_id; better to accept than to hang
        let mut r = std::io::Cursor::new("{\"data\":3.0,\"error\":\"success\"}\n");
        assert!(read_ipc_reply(&mut r, 9).is_some());
    }

    #[test]
    fn read_ipc_reply_gives_up_on_an_endless_event_stream() {
        let events = "{\"event\":\"tick\"}\n".repeat(200);
        let mut r = std::io::Cursor::new(events);
        assert!(read_ipc_reply(&mut r, 1).is_none());
    }

    #[test]
    fn cache_stem_strips_characters_windows_forbids_from_the_title() {
        // sanitisation used to be applied to the video id (already safe) and not to the title, so a
        // stem like "Song: Part II? - Artist [vid]" was an illegal Windows filename
        let stem = cache_file_stem_with_id(
            r#"Song: Part II? *mix* "live" <x>|y"#,
            &["AC/DC".to_string()],
            Some("vid123"),
            None,
        );
        for bad in ['/', '\\', ':', '*', '?', '"', '<', '>', '|'] {
            assert!(!stem.contains(bad), "{bad:?} survived in {stem:?}");
        }
        assert!(stem.contains("vid123"));
    }

    #[test]
    fn cache_lookup_stems_are_identity_first() {
        // With a stable id, every candidate must embed it. The id-less legacy stem is unverifiable, so
        // it is NOT offered — two different videos sharing a title/artist would otherwise collide.
        let with_id = cache_lookup_stems("Song: Part II", &["Artist".to_string()], Some("vid123"));
        assert!(
            with_id.iter().all(|stem| stem.contains("vid123")),
            "an id-less stem was offered for an id-bearing request: {with_id:?}"
        );
        assert!(with_id.contains(&cache_file_stem_with_id(
            "Song: Part II",
            &["Artist".to_string()],
            Some("vid123"),
            None
        )));
        assert!(with_id.iter().any(|stem| stem.contains("Song: Part II")));
        assert!(!with_id.contains(&"Song: Part II - Artist".to_string()));

        // Without a stable id, title+artist is the only identity, so the oldest scheme is still tried.
        let no_id = cache_lookup_stems("Song", &["Artist".to_string()], None);
        assert!(no_id.contains(&"Song - Artist".to_string()));
    }

    #[test]
    fn an_id_bearing_request_ignores_an_unverifiable_legacy_file() {
        let dir = std::env::temp_dir().join(format!("whytui-f9-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // A file from the oldest scheme (title/artist only, no id) that was actually a DIFFERENT video.
        let legacy = dir.join("Blinding Lights - The Weeknd.opus");
        fs::write(&legacy, b"wrong-recording").unwrap();

        // A request carrying a real video id must not resolve to that unverifiable file.
        assert!(
            find_cached_file(
                &dir,
                "Blinding Lights",
                &["The Weeknd".to_string()],
                Some("REAL_ID"),
                &["opus"],
            )
            .is_none(),
            "an id-bearing request matched an unverifiable legacy file"
        );

        // The id-bearing copy for this exact request is still found.
        let stem = cache_file_stem_with_id(
            "Blinding Lights",
            &["The Weeknd".to_string()],
            Some("REAL_ID"),
            None,
        );
        let id_file = dir.join(format!("{}.opus", stem));
        fs::write(&id_file, b"right").unwrap();
        assert_eq!(
            find_cached_file(
                &dir,
                "Blinding Lights",
                &["The Weeknd".to_string()],
                Some("REAL_ID"),
                &["opus"],
            )
            .as_deref(),
            Some(id_file.as_path())
        );

        // A request with no stable id may still use the legacy file — title/artist is all it has.
        assert!(
            find_cached_file(
                &dir,
                "Blinding Lights",
                &["The Weeknd".to_string()],
                None,
                &["opus"],
            )
            .is_some()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_cached_file_matches_a_file_written_under_the_old_scheme() {
        let dir = std::env::temp_dir().join(format!("whytui-cache-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);

        // a file as an older build would have written it: unsanitised title, colon intact
        let old = dir.join("Song: Part II - Artist [vid123].opus");
        fs::write(&old, b"x").unwrap();

        let found = find_cached_file(
            &dir,
            "Song: Part II",
            &["Artist".to_string()],
            Some("vid123"),
            &["flac", "opus"],
        );
        assert_eq!(found.as_deref(), Some(old.as_path()));

        // under --lossless a cached .opus must not satisfy the lookup
        assert!(
            find_cached_file(
                &dir,
                "Song: Part II",
                &["Artist".to_string()],
                Some("vid123"),
                &["flac"],
            )
            .is_none(),
            "a cached .opus was accepted for a lossless request"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_cached_file_returns_none_when_nothing_is_cached() {
        let dir = std::env::temp_dir().join(format!("whytui-cache-empty-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        assert!(
            find_cached_file(
                &dir,
                "Nope",
                &["Nobody".to_string()],
                Some("zzz"),
                &["flac", "opus"]
            )
            .is_none()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_cached_file_ignores_a_directory_with_an_audio_extension() {
        let dir = std::env::temp_dir().join(format!("whytui-cache-dir-{}", std::process::id()));
        let fake = dir.join("Song - Artist [vid123].opus");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&fake).unwrap();
        assert!(
            find_cached_file(
                &dir,
                "Song",
                &["Artist".to_string()],
                Some("vid123"),
                &["opus"]
            )
            .is_none()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cache_file_stem_with_id_unknown_fallback() {
        let result = cache_file_stem_with_id("Track", &["Artist".to_string()], None, None);
        assert!(result.contains("unknown"));
    }
}
