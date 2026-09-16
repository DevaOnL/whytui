mod api;
mod features;
mod flac;
mod offline;
mod player;
mod track_identity;
mod ui1;
mod ui2;
mod ui3;
mod ui_common;

use crate::api::SongDetails;
use crate::player::clear_temp;
use crate::ui_common::set_status_line;
use crate::{
    flac::fetch_flac_stream_url,
    flac::init_api,
    offline::get_excluded_track_keys,
    ui1::{show_playlists, show_songs},
};
use colored::*;

use crossterm::{
    event::{self, Event},
    execute,
    terminal::{self, Clear, ClearType},
};
use rand::seq::IndexedRandom;
use std::collections::VecDeque;
use std::future::Future;
use std::io::stdout;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{OnceLock, RwLock, mpsc};
use std::thread;
use std::time::Duration;
// -------------------------------------------------------------------
// DATA STRUCTURES
// -------------------------------------------------------------------

#[derive(Debug)]
pub struct AppConfig {
    pub offline_mode: bool,
    pub no_autoplay: bool,
    pub lossless_mode: bool,
    pub peak_lossless_mode: bool,
    pub game_mode: bool,
    pub download_mode: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub title: String,
    pub duration: String,
    pub artists: Vec<String>,
    pub album: String,
    pub thumbnail_url: Option<String>,
    pub video_id: Option<String>,
    pub url: String,
    pub playback_context: Option<(String, bool)>,
}

impl Track {
    pub fn new(
        title: String,
        artists: Vec<String>,
        album: String,
        duration: String,
        thumbnail_url: Option<String>,
        video_id: Option<String>,
        url: String,
    ) -> Self {
        Self {
            title,
            artists,
            album,
            duration,
            thumbnail_url,
            video_id,
            url,
            playback_context: None,
        }
    }

    pub fn dummy() -> Self {
        Self::new(
            "Nothing Playing".to_string(), // title
            vec!["~".to_string()],         // artists (Vec<String>)
            "".to_string(),                // album
            "0:00".to_string(),            // duration
            None,                          // thumbnail_url
            None,                          // video_id
            "".to_string(),                // url
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackRequestIdentity {
    key: String,
    source: String,
}

impl TrackRequestIdentity {
    fn new(track: &Track) -> Self {
        Self {
            key: track_identity::track_key(track),
            source: track.url.clone(),
        }
    }

    fn matches(&self, track: &Track) -> bool {
        self.key == track_identity::track_key(track) && self.source == track.url
    }
}

#[derive(Debug)]
enum SourceRequestAction {
    PlayNow,
    QueueHead {
        queued: TrackRequestIdentity,
    },
    Previous {
        current: TrackRequestIdentity,
        history: TrackRequestIdentity,
    },
}

#[derive(Debug)]
struct PendingSourceRequest {
    generation: u64,
    track_key: String,
    track: Track,
    action: SourceRequestAction,
    task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug)]
struct SourceResolutionCompletion {
    generation: u64,
    track_key: String,
    result: Result<String, String>,
}

struct SourceResolutionDriver {
    next_generation: u64,
    pending: Option<PendingSourceRequest>,
    completion_tx: mpsc::Sender<SourceResolutionCompletion>,
    completion_rx: mpsc::Receiver<SourceResolutionCompletion>,
    blocked_queue: Option<TrackRequestIdentity>,
}

impl SourceResolutionDriver {
    fn new() -> Self {
        let (completion_tx, completion_rx) = mpsc::channel();
        Self {
            next_generation: 0,
            pending: None,
            completion_tx,
            completion_rx,
            blocked_queue: None,
        }
    }

    fn start<F>(&mut self, track: Track, action: SourceRequestAction, resolver: F) -> u64
    where
        F: Future<Output = Result<String, String>> + Send + 'static,
    {
        self.cancel_pending();
        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == 0 {
            self.next_generation = 1;
        }

        let generation = self.next_generation;
        let track_key = track_identity::track_key(&track);
        let completion_key = track_key.clone();
        let completion_tx = self.completion_tx.clone();
        let task = tokio::spawn(async move {
            let result = resolver.await;
            let _ = completion_tx.send(SourceResolutionCompletion {
                generation,
                track_key: completion_key,
                result,
            });
        });
        self.pending = Some(PendingSourceRequest {
            generation,
            track_key,
            track,
            action,
            task: Some(task),
        });
        generation
    }

    fn cancel_pending(&mut self) {
        if let Some(mut pending) = self.pending.take()
            && let Some(task) = pending.task.take()
        {
            task.abort();
        }
    }

    fn cancel_queue_head(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|request| matches!(request.action, SourceRequestAction::QueueHead { .. }))
        {
            self.cancel_pending();
        }
        self.blocked_queue = None;
    }

    fn clear_queue_failure(&mut self) {
        self.blocked_queue = None;
    }

    fn queue_is_blocked(&mut self, queued: &TrackRequestIdentity) -> bool {
        if self.blocked_queue.as_ref() == Some(queued) {
            true
        } else {
            self.blocked_queue = None;
            false
        }
    }

    fn mark_queue_failed(&mut self, queued: TrackRequestIdentity) {
        self.blocked_queue = Some(queued);
    }

    fn completion_matches(
        pending: &PendingSourceRequest,
        completion: &SourceResolutionCompletion,
    ) -> bool {
        pending.generation == completion.generation && pending.track_key == completion.track_key
    }

    fn take_completion(&mut self) -> Option<(PendingSourceRequest, Result<String, String>)> {
        while let Ok(completion) = self.completion_rx.try_recv() {
            if self
                .pending
                .as_ref()
                .is_some_and(|pending| Self::completion_matches(pending, &completion))
            {
                let pending = self.pending.take().expect("matching request exists");
                return Some((pending, completion.result));
            }
        }

        let task_stopped = self
            .pending
            .as_ref()
            .and_then(|pending| pending.task.as_ref())
            .is_some_and(tokio::task::JoinHandle::is_finished);
        if task_stopped {
            // Recheck after observing task completion so a just-sent result cannot be mistaken for a
            // resolver that exited without reporting.
            if let Ok(completion) = self.completion_rx.try_recv()
                && self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| Self::completion_matches(pending, &completion))
            {
                let pending = self.pending.take().expect("matching request exists");
                return Some((pending, completion.result));
            }
            let pending = self.pending.take().expect("finished request exists");
            return Some((
                pending,
                Err("Source resolver stopped unexpectedly".to_string()),
            ));
        }

        None
    }

    #[cfg(test)]
    fn install_for_test(&mut self, track: Track, action: SourceRequestAction) -> u64 {
        self.cancel_pending();
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.pending = Some(PendingSourceRequest {
            generation,
            track_key: track_identity::track_key(&track),
            track,
            action,
            task: None,
        });
        generation
    }

    #[cfg(test)]
    fn complete_for_test(
        &self,
        generation: u64,
        track_key: String,
        result: Result<String, String>,
    ) {
        self.completion_tx
            .send(SourceResolutionCompletion {
                generation,
                track_key,
                result,
            })
            .expect("test completion receiver should exist");
    }
}

impl Drop for SourceResolutionDriver {
    fn drop(&mut self) {
        self.cancel_pending();
    }
}

// ----------------------------------------------------------------------------------
// GLOBAL STATE
// ----------------------------------------------------------------------------------
//
static CONFIG: OnceLock<AppConfig> = OnceLock::new();
static SONG_QUEUE: RwLock<Vec<Track>> = RwLock::new(Vec::new());
// STORES ALL DETAILS OF UPCOMING SONGS
static RELATED_SONG_LIST: RwLock<Vec<api::SongDetails>> = RwLock::new(Vec::new());
static RECENTLY_PLAYED: RwLock<VecDeque<Track>> = RwLock::new(VecDeque::new());
const HISTORY_LIMIT: usize = 50;
//TO KEEP CONSISTENT VOLUME LEVEL ACROSS TRACKS (TO BE READ BY player.rs)
pub static VOLUME: AtomicI64 = AtomicI64::new(75);

pub static IS_PLAYING: AtomicBool = AtomicBool::new(false);
pub static IS_LOSSLESS: AtomicBool = AtomicBool::new(false);
pub static PLAYING_LOSSLESS: AtomicBool = AtomicBool::new(false);
static VIEW_MODE: RwLock<String> = RwLock::new(String::new());
static UI_MODE: AtomicUsize = AtomicUsize::new(0);
static REPEAT_MODE: AtomicUsize = AtomicUsize::new(0);
pub static OUTPUT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

pub(crate) fn shutdown_started() -> bool {
    SHUTTING_DOWN.load(Ordering::SeqCst)
}

fn run_if_output_active(shutting_down: &AtomicBool, f: impl FnOnce()) -> bool {
    if shutting_down.load(Ordering::SeqCst) {
        return false;
    }
    f();
    true
}

fn with_output_lock<F>(f: F)
where
    F: FnOnce(),
{
    let _guard = OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    run_if_output_active(&SHUTTING_DOWN, f);
}

/// Wipe the screen, serialised against the redraw threads.
fn clear_screen() {
    with_output_lock(|| {
        let _ = execute!(stdout(), Clear(ClearType::All));
    });
}

//LIST OF SONGS FROM A PLAYLIST
static LIBRARY_SONG_LIST: RwLock<Vec<SongDetails>> = RwLock::new(Vec::new());
pub static LYRIC_OFFSET: AtomicI64 = AtomicI64::new(0);
static AUTOPLAY_GENERATION: AtomicU64 = AtomicU64::new(0);
static AUTOPLAY_COMMIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Keys of tracks the user queued by hand (Shift+digit).
///
/// Starting a new song replaces the autoplay mix, but it used to clear the *whole* queue, silently
/// throwing away explicit picks along with it. These keys are what lets the two be told apart.
static USER_QUEUED_KEYS: RwLock<Vec<String>> = RwLock::new(Vec::new());
// Set when 'q' is pressed inside a nested prompt (search selection, library, guess, ...). Those
// prompts consume the keystroke themselves, so they raise this flag and the main loop does the
// actual teardown on its next pass.
static SHOULD_QUIT: AtomicBool = AtomicBool::new(false);

fn request_quit() {
    SHOULD_QUIT.store(true, Ordering::SeqCst);
}

/// Tracks that failed to play, back to back.
///
/// The idle path advances to the next queued track whenever nothing is playing, which is checked every
/// 250ms. When the tracks themselves are unplayable — expired stream URLs, no network, YouTube
/// refusing the stream — that turned into a runaway: four tracks a second drained the queue, and each
/// drain re-armed autoplay, which issued another API call and up to five more `yt-dlp` resolutions.
/// Measured at 80 mpv spawns in 20 seconds. Hammering YouTube like that is exactly what gets a client
/// rate-limited into the HTTP 403s it was reacting to in the first place, so the failures fed
/// themselves. Auto-advance stops after this many consecutive failures and waits for the user.
/// How many upcoming tracks autoplay resolves stream URLs for in one pass.
///
/// Each one costs a `yt-dlp` invocation, and the URL it returns is short-lived — resolving five at a
/// time meant most of them sat in the queue going stale, so by the time the fourth or fifth played its
/// URL was dead and the track failed. Two is enough to keep the queue fed (autoplay tops up whenever
/// fewer than two remain) while cutting both the request volume and the staleness window.
const AUTOPLAY_PREFETCH_DEPTH: usize = 2;

static CONSECUTIVE_PLAYBACK_FAILURES: AtomicUsize = AtomicUsize::new(0);
const MAX_CONSECUTIVE_PLAYBACK_FAILURES: usize = 3;

fn note_playback_failed() {
    CONSECUTIVE_PLAYBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// Called when a track really played, and whenever the user asks for something explicitly — their
/// keypress is the signal to start trying again.
fn reset_playback_failures() {
    CONSECUTIVE_PLAYBACK_FAILURES.store(0, Ordering::Relaxed);
}

fn playback_is_failing() -> bool {
    CONSECUTIVE_PLAYBACK_FAILURES.load(Ordering::Relaxed) >= MAX_CONSECUTIVE_PLAYBACK_FAILURES
}

/// True while a chooser (search results, playlists, the library pager, the guess prompt) owns the
/// screen.
///
/// Listings used to be appended below the banner, at a cursor parked on the very last row. On any
/// terminal as short as the recommended 80x24 that scrolled the screen once per printed line, which
/// both pushed the first entries out of view — "Select (1-5)" with only 3, 4 and 5 still on screen —
/// and permanently shifted every absolutely-positioned element, because the banner and the 300ms
/// monitor repaint address fixed rows. On top of that both of those kept painting *through* the list.
/// While this is set, the listing has the screen to itself.
pub static PROMPT_ACTIVE: AtomicBool = AtomicBool::new(false);

struct PromptOwnership {
    next_token: u64,
    current_token: u64,
}

impl PromptOwnership {
    const fn new() -> Self {
        Self {
            next_token: 0,
            current_token: 0,
        }
    }

    fn claim(&mut self) -> u64 {
        self.next_token = self.next_token.wrapping_add(1);
        if self.next_token == 0 {
            self.next_token = 1;
        }
        self.current_token = self.next_token;
        self.current_token
    }

    fn try_claim(&mut self) -> Option<u64> {
        (self.current_token == 0).then(|| self.claim())
    }

    fn release(&mut self, token: u64) -> bool {
        if self.current_token != token {
            return false;
        }
        self.current_token = 0;
        true
    }
}

static PROMPT_OWNERSHIP: std::sync::Mutex<PromptOwnership> =
    std::sync::Mutex::new(PromptOwnership::new());
const SEARCH_QUERY_PREFIX: &str = "SEARCH_QUERY:";
const SEARCH_CLOSE_PREFIX: &str = "SEARCH_CLOSE:";

enum SearchPromptCompletion {
    Query(String),
    Close,
}

/// RAII claim on the screen for a chooser. Drop only releases ownership; a normal close must call
/// `finish` explicitly so cancellation and unwinding cannot touch the terminal.
struct PromptScreen {
    token: Option<u64>,
}

impl PromptScreen {
    fn try_enter() -> Option<Self> {
        // Deliberately does NOT clear the screen. Clearing it made the chooser the only thing visible,
        // which hid the banner and the player entirely; the listing is drawn bottom-anchored at
        // absolute rows instead (see ui_common::draw_chooser), so everything above it survives.
        let mut ownership = PROMPT_OWNERSHIP.lock().unwrap_or_else(|e| e.into_inner());
        let token = ownership.try_claim()?;
        PROMPT_ACTIVE.store(true, Ordering::SeqCst);
        Some(Self { token: Some(token) })
    }

    #[cfg(test)]
    fn enter_replacing_for_test() -> Self {
        let mut ownership = PROMPT_OWNERSHIP.lock().unwrap_or_else(|e| e.into_inner());
        let token = ownership.claim();
        PROMPT_ACTIVE.store(true, Ordering::SeqCst);
        Self { token: Some(token) }
    }

    fn release(&mut self) -> bool {
        let Some(token) = self.token.take() else {
            return false;
        };
        let mut ownership = PROMPT_OWNERSHIP.lock().unwrap_or_else(|e| e.into_inner());
        if !ownership.release(token) {
            return false;
        }
        PROMPT_ACTIVE.store(false, Ordering::SeqCst);
        true
    }

    fn token(&self) -> u64 {
        self.token.expect("active prompt has an ownership token")
    }

    fn handoff(mut self) {
        self.token.take();
    }

    fn from_handoff(token: u64) -> Self {
        Self { token: Some(token) }
    }

    fn finish(mut self, on_finish: impl FnOnce()) -> bool {
        let Some(token) = self.token.take() else {
            return false;
        };
        // Keep claims blocked through the repaint. Releasing the token first would let a newer prompt
        // enter and then be erased by this older prompt's completion action.
        let mut ownership = PROMPT_OWNERSHIP.lock().unwrap_or_else(|e| e.into_inner());
        if !ownership.release(token) {
            return false;
        }
        PROMPT_ACTIVE.store(false, Ordering::SeqCst);
        on_finish();
        true
    }
}

impl Drop for PromptScreen {
    fn drop(&mut self) {
        self.release();
    }
}

/// Put the terminal back the way we found it: cooked mode, visible cursor.
fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(stdout(), crossterm::cursor::Show);
}

/// Start the output shutdown while serialised against every terminal writer.
fn begin_shutdown() {
    let _guard = OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
}

/// Flip `SHOULD_QUIT` so the main loop tears down normally.
///
/// Only does an atomic store, which is async-signal-safe; everything else (stopping mpv, restoring
/// the terminal) happens back on the main thread in `quit_app`.
#[cfg(unix)]
extern "C" fn handle_terminating_signal(_sig: libc::c_int) {
    SHOULD_QUIT.store(true, Ordering::SeqCst);
}

/// Turn SIGTERM/SIGHUP/SIGINT into a clean shutdown.
///
/// Without this, `kill whytui` (or closing the terminal) left the shell in raw mode with the cursor
/// hidden, needing a manual `reset`. Note Ctrl-C does *not* arrive here while raw mode is on — the
/// terminal delivers it as a key event instead — but an explicit signal still does.
#[cfg(unix)]
fn install_signal_handlers() {
    // SAFETY: the handler only performs an atomic store, which is async-signal-safe.
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            libc::signal(
                sig,
                handle_terminating_signal as *const () as libc::sighandler_t,
            );
        }
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

/// Restore the terminal if we panic.
///
/// The release profile sets `panic = "abort"`, so there is no unwinding and no destructor will run —
/// but the panic hook still executes before the abort, which is the only chance to hand the terminal
/// back in a usable state instead of leaving raw mode on with the cursor hidden.
/// Set the same no-new-output gate normal shutdown uses, plus `SHOULD_QUIT`, from the panic path.
///
/// This is the load-bearing half of the panic hook. Renderers (`refresh_ui`, the 300ms monitor loop)
/// only stop on `SHUTTING_DOWN`, never on `SHOULD_QUIT` — so without setting it first a live renderer
/// sails past its own checks and re-hides the cursor or repaints *after* the restore below, leaving the
/// shell corrupted. Both are lone atomic stores: lock-free and async-signal-safe, so unlike
/// `begin_shutdown()` this cannot deadlock when the panicking thread already holds `OUTPUT_LOCK`.
fn signal_panic_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    // Also nudge the main loop to tear down: release aborts on panic so it is moot there, but a
    // panicking *worker* thread in a debug build does not end the process, and raw mode is now off.
    SHOULD_QUIT.store(true, Ordering::SeqCst);
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Gate new output before touching the terminal, matching normal-shutdown semantics.
        signal_panic_shutdown();
        // Serialise the restore against a writer that already passed its gate and is mid-draw — but
        // never block: try_lock returns immediately whether the lock is free, held by another thread,
        // or held by *this* thread (the panicking one). A corrupted-but-restored terminal beats a hung
        // panic hook, which is exactly why begin_shutdown()'s blocking lock is avoided here.
        let _guard = OUTPUT_LOCK.try_lock();
        restore_terminal();
        previous(info);
    }));
}

fn run_shutdown_sequence(
    stop_playback: impl FnOnce(),
    stop_monitor: impl FnOnce(),
    restore: impl FnOnce(),
    finish_recording: impl FnOnce(),
    join_workers: impl FnOnce(),
) {
    stop_playback();
    stop_monitor();
    restore();
    finish_recording();
    join_workers();
}

/// Stop playback, restore the terminal and exit. Never returns.
fn quit_app(
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
    source_resolutions: &mut SourceResolutionDriver,
) -> ! {
    begin_shutdown();
    source_resolutions.cancel_pending();

    let title = match current_track {
        Some(track) => {
            if player::heard_any_audio() {
                add_to_history(track.clone());
            }
            track.title.clone()
        }
        None => String::new(),
    };

    run_shutdown_sequence(
        // Called unconditionally: it also removes the mpv IPC socket, which a track that ended on its
        // own leaves behind in the temp dir even though nothing is playing any more.
        || player::stop_process(currently_playing, &title, music_dir),
        ui_common::stop_lyrics,
        || {
            // The gate was set while holding this lock, so queued callbacks can no longer repaint or
            // re-hide the cursor after this restore.
            let _guard = OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            restore_terminal();
        },
        || {
            // mpv is gone and its recording is closed before it is claimed here.
            if let Some(track) = current_track.as_ref() {
                player::finish_recording_on_exit(track, music_dir);
            }
        },
        player::wait_for_recording_workers,
    );

    std::process::exit(0);
}

/// Whether `cmd` is an executable we can actually run.
///
/// Looks it up on PATH rather than running it with `--version`. ffmpeg only accepts `-version` (one
/// dash) and exits 8 on `--version`, so the old probe declared ffmpeg missing on every machine —
/// which made `whytui --download` refuse to start at all, and is why nothing was ever cached.
fn command_exists(cmd: &str) -> bool {
    fn is_executable(path: &std::path::Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            path.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            path.is_file()
        }
    }

    let candidate = std::path::Path::new(cmd);
    if candidate.is_absolute() {
        return is_executable(candidate);
    }

    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path_var).any(|dir| {
        if is_executable(&dir.join(cmd)) {
            return true;
        }
        // Windows stores the extension in the filename
        cfg!(windows) && is_executable(&dir.join(format!("{}.exe", cmd)))
    })
}

fn validate_runtime_dependencies(config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    if !command_exists("mpv") {
        return Err("Missing dependency: mpv. Install mpv and try again.".into());
    }

    if !config.offline_mode && !command_exists("yt-dlp") {
        return Err("Missing dependency: yt-dlp. Install yt-dlp for online playback.".into());
    }

    // Only when downloads can actually happen: offline mode plays local files and never downloads, so
    // requiring ffmpeg for `--offline --download` refused to start over a tool it would never call.
    // ffprobe is needed too — it identifies the recorded codec so the file gets a container that
    // matches it — and it ships alongside ffmpeg.
    if config.download_mode && !config.offline_mode {
        for tool in ["ffmpeg", "ffprobe"] {
            if !command_exists(tool) {
                return Err(format!(
                    "Missing dependency: {}. Install ffmpeg (which provides both ffmpeg and ffprobe) for download mode.",
                    tool
                )
                .into());
            }
        }
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum InputPoll {
    Ready(String),
    Buffered,
    Empty,
    Disconnected,
}

/// A keystroke deferred while a source resolution owns the playback transition, tagged with the
/// playback generation that was live when it was buffered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCommand {
    command: String,
    playback_generation: u64,
}

impl PendingCommand {
    fn now(command: String) -> Self {
        Self {
            command,
            playback_generation: player::current_playback_generation(),
        }
    }
}

/// Whether a command acts on the *currently playing track*, as opposed to global state.
///
/// pause and seek target whatever mpv is live (`player::send_ipc` → the active socket), so replaying a
/// stale one hits the wrong track. Volume and lyric-offset persist through the VOLUME/LYRIC_OFFSET
/// atomics and so are track-independent; navigation, search and quit are screen-global. Only the
/// former are dropped when their owning track is gone.
fn is_track_relative(command: &str) -> bool {
    let command = command.trim();
    command == "pause" || command.starts_with('>') || command.starts_with('<')
}

/// A buffered track-relative command whose owning mpv is no longer the live one must be dropped, never
/// retargeted at whatever is playing now.
fn buffered_command_is_stale(
    command: &str,
    buffered_generation: u64,
    current_generation: u64,
) -> bool {
    is_track_relative(command) && buffered_generation != current_generation
}

fn poll_main_input(
    pending_input: &mut VecDeque<PendingCommand>,
    rx: &mpsc::Receiver<String>,
) -> InputPoll {
    if let Some(item) = pending_input.pop_front() {
        // Drop, rather than retarget, a track-relative command whose track has since been replaced.
        if buffered_command_is_stale(
            &item.command,
            item.playback_generation,
            player::current_playback_generation(),
        ) {
            return InputPoll::Buffered;
        }
        return InputPoll::Ready(item.command);
    }
    match rx.try_recv() {
        // A freshly-arrived keystroke is acted on immediately, so it is current by definition.
        Ok(input) => InputPoll::Ready(input),
        Err(mpsc::TryRecvError::Empty) => InputPoll::Empty,
        Err(mpsc::TryRecvError::Disconnected) => InputPoll::Disconnected,
    }
}

fn poll_input_while_source_pending(
    pending_input: &mut VecDeque<PendingCommand>,
    rx: &mpsc::Receiver<String>,
) -> InputPoll {
    if let Some(index) = pending_input
        .iter()
        .position(|item| matches!(item.command.trim(), "q" | "quit"))
    {
        return InputPoll::Ready(
            pending_input
                .remove(index)
                .expect("quit command index came from this queue")
                .command,
        );
    }
    match rx.try_recv() {
        Ok(input) if matches!(input.trim(), "q" | "quit") => InputPoll::Ready(input),
        Ok(input) => {
            // Do not run another potentially blocking command while a source request owns the
            // playback transition. Buffer it in arrival order, stamped with the playback generation
            // live right now, so a track-relative command is dropped if the track changes before it
            // is replayed.
            pending_input.push_back(PendingCommand::now(input));
            InputPoll::Buffered
        }
        Err(mpsc::TryRecvError::Empty) => InputPoll::Empty,
        Err(mpsc::TryRecvError::Disconnected) => InputPoll::Disconnected,
    }
}

/// Await a spawned network task while keeping playback advancing and quit responsive.
///
/// The main interaction loop must never block on a bare `.await`: a slow request would otherwise
/// freeze track advancement and delay quit for the request's whole duration (up to the client
/// timeout). This drives `poll_playback` between short waits on the task — exactly what the interactive
/// prompts already do on idle — and bails immediately if a shutdown was requested, aborting the task
/// so no cancelled request keeps running. Returns `None` when the task was abandoned (shutdown) or it
/// panicked; otherwise the task's value.
async fn await_while_pumping_playback<T>(
    mut task: tokio::task::JoinHandle<T>,
    yt_client: &api::YTMusic,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
    source_resolutions: &mut SourceResolutionDriver,
) -> Option<T> {
    loop {
        if SHOULD_QUIT.load(Ordering::SeqCst) || shutdown_started() {
            task.abort();
            return None;
        }
        match tokio::time::timeout(Duration::from_millis(120), &mut task).await {
            Ok(Ok(value)) => return Some(value),
            // The task panicked or was cancelled: nothing to deliver.
            Ok(Err(_join_error)) => return None,
            // Still in flight — keep the player moving, then wait on it again.
            Err(_elapsed) => {
                poll_playback(
                    yt_client,
                    current_track,
                    currently_playing,
                    music_dir,
                    source_resolutions,
                );
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ----------------------------------------------------------------------------------
    // PART 1 - GET ARGUMENTS, INITIAL GLOBAL (STATIC) VARIABLES
    // ----------------------------------------------------------------------------------
    let args: Vec<String> = std::env::args().collect();
    let app_config = AppConfig {
        download_mode: args.iter().any(|a| a == "--download" || a == "-d"), //save songs to music_dir
        offline_mode: args.iter().any(|a| a == "--offline" || a == "-o"), //play only from offline library
        no_autoplay: args.iter().any(|a| a == "--nomix" || a == "-n"),    //dont auto queue
        lossless_mode: args
            .iter()
            .any(|a| a == "--lossless" || a == "-l" || a == "--peak-lossless" || a == "-pl"), //try fetching from tidal
        peak_lossless_mode: args.iter().any(|a| a == "--peak-lossless" || a == "-pl"), //try fetching peak from tidal
        game_mode: args.iter().any(|a| a == "--guess" || a == "-g"), //guess quality (to be used with --lossless)
    };
    // Set the global OnceLock
    CONFIG.set(app_config).expect("Failed to set config");

    // Installed before anything touches the terminal, so every abnormal exit still hands it back.
    install_panic_hook();
    install_signal_handlers();

    // Validate runtime dependencies
    validate_runtime_dependencies(config())?;

    //set default view mode to queue
    *VIEW_MODE.write().unwrap_or_else(|e| e.into_inner()) = "queue".to_string();
    //create music_dir and temp dir to store currently playing song
    let music_dir = player::prepare_music_dir()?;
    //set cookie path
    let cookies_path = music_dir.join("config/cookies.txt");
    //Custom unofficial apiz ( call with cookies if available)
    let cookies_str = cookies_path
        .to_str()
        .ok_or_else(|| format!("Cookies path is not valid UTF-8: {:?}", cookies_path))?;

    let yt_client = api::YTMusic::new_with_cookies(cookies_str).map_err(|e| {
        format!(
            "Failed to load YouTube Music cookies from {}: {}",
            cookies_str, e
        )
    })?;
    //mpv handle to extract child and stop songs if needed
    let mut currently_playing: Option<Child> = None;
    //contains song details (including vid_id for online songs)
    let mut current_track: Option<Track> = None;
    let mut source_resolutions = SourceResolutionDriver::new();
    // CLEAR SCREEN BEFORE STARTING THE REAL SHIT
    clear_screen();

    clear_temp(&music_dir);
    if config().lossless_mode && !config().offline_mode {
        println!("Finding fastest FLAC server...");
        match init_api().await {
            Ok(mirror) => println!("Selected: {}", mirror),
            Err(e) => println!("FLAC API init failed: {}", e),
        }
    }
    //
    //
    //
    //
    //

    // ----------------------------------------------------------------------------------
    // PART 2 - SETUP TRANSMITTER, RECIEVER CHANNEL FOR POLLING INPUT
    //         transmitter (sends any input for Search/Command)
    //         receiver (sleeps every 250 ms if not input)
    // ----------------------------------------------------------------------------------

    // let (tx, rx) = mpsc::channel::<String>();
    // thread::spawn(move || {
    //      loop {
    //          let mut s = String::new();
    //          if std::io::stdin().read_line(&mut s).is_ok() {
    //              let _ = tx.send(s.trim().to_string());
    //          }
    //      }
    // });
    let (tx, rx) = mpsc::channel::<String>();
    spawn_input_handler(tx);
    // Keystrokes a command read ahead of time but did not consume, replayed before the channel so
    // that over-draining (e.g. the volume repeat-detector) cannot swallow the next command. Each
    // carries the playback generation it was buffered under, so a stale track-relative command is
    // dropped instead of hitting a track it was never meant for.
    let mut pending_input: VecDeque<PendingCommand> = VecDeque::new();
    //
    //
    //
    //
    //

    // ----------------------------------------------------------------------------------
    // PART 3 - INITIALIZATION
    // ----------------------------------------------------------------------------------

    // -------------------------------------------------------------------
    // TERMINAL SIZE: Support standard 80x24 or larger
    // -------------------------------------------------------------------
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    if cols < 60 || rows < 20 {
        eprintln!(
            "Warning: Terminal is small ({}x{}). Some UI elements may not display correctly.",
            cols, rows
        );
        eprintln!("Recommended minimum: 80x24 or larger.");
    }
    // ----------------------------------------------------------------------------------
    // CASE 1 : IF OFFLINE MODE INITIAL FETCH RANDOM SONG + POPULATE QUEUE
    // ----------------------------------------------------------------------------------
    if config().offline_mode {
        let exclude = get_excluded_track_keys();
        {
            let mut q = SONG_QUEUE.write().unwrap_or_else(|e| e.into_inner());
            offline::populate_queue_offline(&music_dir, &mut q, &exclude);
        }

        if let Some(track) = queue_next() {
            ui_common::clear_lyrics();
            if start_playback(&track, &music_dir, &mut currently_playing) {
                current_track = Some(track.clone());
                refresh_ui(Some(&track));
            } else {
                current_track = None;
                refresh_ui(None);
            }
        } else {
            refresh_ui(Some(&Track::dummy()));
            set_status_line(Some("No local songs found!".to_string()));
            // refresh_ui(None);
        }
    }
    // ----------------------------------------------------------------------------------
    // CASE 2 : IF ONLINE MODE TRY TO CONNECT TO API AND FETCH USERNAME
    // ----------------------------------------------------------------------------------
    else {
        let user_status = yt_client
            .fetch_account_name()
            .await
            .unwrap_or("Error".to_string());

        refresh_ui(Some(&Track::dummy()));
        set_status_line(Some(format!("Wassup {}", user_status)));
    }

    //
    //
    //
    //
    //

    // -------------------------------------------------------------------
    // START OF GAME LOOP (BOTH ONLINE,OFFLINE)
    // -------------------------------------------------------------------
    loop {
        // if event::poll(Duration::from_millis(0))? {
        //      match event::read()? {
        //          Event::Resize(_, _) => {
        //              execute!(stdout(), Clear(ClearType::All))?;
        //              refresh_ui(None);
        //          }
        //          _ => {}
        //      }
        // }

        // a nested prompt swallowed a 'q' and asked us to shut down
        if SHOULD_QUIT.load(Ordering::SeqCst) {
            quit_app(
                &mut current_track,
                &mut currently_playing,
                &music_dir,
                &mut source_resolutions,
            );
        }

        // -------------------------------------------------------------------
        // PART 4 - ADVANCE PLAYBACK IF THE CURRENT TRACK FINISHED
        // -------------------------------------------------------------------
        poll_playback(
            &yt_client,
            &mut current_track,
            &mut currently_playing,
            &music_dir,
            &mut source_resolutions,
        );

        //
        //
        //
        //
        //

        // -------------------------------------------------------------------
        // PART 5 - CHECK FOR ANY INPUT FROM USER VIA RX
        // -------------------------------------------------------------------
        let input_poll = if source_resolutions.pending.is_some() {
            poll_input_while_source_pending(&mut pending_input, &rx)
        } else {
            poll_main_input(&mut pending_input, &rx)
        };
        let input = match input_poll {
            InputPoll::Ready(input) => input,
            // An item was consumed, so another immediate pass is finite work rather than a busy loop.
            InputPoll::Buffered => continue,
            InputPoll::Empty => {
                // No input or completed source job: wait before the next nonblocking poll.
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            // The input thread is gone, so no keystroke will ever arrive again.
            InputPoll::Disconnected => {
                quit_app(
                    &mut current_track,
                    &mut currently_playing,
                    &music_dir,
                    &mut source_resolutions,
                );
            }
        };

        // -------------------------------------------------------------------
        // CASE 1 : IF USER SIMLPY PRESSED ENTER REFRESH UI TO FIX ANY SCROLL
        // -------------------------------------------------------------------
        if input.is_empty() {
            clear_screen();
            refresh_ui(None);
            continue;
        }

        // -------------------------------------------------------------------
        // CASE 2 : SEARCH QUERIES ARE PREFIXED; EVERYTHING ELSE IS A KEYBIND TOKEN
        //
        // Queries used to travel down the same channel as keybind tokens and were run through
        // handle_global_commands first, so searching for "q" quit the app, "clear" emptied the
        // queue and "next" skipped the track. Splitting them here also means an unrecognised
        // token is simply ignored instead of being searched for: pressing Enter used to fire a
        // search for the literal string "enter".
        // -------------------------------------------------------------------
        let query = if let Some(encoded) = input.strip_prefix(SEARCH_QUERY_PREFIX) {
            let Some((token, query)) = encoded.split_once(':') else {
                continue;
            };
            let Ok(token) = token.parse::<u64>() else {
                continue;
            };
            let screen = PromptScreen::from_handoff(token);
            if SHOULD_QUIT.load(Ordering::SeqCst) {
                continue;
            }
            if !screen.finish(|| {
                clear_screen();
                refresh_ui(current_track.as_ref());
            }) {
                continue;
            }
            query.to_string()
        } else if let Some(token) = input.strip_prefix(SEARCH_CLOSE_PREFIX) {
            let Ok(token) = token.parse::<u64>() else {
                continue;
            };
            let screen = PromptScreen::from_handoff(token);
            if SHOULD_QUIT.load(Ordering::SeqCst) {
                continue;
            }
            screen.finish(|| {
                clear_screen();
                refresh_ui(current_track.as_ref());
                set_status_line(None);
            });
            continue;
        } else if let Some(query) = input.strip_prefix('/') {
            query.to_string()
        } else {
            let _ = handle_global_commands(
                &input,
                &rx, // Passed RX so library can use it
                &mut pending_input,
                &yt_client,
                &mut current_track,
                &mut currently_playing,
                &music_dir,
                &mut source_resolutions,
            )
            .await;
            continue;
        };
        if SHOULD_QUIT.load(Ordering::SeqCst) {
            continue;
        }

        // -------------------------------------------------------------------
        // CASE 3 : SEARCH IS NOT AVAILABLE IN OFFLINE MODE
        // -------------------------------------------------------------------
        if config().offline_mode {
            refresh_ui(None);
            set_status_line(Some("Nope not here".to_string()));
            continue;
        }

        // -------------------------------------------------------------------
        // CASE 4 : USE THE RECEIVED QUERY TO SEARCH THE CUSTOM API
        // -------------------------------------------------------------------
        // Run the search off the main await so playback keeps advancing and quit stays responsive
        // while it is in flight — a slow search used to freeze both for its whole duration.
        set_status_line(Some("Searching...".to_string()));
        let search_task = {
            let yt = yt_client.clone();
            let query = query.clone();
            tokio::spawn(async move { yt.search_songs(&query, 5).await.map_err(|e| e.to_string()) })
        };
        let songs = match await_while_pumping_playback(
            search_task,
            &yt_client,
            &mut current_track,
            &mut currently_playing,
            &music_dir,
            &mut source_resolutions,
        )
        .await
        {
            Some(Ok(songs)) => songs,
            Some(Err(_)) => {
                refresh_ui(None);
                set_status_line(Some("Search failed (retry)".to_string()));
                continue;
            }
            // Shutdown was requested (or the task died) while searching: let the loop tear down.
            None => continue,
        };

        // -------------------------------------------------------------------
        // CASE 4.1 : IF NO RESULTS SIMPLY REFRESH UI
        // -------------------------------------------------------------------
        if songs.is_empty() {
            refresh_ui(None);
            set_status_line(Some("Search failed (retry)".to_string()));
            continue;
        }

        //
        //
        //
        //

        // -------------------------------------------------------------------
        // PART 6 - IF NONE OF THE ABOVE AND IN ONLINE MODE,
        //         USE RECIEVED TEXT TO SEARCH CUSTOM API
        // -------------------------------------------------------------------
        // -------------------------------------------------------------------
        // CASE 1 : IF USING MINIMAL UI SIMULATE AUTO SELECTING FIRST RESULT
        // -------------------------------------------------------------------

        // The playlist context lives on each Track. Only a selection that actually starts playback
        // replaces it; cancelling the prompt or Shift+digit queueing leaves the current mix alone.
        if UI_MODE.load(Ordering::Relaxed) == 2 {
            // simulate selecting the first result
            handle_song_selection(
                "1".to_string(),
                &songs,
                &music_dir,
                &yt_client,
                &mut current_track,
                &mut currently_playing,
                None,
                &mut source_resolutions,
            )
            .unwrap_or_else(|e| {
                with_output_lock(|| {
                    println!("Auto-select error: {}", e);
                });
            });
            //finish this loop
            continue;
        }

        // -------------------------------------------------------------------
        // CASE 2 : IF IN OTHER UI MODE TAKE INPUT FROM USER FOR SELECTION
        // -------------------------------------------------------------------
        // Bound in its own statement so the closure's borrows are released before
        // handle_song_selection needs them mutably.
        let selection = {
            // The results own the screen while the user chooses. Printed below the banner they
            // scrolled the terminal and the first entries vanished off the top.
            let Some(screen) = PromptScreen::try_enter() else {
                continue;
            };
            with_output_lock(|| {
                show_songs(&songs);
            });

            let selection = read_song_selection(&rx, songs.len(), || {
                // keep playback moving; nothing repaints over us now, so there is nothing to redraw
                poll_playback(
                    &yt_client,
                    &mut current_track,
                    &mut currently_playing,
                    &music_dir,
                    &mut source_resolutions,
                );
            });
            if !SHOULD_QUIT.load(Ordering::SeqCst) {
                screen.finish(|| {
                    clear_screen();
                    refresh_ui(current_track.as_ref());
                });
            }
            selection
        };

        if SHOULD_QUIT.load(Ordering::SeqCst) {
            continue;
        }
        if let Some(sel_str) = selection {
            handle_song_selection(
                sel_str,
                &songs,
                &music_dir,
                &yt_client,
                &mut current_track,
                &mut currently_playing,
                None,
                &mut source_resolutions,
            )
            .unwrap_or_else(|e| {
                set_status_line(Some(format!(":( Error playing song: {}", e)));
            });
        }
    }
}

use std::path::PathBuf;

#[allow(clippy::too_many_arguments)]
async fn handle_global_commands(
    input: &str,
    rx: &std::sync::mpsc::Receiver<String>,
    pending_input: &mut VecDeque<PendingCommand>,
    yt_client: &api::YTMusic,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
    source_resolutions: &mut SourceResolutionDriver,
) -> bool {
    // let title_ref = current_track.as_ref().map(|t| t.title.as_str()); //now playing song title to pass to refresh_ui
    let current_mode = UI_MODE.load(Ordering::Relaxed); //current ui mode

    // Seek check
    if input.starts_with('>') || input.starts_with('<') {
        if let Ok(s) = input[1..].trim().parse::<i64>() {
            player::seek(if input.starts_with('<') { -s } else { s });
        }
        // refresh_ui(None);
        return true;
    }

    // special commands
    match input {
        "REFRESH_UI" => {
            // execute!(stdout(), Clear(ClearType::All));
            refresh_ui(None);
            set_status_line(None);
            true
        }
        // The README's "press Enter to tidy the screen". The input thread sends the token "enter";
        // only ESC sends "", so the main loop's is_empty() branch never saw an Enter press.
        "enter" => {
            clear_screen();
            refresh_ui(None);
            true
        }
        // only meaningful inside a selection prompt, so swallow it at top level
        "backspace" => true,
        "q" | "quit" => {
            quit_app(
                current_track,
                currently_playing,
                music_dir,
                source_resolutions,
            );
        }
        // "s" | "stop" => {
        //     if let Some(track) = current_track {
        //         add_to_history(track.clone());
        //         player::stop_process(currently_playing, &track.title, music_dir);
        //     }
        //     *current_track = None;
        //     refresh_ui(None);
        //     set_status_line(Some(format!("STOPPED SONG")));
        //     return true;
        // }
        "c" | "clear" => {
            invalidate_autoplay_generation();
            source_resolutions.cancel_queue_head();
            SONG_QUEUE
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            USER_QUEUED_KEYS
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            set_status_line(Some("QUEUE CLEARED".to_string()));
            true
        }
        s if s == "+" || s == "-" => {
            let mut delta: i64 = if s == "+" { 5 } else { -5 };

            while let Ok(next_input) = rx.try_recv() {
                if next_input == "+" {
                    delta += 5;
                } else if next_input == "-" {
                    delta -= 5;
                } else {
                    // not a volume key: hand it back rather than swallowing the keystroke
                    pending_input.push_back(PendingCommand::now(next_input));
                    break;
                }
            }

            let current = VOLUME.load(Ordering::Relaxed);
            let new_vol = (current + delta).clamp(0, 150);
            VOLUME.store(new_vol, Ordering::Relaxed);

            player::vol_change(delta);

            set_status_line(Some(format!("VOLUME {}", new_vol)));

            true
        }

        "pause" => {
            if currently_playing.is_some() {
                player::toggle_pause();
                set_status_line(Some("PAUSED/PLAYED".to_string()));
            }
            true
        }
        "l" | "like" => {
            // If track is playing,playing track has a vid_id
            if let Some(track) = current_track
                && let Some(vid) = &track.video_id
            {
                let yt = yt_client.clone();
                let video_id = vid.clone();

                tokio::spawn(async move {
                    match yt.like_song(&video_id).await {
                        Ok(_) => {
                            set_status_line(Some("Added to Liked Songs!".to_string()));
                        }
                        Err(e) => {
                            set_status_line(Some(format!(":( Couldn't like song: {}", e)));
                        }
                    }
                });
            }
            refresh_ui(None);
            true
        }
        "a" | "add" => {
            // Copied out of current_track up front so the idle hook below is free to borrow the
            // playback state mutably while the playlist prompt is on screen.
            let video_id = current_track.as_ref().and_then(|t| t.video_id.clone());

            if let Some(video_id) = video_id {
                let fetch = {
                    let yt = yt_client.clone();
                    tokio::spawn(async move {
                        yt.fetch_library_playlists()
                            .await
                            .map_err(|e| e.to_string())
                    })
                };
                match await_while_pumping_playback(
                    fetch,
                    yt_client,
                    current_track,
                    currently_playing,
                    music_dir,
                    source_resolutions,
                )
                .await
                {
                    Some(Ok(playlists)) => {
                        let selection = {
                            let Some(screen) = PromptScreen::try_enter() else {
                                return true;
                            };
                            with_output_lock(|| {
                                show_playlists(&playlists);
                            });

                            let selection = read_number_selection(rx, playlists.len(), || {
                                poll_playback(
                                    yt_client,
                                    current_track,
                                    currently_playing,
                                    music_dir,
                                    source_resolutions,
                                );
                            });
                            if SHOULD_QUIT.load(Ordering::SeqCst) {
                                return true;
                            }
                            screen.finish(|| {
                                clear_screen();
                                refresh_ui(current_track.as_ref());
                            });
                            selection
                        };

                        if let Some(sel) = selection {
                            let selected_playlist_id = playlists[sel - 1].playlist_id.clone();
                            let yt = yt_client.clone();

                            tokio::spawn(async move {
                                match yt.add_to_playlist(&selected_playlist_id, &video_id).await {
                                    Ok(_) => {
                                        set_status_line(Some("Added to Playlist!".to_string()))
                                    }
                                    Err(e) => set_status_line(Some(format!(":( Error: {}", e))),
                                }
                            });
                        }
                    }
                    Some(Err(error)) => set_status_line(Some(error)),
                    // Shutdown requested while fetching the library: fall through to teardown.
                    None => return true,
                }
            }
            // current_track, not None: the playlist prompt runs poll_playback on idle, so the track
            // can auto-advance while it is open — re-arm the monitor for what is actually playing.
            refresh_ui(current_track.as_ref());
            true
        }
        "u" | "user" => {
            // Fire-and-forget, like `like`/`add`: the greeting is non-interactive, so there is no
            // reason to block the loop on the round-trip.
            let yt = yt_client.clone();
            tokio::spawn(async move {
                let user_status = yt
                    .fetch_account_name()
                    .await
                    .unwrap_or_else(|_| "Error".to_string());
                set_status_line(Some(format!("Wassup {}", user_status)));
            });
            true
        }
        "t" | "translate" => {
            let (mode, availability) = ui_common::cycle_lyric_display_mode();
            match (mode, availability) {
                (1, ui_common::LyricVariantAvailability::Available) => {
                    set_status_line(Some("ROMANIZED LYRICS".to_string()))
                }
                (1, ui_common::LyricVariantAvailability::Loading) => {
                    set_status_line(Some("Fetching romanization...".to_string()))
                }
                (1, ui_common::LyricVariantAvailability::Unavailable) => {
                    set_status_line(Some("No romanization found".to_string()))
                }
                (2, ui_common::LyricVariantAvailability::Available) => {
                    set_status_line(Some("TRANSLATED LYRICS".to_string()))
                }
                (2, ui_common::LyricVariantAvailability::Loading) => {
                    set_status_line(Some("Fetching translation...".to_string()))
                }
                (2, ui_common::LyricVariantAvailability::Unavailable) => {
                    set_status_line(Some("No translation found".to_string()))
                }
                _ => set_status_line(Some("ORIGINAL LYRICS".to_string())),
            }
            refresh_ui(None);
            true
        }
        // "w" | "wrong" => {
        //     ui_common::stop_lyrics();
        //     ui_common::clear_lyrics();
        //     set_status_line(Some(format!("sorry... stopped lyrics")));
        //     refresh_ui(None);
        //     return true;
        // }
        //toggle between the ui modes
        "v" | "view" => {
            let next_ui_mode = (current_mode + 1) % 3;
            UI_MODE.store(next_ui_mode, Ordering::Relaxed);

            clear_screen();
            let monitored_track = current_track.clone().unwrap_or_else(Track::dummy);
            restart_monitor_for_current_view(&monitored_track);
            refresh_ui(current_track.as_ref());
            true
        }
        "r" | "recents" => {
            {
                let mut mode = VIEW_MODE.write().unwrap_or_else(|e| e.into_inner());
                *mode = if *mode == "queue" {
                    "recent".to_string()
                } else {
                    "queue".to_string()
                };
            }
            refresh_ui(None);
            true
        }
        "R" | "repeat" => {
            let current_repeat = REPEAT_MODE.load(Ordering::Relaxed);
            let next_repeat_mode = (current_repeat + 1) % 3;

            REPEAT_MODE.store(next_repeat_mode, Ordering::Relaxed);
            let status = if next_repeat_mode == 0 {
                "No Repeat"
            } else if next_repeat_mode == 1 {
                "Repeat Once"
            } else {
                "Repeat Forever"
            };
            set_status_line(Some(status.to_string()));
            true
        }
        "n" | "next" => {
            // an explicit skip is the user asking us to try again
            reset_playback_failures();
            // No unconditional invalidate here: advance_to_next_queued mints a fresh generation when
            // it starts a track (which supersedes any in-flight fetch anyway), and cancelling without
            // arming a replacement is what used to strand the session with an empty queue forever.
            let leaving = current_track.clone();

            if let Some(track) = leaving.as_ref() {
                leave_current_track(track, currently_playing, music_dir);
            }

            source_resolutions.cancel_pending();
            source_resolutions.clear_queue_failure();
            match advance_to_next_queued(
                yt_client,
                current_track,
                currently_playing,
                music_dir,
                source_resolutions,
            ) {
                AdvanceOutcome::Started => set_status_line(Some("PLAYING NEXT".into())),
                AdvanceOutcome::Pending | AdvanceOutcome::Blocked => {
                    *current_track = None;
                    crate::IS_PLAYING.store(false, Ordering::SeqCst);
                    refresh_ui(Some(&Track::dummy()));
                }
                AdvanceOutcome::Empty => {
                    // Nothing queued. Seed a new mix from the track we just left, or autoplay would have
                    // no way back — then say so and repaint, because leaving the old track on screen made
                    // 'n' on an empty queue look like it did nothing.
                    if let Some(track) = leaving.as_ref() {
                        rearm_autoplay(yt_client, track, music_dir);
                    }
                    *current_track = None;
                    crate::IS_PLAYING.store(false, Ordering::SeqCst);
                    refresh_ui(Some(&Track::dummy()));
                    set_status_line(Some("QUEUE EMPTY".into()));
                }
            }

            true
        }

        "p" | "previous" => {
            // an explicit skip is the user asking us to try again
            reset_playback_failures();
            // Deliberately no invalidate before the checks below: this arm is a no-op when nothing is
            // playing or the history is empty, and cancelling the in-flight mix on a keypress that
            // changes nothing used to kill autoplay for the rest of the session.
            if let Some(track) = current_track.as_ref() {
                if let Some(prev_track) = peek_prev_track() {
                    source_resolutions.cancel_pending();
                    let current_identity = TrackRequestIdentity::new(track);
                    let history_identity = TrackRequestIdentity::new(&prev_track);
                    if queued_track_needs_resolution(&prev_track) {
                        let youtube_only = !prev_track.url.trim().is_empty();
                        start_source_resolution(
                            source_resolutions,
                            yt_client,
                            prev_track,
                            SourceRequestAction::Previous {
                                current: current_identity,
                                history: history_identity,
                            },
                            youtube_only,
                        );
                        set_status_line(Some("RESOLVING PREVIOUS".into()));
                    } else {
                        commit_previous_track(
                            prev_track,
                            &current_identity,
                            &history_identity,
                            yt_client,
                            current_track,
                            currently_playing,
                            music_dir,
                        );
                    }
                } else {
                    // history is empty; without this 'p' looks like a dead key
                    set_status_line(Some("NO PREVIOUS SONG".into()));
                }
            }
            true
        }
        "L" | "library" => {
            if config().offline_mode || UI_MODE.load(Ordering::Relaxed) == 2 {
                refresh_ui(None);
                return true;
            }

            let ui_mode = UI_MODE.load(Ordering::Relaxed);
            if ui_mode == 2 {
                refresh_ui(None);
                return true;
            }

            //give rx to library helper
            if let Err(e) = handle_library_browsing(
                rx,
                yt_client,
                music_dir,
                current_track,
                currently_playing,
                source_resolutions,
            )
            .await
            {
                // No "Error in Library: " prefix — it ate 18 of the ~31 columns the status line has,
                // leaving the actual reason truncated away.
                set_status_line(Some(e.to_string()));
            }

            if SHOULD_QUIT.load(Ordering::SeqCst) {
                return true;
            }
            // current_track, not None: a track can auto-advance while the library browser is open, so
            // re-arm the monitor for what is actually playing rather than leaving it on the old song.
            refresh_ui(current_track.as_ref());
            true
        }
        "g" | "guess" => {
            if currently_playing.is_none() || !config().game_mode {
                refresh_ui(None);
                set_status_line(Some("NOT NOW!".into()));
                return true;
            }

            let Some(screen) = PromptScreen::try_enter() else {
                return true;
            };
            with_output_lock(|| {
                print!(
                    "\n\n\n\r  --- GUESS THE FORMAT --- \n\r  1) OPUS (Lossy)\n\r  2) FLAC (Lossless)"
                );
                let _ = std::io::stdout().flush();
            });

            // Keep playback moving while we wait for the guess; a plain rx.recv() here left the
            // player silent once the track ran out. If the track does change, the answer changed
            // with it, so abandon the round rather than grade the guess against the wrong song.
            let guess_input = loop {
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(i) => break Some(i),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if SHOULD_QUIT.load(Ordering::SeqCst) {
                            break None;
                        }
                        if poll_playback(
                            yt_client,
                            current_track,
                            currently_playing,
                            music_dir,
                            source_resolutions,
                        ) {
                            break None;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        request_quit();
                        break None;
                    }
                }
            };

            if SHOULD_QUIT.load(Ordering::SeqCst) {
                return true;
            }

            let Some(guess_input) = guess_input else {
                screen.finish(|| {
                    clear_screen();
                    refresh_ui(current_track.as_ref());
                });
                set_status_line(Some("Guess cancelled".into()));
                return true;
            };

            let guess = guess_input.trim();
            if guess == "q" {
                request_quit();
                return true;
            }

            screen.finish(|| {
                clear_screen();
                refresh_ui(current_track.as_ref());
            });
            let is_lossless = PLAYING_LOSSLESS.load(Ordering::SeqCst);

            let correct = match guess {
                "1" => !is_lossless,
                "2" => is_lossless,
                // ESC sends "" (then REFRESH_UI). These used to fall into `_ => false`, so
                // cancelling the round was graded as a wrong guess and gave the answer away.
                "" | "REFRESH_UI" => {
                    set_status_line(Some("Guess cancelled".to_string()));
                    return true;
                }
                // Anything else is not a guess at all. Grading it as a wrong answer gave the
                // format away for any stray keypress — a volume key, a digit, 'v', '-'.
                _ => {
                    set_status_line(Some("Invalid Input".to_string()));
                    return true;
                }
            };

            if correct {
                set_status_line(Some("CORRECT GUESS!".into()));
            } else {
                let actual = if is_lossless { "FLAC" } else { "OPUS" };
                set_status_line(Some(format!("WRONG! It was {}", actual)));
            }

            true
        }
        s if s == "[" || s == "]" => {
            let mut delta: i64 = if s == "]" { 100 } else { -100 };

            while let Ok(next_input) = rx.try_recv() {
                if next_input == "]" {
                    delta += 100;
                } else if next_input == "[" {
                    delta -= 100;
                } else {
                    // not a lyric-offset key: hand it back rather than swallowing the keystroke
                    pending_input.push_back(PendingCommand::now(next_input));
                    break;
                }
            }

            let current = LYRIC_OFFSET.load(Ordering::Relaxed);
            let new_offset = current + delta;
            LYRIC_OFFSET.store(new_offset, Ordering::Relaxed);

            set_status_line(Some(format!("LYRICS OFFSET {}", new_offset)));

            true
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_song_selection(
    selection_input: String,
    songs_list: &[api::SongDetails],
    music_dir: &PathBuf,
    yt_client: &api::YTMusic,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    playlist_context: Option<(String, bool)>,
    source_resolutions: &mut SourceResolutionDriver,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = selection_input.trim();
    reset_playback_failures();
    let (idx, is_queue) = if input.to_lowercase().starts_with('q') {
        (input[1..].parse::<usize>().unwrap_or(0), true)
    } else {
        (input.parse::<usize>().unwrap_or(0), false)
    };

    if idx >= 1 && idx <= songs_list.len() {
        let selected = &songs_list[idx - 1];
        // Covers every filename scheme that has shipped, so a track already on disk is not fetched
        // from the network again. Under --lossless only a cached FLAC will do: playing a cached .opus
        // instead silently downgraded the audio the user explicitly asked to be lossless.
        let acceptable: &[&str] = if config().lossless_mode {
            player::LOSSLESS_AUDIO_EXTENSIONS
        } else {
            player::CACHED_AUDIO_EXTENSIONS
        };
        let src = player::find_cached_file(
            music_dir,
            &selected.title,
            &selected.artists,
            Some(&selected.video_id),
            acceptable,
        )
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

        let mut new_track = Track::new(
            selected.title.clone(),
            selected.artists.clone(),
            selected.album.clone(),
            selected.duration.clone(),
            selected.thumbnail_url.clone(),
            Some(selected.video_id.clone()),
            src,
        );
        new_track.playback_context = playlist_context.clone();

        if is_queue {
            // An uncached manual pick enters the queue immediately with an empty source. Resolution
            // happens only when it reaches the head, so cancellation or failure cannot lose the pick
            // and several picks retain the order in which the user made them.
            // Appended, not pushed to the front. The README documents Shift+digit as "add it to the
            // queue", and front-insertion reversed the user's own ordering: queueing result 1 and then
            // result 2 played them 2 first. (`p` still front-inserts, which is correct there — it puts
            // the track you left back so it resumes after the previous one.)
            queue_add_by_user(new_track);
            // current_track, not None: the track may have auto-advanced while the results prompt was
            // open, so re-arm the monitor for whatever is actually playing now.
            refresh_ui(current_track.as_ref());
        } else {
            source_resolutions.cancel_pending();
            if new_track.url.is_empty() {
                if !config().game_mode {
                    let status = if config().lossless_mode {
                        "Trying to fetch lossless"
                    } else {
                        "Fetching from youtube"
                    };
                    set_status_line(Some(status.to_string()));
                }
                start_source_resolution(
                    source_resolutions,
                    yt_client,
                    new_track,
                    SourceRequestAction::PlayNow,
                    false,
                );
            } else {
                commit_play_now(
                    new_track,
                    yt_client,
                    current_track,
                    currently_playing,
                    music_dir,
                );
            }
        }
    } else {
        refresh_ui(None);
    }

    Ok(())
}

fn start_source_resolution(
    source_resolutions: &mut SourceResolutionDriver,
    yt_client: &api::YTMusic,
    track: Track,
    action: SourceRequestAction,
    youtube_only: bool,
) {
    let resolver_track = track.clone();
    let yt = yt_client.clone();
    let try_lossless = config().lossless_mode && !youtube_only;
    source_resolutions.start(track, action, async move {
        resolve_track_source(yt, resolver_track, try_lossless).await
    });
}

async fn resolve_track_source(
    yt_client: api::YTMusic,
    track: Track,
    try_lossless: bool,
) -> Result<String, String> {
    let mut last_error = None;
    if try_lossless {
        let clean_title =
            if_title_contains_non_english_and_other_language_script_return_only_english_part(
                &track.title,
            );
        match fetch_flac_stream_url(&clean_title, &track.artists, &track.duration).await {
            Ok(url) => return Ok(url),
            Err(error) => last_error = Some(error.to_string()),
        }
    }

    let video_id = track
        .video_id
        .as_deref()
        .filter(|video_id| !video_id.trim().is_empty())
        .ok_or_else(|| last_error.unwrap_or_else(|| "Track has no YouTube id".to_string()))?;
    yt_client
        .fetch_stream_url(video_id)
        .await
        .map_err(|error| error.to_string())
}

fn commit_play_now(
    new_track: Track,
    yt_client: &api::YTMusic,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
) -> bool {
    if SHOULD_QUIT.load(Ordering::SeqCst) || shutdown_started() {
        return false;
    }
    if let Some(track) = current_track.as_ref() {
        leave_current_track(track, currently_playing, music_dir);
    }
    ui_common::clear_lyrics();
    *current_track = None;

    if !start_playback(&new_track, music_dir, currently_playing) {
        refresh_ui(Some(&Track::dummy()));
        return false;
    }

    invalidate_autoplay_generation();
    *current_track = Some(new_track.clone());
    // Playing a picked song starts a new mix, so the previous context's autoplay entries are dropped
    // while hand-queued tracks survive.
    if !config().no_autoplay {
        retain_only_user_queued();
        RELATED_SONG_LIST
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    rearm_autoplay(yt_client, &new_track, music_dir);
    refresh_ui(Some(&new_track));
    true
}

/// Read a 1-based selection from a numbered prompt.
///
/// `on_idle` runs whenever no key has arrived for a moment, so playback keeps advancing while the
/// prompt is up. It must redraw the prompt if it repainted over it — see [`recv_or_idle`].
fn read_number_selection(
    rx: &std::sync::mpsc::Receiver<String>,
    max: usize,
    mut on_idle: impl FnMut(),
) -> Option<usize> {
    // Nothing is selectable, so waiting for a keypress would just look like a hang.
    if max == 0 {
        return None;
    }
    let mut buf = String::new();

    loop {
        {
            let msg = recv_or_idle(rx, &mut on_idle)?;
            if msg.len() == 1 && msg.chars().all(|c| c.is_ascii_digit()) {
                buf.push_str(&msg);

                // Fewer than ten options means a single digit can only mean one thing, so act on
                // it immediately. Requiring Enter made "press the number" look like a dead key —
                // and the library pager one screen over already selects on the digit alone, so
                // the identical keystroke behaved differently in two adjacent screens.
                if max < 10 {
                    if let Ok(n) = buf.parse::<usize>()
                        && (1..=max).contains(&n)
                    {
                        return Some(n);
                    }
                    // out of range: ignore it rather than cancelling, and let them try again
                    buf.clear();
                }
            } else if msg == "enter" {
                if buf.is_empty() {
                    return None;
                }

                let n: usize = buf.parse().ok()?;
                return (1..=max).contains(&n).then_some(n);
            } else if msg == "backspace" {
                buf.pop();
            } else if msg == "q" {
                request_quit();
                return None;
            } else {
                // anything else (including "" and REFRESH_UI) cancels the prompt
                return None;
            }
        }
    }
}

/// Block for the next keystroke, running `on_idle` between polls.
///
/// `on_idle` keeps playback advancing while a prompt sits on screen, and is responsible for redrawing
/// the prompt if it repainted over it. Returns None only when the prompt should really be abandoned:
/// a shutdown was requested, or the input thread is gone.
///
/// It deliberately does NOT abandon the prompt just because a track changed. Doing that meant an
/// ordinary track transition silently cancelled whatever the user was choosing from — the search
/// results vanished with the typed digits, and the library browser closed itself.
fn recv_or_idle(
    rx: &std::sync::mpsc::Receiver<String>,
    on_idle: &mut impl FnMut(),
) -> Option<String> {
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(msg) => return Some(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // a signal (or a 'q' swallowed elsewhere) asked us to shut down: hand control back
                // to the main loop rather than sitting here
                if SHOULD_QUIT.load(Ordering::SeqCst) {
                    return None;
                }
                on_idle();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                request_quit();
                return None;
            }
        }
    }
}

/// Read a song selection: a digit buffer confirmed with Enter, or a pre-assembled "q1".."q5" to
/// queue instead of play. See [`read_number_selection`] for the `on_idle` contract.
fn read_song_selection(
    rx: &std::sync::mpsc::Receiver<String>,
    max: usize,
    mut on_idle: impl FnMut(),
) -> Option<String> {
    if max == 0 {
        return None;
    }
    let mut buf = String::new();

    loop {
        let msg = recv_or_idle(rx, &mut on_idle)?;
        let msg = msg.trim();

        // Bare 'q' quits (as documented). Queueing from this prompt is Shift+digit, which the input
        // thread already delivers pre-assembled as "q1".."q5" and is handled further down.
        if msg.eq_ignore_ascii_case("q") {
            request_quit();
            return None;
        }

        if msg.len() == 1 && msg.chars().all(|c| c.is_ascii_digit()) {
            buf.push_str(msg);

            // See read_number_selection: with under ten results the digit is unambiguous, so select
            // on it rather than waiting for an Enter the prompt never asked for.
            if max < 10 {
                if let Ok(n) = buf.parse::<usize>()
                    && (1..=max).contains(&n)
                {
                    return Some(buf);
                }
                buf.clear();
            }
            continue;
        }

        if msg == "backspace" {
            buf.pop();
            continue;
        }

        if msg == "enter" {
            let n = buf.parse::<usize>().ok()?;
            return (1..=max).contains(&n).then_some(buf);
        }

        if let Some(rest) = msg.strip_prefix('q').or_else(|| msg.strip_prefix('Q')) {
            let n = rest.parse::<usize>().ok()?;
            return (1..=max).contains(&n).then(|| msg.to_string());
        }

        if let Ok(n) = msg.parse::<usize>() {
            return (1..=max).contains(&n).then(|| msg.to_string());
        }

        if msg.is_empty() || msg == "REFRESH_UI" {
            return None;
        }

        return None;
    }
}

fn start_playback(
    track: &Track,
    music_dir: &std::path::Path,
    currently_playing: &mut Option<Child>,
) -> bool {
    match player::play_file(&track.url, track, music_dir) {
        Ok(child) => {
            *currently_playing = Some(child);
            true
        }
        Err(e) => {
            crate::IS_PLAYING.store(false, Ordering::SeqCst);
            *currently_playing = None;
            note_playback_failed();
            // The prefix alone ate 25 of the ~31 columns, so the actual reason was always cut off.
            // mpv's own diagnostics go to the per-track log that last_playback_error reads.
            let _ = e;
            set_status_line(Some("Could not start playback".to_string()));
            false
        }
    }
}

fn preserve_failed_queue_request(
    source_resolutions: &mut SourceResolutionDriver,
    action: &SourceRequestAction,
) {
    if let SourceRequestAction::QueueHead { queued } = action {
        source_resolutions.mark_queue_failed(queued.clone());
    }
}

fn poll_source_resolution(
    source_resolutions: &mut SourceResolutionDriver,
    yt_client: &api::YTMusic,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
) -> bool {
    if SHOULD_QUIT.load(Ordering::SeqCst) || shutdown_started() {
        source_resolutions.cancel_pending();
        return false;
    }
    let Some((mut pending, result)) = source_resolutions.take_completion() else {
        return false;
    };

    let url = match result {
        Ok(url) => url,
        Err(error) => {
            preserve_failed_queue_request(source_resolutions, &pending.action);
            set_status_line(Some(error));
            return false;
        }
    };
    pending.track.url = url;

    match pending.action {
        SourceRequestAction::PlayNow => commit_play_now(
            pending.track,
            yt_client,
            current_track,
            currently_playing,
            music_dir,
        ),
        SourceRequestAction::QueueHead { queued } => {
            let started = commit_queued_track(
                pending.track,
                &queued,
                yt_client,
                current_track,
                currently_playing,
                music_dir,
            );
            if !started && queue_front_matches(&queued) {
                source_resolutions.mark_queue_failed(queued);
            }
            started
        }
        SourceRequestAction::Previous { current, history } => commit_previous_track(
            pending.track,
            &current,
            &history,
            yt_client,
            current_track,
            currently_playing,
            music_dir,
        ),
    }
}

/// One pass of the playback state machine: reap a finished mpv, honour repeat, and otherwise move
/// on to the next queued track.
///
/// This lives in a function rather than inline in the main loop because any prompt that blocks on
/// the input channel also blocks track advancement — leaving the player silent, with a stale queue,
/// for as long as the prompt is open. Long-lived prompts call this while they wait.
///
/// Returns true if it repainted the screen, so a caller that had drawn its own listing over the UI
/// knows to redraw it.
fn poll_playback(
    yt_client: &api::YTMusic,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
    source_resolutions: &mut SourceResolutionDriver,
) -> bool {
    let source_repainted = poll_source_resolution(
        source_resolutions,
        yt_client,
        current_track,
        currently_playing,
        music_dir,
    );
    // Set when mpv rejected the stream rather than playing it through; reported to the user further
    // down, once the repaint that would erase the message has already happened.
    let mut stream_rejected = false;
    let mut playback_error = None;

    if let Some(child) = currently_playing.as_mut() {
        // Reaping previous song instance thus mutable reference needed (also to check if finished
        // naturally). Ok(None) is the only "still playing" answer; an Err means we can no longer
        // observe the child at all, so treat it as finished rather than waiting on it forever.
        let exit_status = match child.try_wait() {
            Ok(None) => return source_repainted,
            Ok(Some(status)) => Some(status),
            Err(_) => {
                // We can no longer observe mpv. Dropping the handle (at `*currently_playing = None`
                // below) neither kills nor reaps it, so an mpv that is somehow still alive would keep
                // playing with no handle left to stop it. Stop it explicitly before treating the
                // track as finished.
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        };

        // NOTE: lyrics are cleared by advance_to_next_queued, i.e. only when the track actually
        // changes. Clearing them here dropped them for a repeat too, and since the lyric track key
        // was unchanged nothing refetched them — every repeat played without lyrics.

        let heard_audio = player::heard_any_audio();
        let exited_successfully = exit_status.map(|status| status.success()).unwrap_or(false);
        let playback_succeeded = heard_audio || exited_successfully;

        if let Some(track) = current_track.as_ref() {
            if playback_succeeded {
                add_to_history(track.clone());
            }
            // mpv has exited, so the copy it recorded while playing is closed and complete. Keep it
            // if enough of the track was heard.
            player::finish_recording_async(track, music_dir);
        }

        // mpv exiting almost immediately, with a failure code, means the stream was refused — an
        // expired or region-blocked URL, or a 403 from googlevideo. That is not a track "finishing",
        // but it was handled as one: the queue advanced and the song looked like it had been skipped
        // for no reason, with mpv's own diagnostics going to /dev/null.
        // Keyed on whether mpv ever reported a playback position, not on elapsed wall-clock: a
        // refused stream can take several seconds to give up (mpv retries via its ytdl hook first),
        // so a short-elapsed-time test classified those as a normal finish and skipped them silently.
        stream_rejected = exit_status.map(|s| !s.success()).unwrap_or(false) && !heard_audio;
        playback_error = if stream_rejected {
            player::last_playback_error(music_dir)
        } else {
            None
        };

        // Feed the circuit breaker. "Did mpv ever produce audio" is the honest test of whether the
        // track played: a stream that was refused never does, however long mpv spent retrying.
        if playback_succeeded {
            reset_playback_failures();
        } else {
            note_playback_failed();
        }

        *currently_playing = None;

        // -------------------------------------------------------------------
        // CASE 1 : REPEAT THE TRACK THAT JUST ENDED
        // -------------------------------------------------------------------
        let should_repeat = REPEAT_MODE.load(Ordering::Relaxed);

        if should_repeat > 0
            && let Some(track) = current_track.as_ref()
        {
            // Repeating a track that never actually played would respawn mpv every loop pass
            // indefinitely.
            if !playback_succeeded {
                REPEAT_MODE.store(0, Ordering::Relaxed);
                set_status_line(Some("Repeat off: track wouldn't play".into()));
            } else if start_playback(track, music_dir, currently_playing) {
                if should_repeat == 1 {
                    REPEAT_MODE.store(0, Ordering::Relaxed);
                }
            } else {
                REPEAT_MODE.store(0, Ordering::Relaxed);
            }
        }
    }

    // -------------------------------------------------------------------
    // CASE 2 : NOT REPEATING -> PLAY THE NEXT QUEUED TRACK
    //
    // Reached both when a track just ended and when we were already idle: completion used to be
    // detected only while a child was alive, so once a track ended with an empty queue there was
    // never a live child again and tracks the background autoplay task appended a moment later
    // showed up in the queue panel but were never played.
    // -------------------------------------------------------------------
    if currently_playing.is_some() {
        // a repeat restarted the same track; nothing was repainted
        return source_repainted;
    }

    let was_playing = current_track.is_some();
    let autoplay_seed = current_track.clone();

    // Several tracks in a row produced no audio, so something upstream is broken. Continuing to
    // advance re-resolves stream URLs as fast as the poll interval allows — the runaway that caused
    // the rate-limiting in the first place. Stop and wait for the user; any explicit action ('n', 'p',
    // a search, a library pick) clears the count and resumes.
    if playback_is_failing() {
        if was_playing {
            *current_track = None;
            crate::IS_PLAYING.store(false, Ordering::SeqCst);
            refresh_ui(Some(&Track::dummy()));
            set_status_line(Some("Playback failing - stopped".into()));
            return true;
        }
        return source_repainted;
    }

    let repainted = match advance_to_next_queued(
        yt_client,
        current_track,
        currently_playing,
        music_dir,
        source_resolutions,
    ) {
        AdvanceOutcome::Started => true,
        AdvanceOutcome::Pending | AdvanceOutcome::Blocked if was_playing => {
            *current_track = None;
            crate::IS_PLAYING.store(false, Ordering::SeqCst);
            refresh_ui(Some(&Track::dummy()));
            true
        }
        AdvanceOutcome::Pending | AdvanceOutcome::Blocked => source_repainted,
        AdvanceOutcome::Empty if was_playing => {
            // The queue ran dry exactly as a track finished. Seed a new mix from it before letting go,
            // otherwise autoplay ends here for good: nothing else re-arms it, and the idle re-arm above
            // would find an empty queue on every pass forever.
            if let Some(track) = autoplay_seed.as_ref() {
                rearm_autoplay(yt_client, track, music_dir);
            }
            *current_track = None;
            crate::IS_PLAYING.store(false, Ordering::SeqCst);
            refresh_ui(Some(&Track::dummy()));
            true
        }
        AdvanceOutcome::Empty => source_repainted,
    };

    // Set after the repaint above, which would otherwise wipe the status line.
    if stream_rejected {
        // Prefer mpv's own reason over a generic message; it is the difference between "something
        // went wrong" and "YouTube refused the stream".
        let reason = playback_error.unwrap_or_else(|| "Stream unavailable - skipped".to_string());
        set_status_line(Some(reason));
    }

    repainted
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdvanceOutcome {
    Started,
    Pending,
    Blocked,
    Empty,
}

/// Start the queue head, resolving its source in the background first when necessary. The queue item
/// is not removed until mpv has been spawned successfully on the state-machine thread.
fn advance_to_next_queued(
    yt_client: &api::YTMusic,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
    source_resolutions: &mut SourceResolutionDriver,
) -> AdvanceOutcome {
    if source_resolutions.pending.is_some() {
        return AdvanceOutcome::Pending;
    }

    let Some(track) = queue_front() else {
        return AdvanceOutcome::Empty;
    };
    let queued = TrackRequestIdentity::new(&track);
    if source_resolutions.queue_is_blocked(&queued) {
        return AdvanceOutcome::Blocked;
    }

    if queued_track_needs_resolution(&track) {
        let youtube_only = !track.url.trim().is_empty();
        start_source_resolution(
            source_resolutions,
            yt_client,
            track,
            SourceRequestAction::QueueHead { queued },
            youtube_only,
        );
        set_status_line(Some("Resolving queued song".to_string()));
        return AdvanceOutcome::Pending;
    }

    if commit_queued_track(
        track,
        &queued,
        yt_client,
        current_track,
        currently_playing,
        music_dir,
    ) {
        AdvanceOutcome::Started
    } else {
        source_resolutions.mark_queue_failed(queued);
        AdvanceOutcome::Blocked
    }
}

fn commit_queued_track(
    track: Track,
    queued: &TrackRequestIdentity,
    yt_client: &api::YTMusic,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
) -> bool {
    if SHOULD_QUIT.load(Ordering::SeqCst) || shutdown_started() || !queue_front_matches(queued) {
        return false;
    }

    ui_common::clear_lyrics();
    if !start_playback(&track, music_dir, currently_playing) {
        *current_track = None;
        return false;
    }
    if queue_take_front(queued).is_none() {
        player::stop_process(currently_playing, &track.title, music_dir);
        return false;
    }
    *current_track = Some(track.clone());
    rearm_autoplay(yt_client, &track, music_dir);
    refresh_ui(Some(&track));
    true
}

fn commit_previous_track(
    track: Track,
    expected_current: &TrackRequestIdentity,
    expected_history: &TrackRequestIdentity,
    yt_client: &api::YTMusic,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
) -> bool {
    if SHOULD_QUIT.load(Ordering::SeqCst)
        || shutdown_started()
        || !current_track
            .as_ref()
            .is_some_and(|current| expected_current.matches(current))
    {
        return false;
    }
    let Some(history_track) = take_prev_track(expected_history) else {
        return false;
    };
    let leaving = current_track
        .take()
        .expect("current track was checked above");

    ui_common::clear_lyrics();
    player::stop_process(currently_playing, &leaving.title, music_dir);
    // Going backwards must not add the track being left to history; it belongs at the queue head so
    // forward playback resumes it next.
    player::finish_recording_async(&leaving, music_dir);
    queue_add_front(leaving);

    if !start_playback(&track, music_dir, currently_playing) {
        RECENTLY_PLAYED
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(history_track);
        refresh_ui(Some(&Track::dummy()));
        return false;
    }

    *current_track = Some(track.clone());
    rearm_autoplay(yt_client, &track, music_dir);
    refresh_ui(Some(&track));
    set_status_line(Some("PLAYING PREVIOUS".into()));
    true
}

fn youtube_stream_is_expiring(url: &str, now_secs: u64) -> bool {
    if !url.contains("googlevideo.com") {
        return false;
    }
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .query_pairs()
                .find(|(name, _)| name == "expire")
                .and_then(|(_, value)| value.parse::<u64>().ok())
        })
        .map(|expiry| expiry <= now_secs.saturating_add(300))
        .unwrap_or(false)
}

fn queued_track_needs_resolution(track: &Track) -> bool {
    if track.url.trim().is_empty() {
        return true;
    }
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    youtube_stream_is_expiring(&track.url, now_secs)
}

/// Start a fresh autoplay fetch seeded from `track`, superseding any fetch already in flight.
///
/// Every path that wants to cancel pending autoplay must go through here rather than bumping
/// AUTOPLAY_GENERATION on its own. Bumping the generation without arming a replacement kills the
/// pending mix and leaves nothing to refill the queue, which stranded playback silently for the rest
/// of the session — pressing 'p' with an empty history was enough to do it.
fn rearm_autoplay(yt_client: &api::YTMusic, track: &Track, music_dir: &std::path::Path) {
    if config().no_autoplay {
        return;
    }

    // Nothing is playing successfully, so resolving five more stream URLs would only add to the
    // request storm that is causing the failures.
    if playback_is_failing() {
        return;
    }

    if config().offline_mode {
        let mut exclude = get_excluded_track_keys();
        // The song playing right now is in neither the queue (it was popped off) nor the history (it
        // is only added when it ends), so without this the picker could immediately queue up the very
        // track the user is listening to.
        exclude.push(track_identity::track_key(track));

        let mut q = SONG_QUEUE.write().unwrap_or_else(|e| e.into_inner());
        offline::populate_queue_offline(music_dir, &mut q, &exclude);
        return;
    }

    let generation = {
        let _commit = AUTOPLAY_COMMIT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        RELATED_SONG_LIST
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        AUTOPLAY_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
    };

    if let Some(vid) = &track.video_id {
        let yt = yt_client.clone();
        let v = vid.clone();
        let playback_context = track.playback_context.clone();
        tokio::spawn(async move {
            let mut retry_delay = Duration::from_secs(2);
            loop {
                if queue_auto_add_online(
                    yt.clone(),
                    v.clone(),
                    generation,
                    playback_context.clone(),
                )
                .await
                    || !is_current_autoplay_generation(generation)
                {
                    break;
                }
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
            }
        });
    }
}

/// Stop playing `track`: bank it in history, kill mpv, then keep the copy mpv recorded if enough of
/// the track was actually heard.
///
/// Every "the user moved off this track" path goes through here so they cannot disagree about whether
/// a nearly-finished song gets cached. Order matters: mpv has to be stopped before the recording is
/// finalised, or ffmpeg would read a file still being written.
fn leave_current_track(
    track: &Track,
    currently_playing: &mut Option<Child>,
    music_dir: &std::path::PathBuf,
) {
    if player::heard_any_audio() {
        add_to_history(track.clone());
    }
    player::stop_process(currently_playing, &track.title, music_dir);
    player::finish_recording_async(track, music_dir);
}

fn is_current_autoplay_generation(generation: u64) -> bool {
    AUTOPLAY_GENERATION.load(Ordering::SeqCst) == generation
}

fn invalidate_autoplay_generation() {
    let _commit = AUTOPLAY_COMMIT_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    AUTOPLAY_GENERATION.fetch_add(1, Ordering::SeqCst);
}

async fn handle_library_browsing(
    rx: &std::sync::mpsc::Receiver<String>,
    yt_client: &api::YTMusic,
    music_dir: &std::path::PathBuf,
    current_track: &mut Option<Track>,
    currently_playing: &mut Option<Child>,
    source_resolutions: &mut SourceResolutionDriver,
) -> Result<(), Box<dyn std::error::Error>> {
    set_status_line(Some("Fetching Library...".to_string()));

    let playlists = {
        let fetch = {
            let yt = yt_client.clone();
            tokio::spawn(async move {
                yt.fetch_library_playlists()
                    .await
                    .map_err(|e| e.to_string())
            })
        };
        match await_while_pumping_playback(
            fetch,
            yt_client,
            current_track,
            currently_playing,
            music_dir,
            source_resolutions,
        )
        .await
        {
            Some(Ok(playlists)) => playlists,
            Some(Err(error)) => return Err(error.into()),
            None => return Ok(()), // shutting down
        }
    };
    let selection = {
        // The playlist list owns the screen; appended under the banner it scrolled away.
        let Some(screen) = PromptScreen::try_enter() else {
            return Ok(());
        };
        with_output_lock(|| {
            show_playlists(&playlists);
        });

        let selection = read_number_selection(rx, playlists.len(), || {
            poll_playback(
                yt_client,
                current_track,
                currently_playing,
                music_dir,
                source_resolutions,
            );
        });
        if SHOULD_QUIT.load(Ordering::SeqCst) {
            return Ok(());
        }
        screen.finish(|| {
            clear_screen();
            refresh_ui(current_track.as_ref());
        });
        selection
    };
    let sel = match selection {
        Some(n) => n,
        None => return Ok(()),
    };

    let selected_playlist = &playlists[sel - 1];
    set_status_line(Some(format!("Loading '{}'...", selected_playlist.title)));

    let (initial_songs, mut continuation_token) = {
        let fetch = {
            let yt = yt_client.clone();
            let playlist_id = selected_playlist.playlist_id.clone();
            tokio::spawn(async move {
                yt.fetch_playlist_songs(&playlist_id, 100)
                    .await
                    .map_err(|e| e.to_string())
            })
        };
        match await_while_pumping_playback(
            fetch,
            yt_client,
            current_track,
            currently_playing,
            music_dir,
            source_resolutions,
        )
        .await
        {
            Some(Ok(result)) => result,
            Some(Err(error)) => return Err(error.into()),
            None => return Ok(()), // shutting down
        }
    };
    *LIBRARY_SONG_LIST.write().unwrap_or_else(|e| e.into_inner()) = initial_songs;

    let mut page: usize = 1;
    const PAGE_SIZE: usize = 5;

    // The pager owns the screen only while it is *listing*. The chosen song is played after the guard
    // is released, because refresh_ui is suppressed while a chooser is up: starting playback inside
    // this scope meant the banner never repainted for the new track, so the monitor kept drawing the
    // old one and the UI sat on "Nothing Playing" while the song was audibly playing.
    let chosen: Option<(SongDetails, bool)> = {
        let Some(screen) = PromptScreen::try_enter() else {
            return Ok(());
        };

        let chosen = 'pager: loop {
            let list_len = LIBRARY_SONG_LIST
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .len();
            let start = (page - 1) * PAGE_SIZE;
            let end = std::cmp::min(start + PAGE_SIZE, list_len);

            let slice = if start < list_len {
                LIBRARY_SONG_LIST.read().unwrap_or_else(|e| e.into_inner())[start..end].to_vec()
            } else {
                Vec::new()
            };
            with_output_lock(|| {
                ui_common::show_pager_page(
                    format!(
                        " [n]ext | [p]rev | [s]huffle    (page {}, {} loaded)",
                        page, list_len
                    ),
                    &slice,
                    start >= list_len,
                );
            });

            // Keep the player running while the pager waits. A plain rx.recv() here blocked track
            // advancement for as long as the browser was open, so the current song would finish and
            // the player would just go silent with a full queue until the user closed the browser.
            let input = loop {
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(i) => break i,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // a signal asked us to shut down; let the main loop do the teardown
                        if SHOULD_QUIT.load(Ordering::SeqCst) {
                            break 'pager None;
                        }
                        if poll_playback(
                            yt_client,
                            current_track,
                            currently_playing,
                            music_dir,
                            source_resolutions,
                        ) {
                            // advancing repainted the banner over our listing, so redraw it
                            continue 'pager;
                        }
                    }
                    // input thread is gone: returning to the main loop is the only sane move, and
                    // looping here would spin at 100% CPU on an endless stream of errors
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        request_quit();
                        break 'pager None;
                    }
                }
            };
            match input.trim() {
                "q" => {
                    request_quit();
                    break 'pager None;
                }
                "n" => {
                    if page * PAGE_SIZE >= list_len {
                        if let Some(token) = continuation_token.take() {
                            set_status_line(Some("Fetching more...".into()));

                            let fetch = {
                                let yt = yt_client.clone();
                                let token = token.clone();
                                tokio::spawn(async move {
                                    yt.fetch_continuation(&token)
                                        .await
                                        .map_err(|e| e.to_string())
                                })
                            };
                            match await_while_pumping_playback(
                                fetch,
                                yt_client,
                                current_track,
                                currently_playing,
                                music_dir,
                                source_resolutions,
                            )
                            .await
                            {
                                Some(Ok((new_songs, next_token))) => {
                                    LIBRARY_SONG_LIST
                                        .write()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .extend(new_songs);
                                    continuation_token = next_token;
                                    page += 1;

                                    // Swallow keys buffered while the fetch was in flight so spamming
                                    // 'n' does not skip pages — but never swallow a quit, which used to
                                    // be dropped here and simply ignored.
                                    while let Ok(buffered) = rx.try_recv() {
                                        if buffered.trim().eq_ignore_ascii_case("q") {
                                            request_quit();
                                            break;
                                        }
                                    }
                                    if SHOULD_QUIT.load(Ordering::SeqCst) {
                                        break 'pager None;
                                    }
                                }
                                Some(Err(e)) => {
                                    set_status_line(Some(format!("Error: {}", e)));
                                    continuation_token = Some(token);
                                }
                                // Shutting down mid-fetch: abandon the pager (the whole browser is
                                // torn down, so the unused token is simply dropped).
                                None => break 'pager None,
                            }
                        } else {
                            set_status_line(Some("No more songs.".into()));
                        }
                    } else {
                        page += 1;
                    }
                }
                "p" => {
                    if page > 1 {
                        page -= 1;
                    }
                }
                "s" => {
                    let song_to_play = {
                        let list = LIBRARY_SONG_LIST.read().unwrap_or_else(|e| e.into_inner());
                        list.choose(&mut rand::rng()).cloned()
                    };

                    if let Some(song) = song_to_play {
                        break 'pager Some((song, true));
                    }
                }
                "" => break 'pager None,

                num_str => {
                    if let Ok(num) = num_str.parse::<usize>() {
                        if num == 0 || num > PAGE_SIZE {
                            continue;
                        }
                        let song_idx = (page - 1) * PAGE_SIZE + (num - 1);
                        let song = LIBRARY_SONG_LIST
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(song_idx)
                            .cloned();

                        if let Some(s) = song {
                            break 'pager Some((s, false));
                        }
                    }
                }
            }
        };
        if SHOULD_QUIT.load(Ordering::SeqCst) {
            return Ok(());
        }
        screen.finish(|| {
            clear_screen();
            refresh_ui(current_track.as_ref());
        });
        chosen
    };

    // Guard released, so refresh_ui works again and the banner will repaint for the new track.
    if let Some((song, shuffle)) = chosen {
        handle_song_selection(
            "1".into(),
            &[song],
            music_dir,
            yt_client,
            current_track,
            currently_playing,
            Some((selected_playlist.playlist_id.clone(), shuffle)),
            source_resolutions,
        )?;
    }

    Ok(())
}
// -------------------------------------------------------------------
// QUEUE & MPV IPC & PLAYBACK
// -------------------------------------------------------------------

/// Re-point the progress/lyrics monitor at `track` without repainting the banner, dispatching to the
/// current UI mode's draw callback. Used from `refresh_ui` while a chooser owns the screen, so the
/// now-playing line follows an auto-advance instead of freezing on the previous song.
fn rearm_monitor_only(track: &Track) {
    match UI_MODE.load(Ordering::Relaxed) {
        0 => ui1::rearm_monitor(track),
        1 => ui2::rearm_monitor(track),
        _ => ui3::rearm_monitor(track),
    }
}

fn restart_monitor_for_current_view(track: &Track) {
    match UI_MODE.load(Ordering::Relaxed) {
        0 => ui1::restart_monitor_for_view(track),
        1 => ui2::restart_monitor_for_view(track),
        _ => ui3::restart_monitor_for_view(track),
    }
}

fn refresh_ui(track_details: Option<&Track>) {
    if shutdown_started() {
        return;
    }

    // A chooser owns the screen; painting the banner now would overwrite the list the user is
    // reading. It gets drawn again as soon as the chooser closes.
    if PROMPT_ACTIVE.load(Ordering::SeqCst) {
        // But still re-arm the now-playing monitor if a track was given. A track can auto-advance
        // (via a prompt's on_idle poll_playback) while the chooser is open; without this the monitor
        // keeps drawing the PREVIOUS song's title against the new song's advancing progress until the
        // prompt closes. This only re-points the monitor — the banner/queue are left untouched, and
        // the monitor draws the same now-playing rows it already occupied, so it cannot land on the
        // chooser's rows that it wasn't already using.
        if let Some(track) = track_details {
            rearm_monitor_only(track);
        }
        return;
    }

    let _g = OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if shutdown_started() {
        return;
    }

    let mode = VIEW_MODE.read().unwrap_or_else(|e| e.into_inner()).clone();
    let ui_mode = UI_MODE.load(Ordering::Relaxed);
    // prepare data to send
    let titles: Vec<String> = if mode == "recent" {
        let h = RECENTLY_PLAYED.read().unwrap_or_else(|e| e.into_inner());
        h.iter().map(|t| t.title.clone()).rev().collect()
    } else {
        let q = SONG_QUEUE.read().unwrap_or_else(|e| e.into_inner());
        q.iter().map(|t| t.title.clone()).collect()
    };

    if ui_mode == 0 {
        ui1::load_banner(track_details, &titles, &mode);
    } else if ui_mode == 1 {
        ui2::load_banner(track_details, &titles, &mode);
    } else if ui_mode == 2 {
        ui3::load_banner(track_details, &titles, &mode);
    }

    // The banner art covers the status row, so put the status line back. Without this, any message
    // set before a repaint was wiped out and never redrawn.
    ui_common::repaint_status_line_locked();
}

fn add_to_history(track: Track) {
    let mut list = RECENTLY_PLAYED.write().unwrap_or_else(|e| e.into_inner());
    if let Some(last) = list.back()
        && track_identity::track_key(last) == track_identity::track_key(&track)
    {
        return;
    }
    if list.len() >= HISTORY_LIMIT {
        list.pop_front();
    }
    list.push_back(track);
}

fn peek_prev_track() -> Option<Track> {
    RECENTLY_PLAYED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .back()
        .cloned()
}

fn take_prev_track(expected: &TrackRequestIdentity) -> Option<Track> {
    let mut list = RECENTLY_PLAYED.write().unwrap_or_else(|e| e.into_inner());
    list.back()
        .is_some_and(|track| expected.matches(track))
        .then(|| list.pop_back())
        .flatten()
}

fn queue_add(track: Track) {
    let mut q = SONG_QUEUE.write().unwrap_or_else(|e| e.into_inner());
    q.push(track);
}

/// Append a track the user chose explicitly, and remember that they did.
fn queue_add_by_user(track: Track) {
    USER_QUEUED_KEYS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push(track_identity::track_key(&track));
    queue_add(track);
}

/// Drop the autoplay mix but keep anything the user queued by hand, in order.
fn retain_only_user_queued() {
    let mut keys = USER_QUEUED_KEYS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut q = SONG_QUEUE.write().unwrap_or_else(|e| e.into_inner());
    let mut kept_keys = Vec::new();
    q.retain(|track| {
        let key = track_identity::track_key(track);
        if let Some(index) = keys.iter().position(|candidate| candidate == &key) {
            keys.remove(index);
            kept_keys.push(key);
            true
        } else {
            false
        }
    });
    drop(q);
    *USER_QUEUED_KEYS.write().unwrap_or_else(|e| e.into_inner()) = kept_keys;
}

fn queue_add_front(track: Track) {
    let mut q = SONG_QUEUE.write().unwrap_or_else(|e| e.into_inner());
    q.insert(0, track);
}

fn queue_front() -> Option<Track> {
    SONG_QUEUE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .first()
        .cloned()
}

fn queue_front_matches(expected: &TrackRequestIdentity) -> bool {
    SONG_QUEUE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .first()
        .is_some_and(|track| expected.matches(track))
}

fn queue_take_front(expected: &TrackRequestIdentity) -> Option<Track> {
    let next = {
        let mut queue = SONG_QUEUE.write().unwrap_or_else(|e| e.into_inner());
        if queue.first().is_some_and(|track| expected.matches(track)) {
            Some(queue.remove(0))
        } else {
            None
        }
    };
    if let Some(track) = next.as_ref() {
        forget_user_queued(track);
    }
    next
}

/// Stop tracking a hand-queued key once that track has left the queue.
fn forget_user_queued(track: &Track) {
    let key = track_identity::track_key(track);
    let mut keys = USER_QUEUED_KEYS.write().unwrap_or_else(|e| e.into_inner());
    if let Some(index) = keys.iter().position(|candidate| candidate == &key) {
        keys.remove(index);
    }
}

fn queue_next() -> Option<Track> {
    // Bound in its own statement so the queue guard is released before forget_user_queued takes the
    // key list — the two are separate locks and nesting them invites an ordering bug later.
    let next = {
        let mut q = SONG_QUEUE.write().unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            None
        } else {
            Some(q.remove(0))
        }
    };
    if let Some(track) = next.as_ref() {
        forget_user_queued(track);
    }
    next
}

fn if_title_contains_non_english_and_other_language_script_return_only_english_part(
    title: &str,
) -> String {
    let ascii_only: String = title.chars().filter(|c| c.is_ascii()).collect();
    let cleaned = ascii_only
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ");
    if cleaned.is_empty() {
        title.trim().to_string()
    } else {
        cleaned
    }
}

pub async fn queue_auto_add_online(
    yt: api::YTMusic,
    id: String,
    generation: u64,
    playback_context: Option<(String, bool)>,
) -> bool {
    let needs_songs = {
        let q = SONG_QUEUE.read().unwrap_or_else(|e| e.into_inner());
        q.len() < 2
    };

    if needs_songs {
        let mut confirmed_empty = false;
        // check if this task is still the current generation
        if !is_current_autoplay_generation(generation) {
            return true;
        }

        // check if saved related songs are exhausted
        let cache_empty = {
            let c = RELATED_SONG_LIST.read().unwrap_or_else(|e| e.into_inner());
            c.is_empty()
        };

        if cache_empty {
            let (playlist_id, should_suffle) = match &playback_context {
                Some((playlist_id, shuffle_state)) => (Some(playlist_id.clone()), *shuffle_state),
                None => (None, false),
            };

            let related = match yt
                .fetch_related_songs(&id, playlist_id.as_deref(), 50, should_suffle)
                .await
            {
                Ok(related) => related,
                Err(_) => return false,
            };
            confirmed_empty = related.is_empty();
            let _commit = AUTOPLAY_COMMIT_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !is_current_autoplay_generation(generation) {
                return true;
            }
            let mut c = RELATED_SONG_LIST.write().unwrap_or_else(|e| e.into_inner());
            for song in related {
                c.push(song);
            }
        }

        // Resolve stream URLs and add to queue.
        //
        // Taken one at a time. The cache lock cannot be held across the awaits below, so this used to
        // move five entries into a local Vec up front — and a generation bump partway through then
        // discarded every entry still in hand. They were neither queued nor back in the cache, so the
        // next autoplay went to the network again for songs already fetched. Now at most the single
        // in-flight entry is lost.
        let mut queued = 0usize;

        for _ in 0..AUTOPLAY_PREFETCH_DEPTH {
            let next = {
                let _commit = AUTOPLAY_COMMIT_LOCK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !is_current_autoplay_generation(generation) {
                    return true;
                }
                let mut c = RELATED_SONG_LIST.write().unwrap_or_else(|e| e.into_inner());
                if c.is_empty() {
                    None
                } else {
                    Some(c.remove(0))
                }
            };
            let Some(details) = next else { break };

            let mut final_url = None;

            // Check lossless if enabled
            if config().lossless_mode {
                let clean_title =
                    if_title_contains_non_english_and_other_language_script_return_only_english_part(
                        &details.title,
                    );
                final_url =
                    fetch_flac_stream_url(&clean_title, &details.artists, &details.duration)
                        .await
                        .ok();
            }

            // Check generation after stream resolution
            if !is_current_autoplay_generation(generation) {
                return true;
            }

            if final_url.is_none() {
                final_url = yt.fetch_stream_url(&details.video_id).await.ok();
            }

            // Check generation again after stream resolution
            if !is_current_autoplay_generation(generation) {
                return true;
            }

            if let Some(url) = final_url {
                let _commit = AUTOPLAY_COMMIT_LOCK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !is_current_autoplay_generation(generation) {
                    return true;
                }

                let mut track = Track::new(
                    details.title,
                    details.artists,
                    details.album,
                    details.duration,
                    details.thumbnail_url,
                    Some(details.video_id),
                    url,
                );
                track.playback_context = playback_context.clone();
                queue_add(track);
                queued += 1;
            }
        }

        // Check generation before final UI refresh
        if is_current_autoplay_generation(generation) {
            refresh_ui(None);
            // Don't claim success when nothing was queued — that made a failed autoplay fetch look
            // identical to a working one.
            if queued > 0 {
                set_status_line(Some(format!("Fetched {} Similar Songs!", queued)));
            } else if confirmed_empty {
                set_status_line(Some("No similar songs found".to_string()));
            }
        }
        queued > 0 || confirmed_empty
    } else {
        true
    }
}

pub fn config() -> &'static AppConfig {
    CONFIG.get().expect("Config is not initialized")
}

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use std::io::{self, Write};
use std::sync::mpsc::Sender;
pub fn spawn_input_handler(tx: Sender<String>) {
    std::thread::spawn(move || {
        // Serialise raw-mode entry with shutdown so this thread cannot re-enable it after restore.
        with_output_lock(|| {
            let _ = crossterm::terminal::enable_raw_mode();
        });

        loop {
            if shutdown_started() {
                return;
            }
            if let Ok(true) = event::poll(std::time::Duration::from_millis(100))
                && let Ok(ev) = event::read()
            {
                match ev {
                    // Modified keypresses are handled FIRST and never fall through to the plain
                    // bindings below. crossterm reports Ctrl+<letter> as the bare letter plus a
                    // CONTROL modifier, and raw mode turns ISIG off so Ctrl+C is delivered here
                    // rather than as a signal — so matching on key.code alone ran the unmodified
                    // command: Ctrl+C wiped the queue, Ctrl+L liked the track on the user's real
                    // account, Ctrl+Q quit, and Ctrl+\ .. Ctrl+_ injected digits into prompts.
                    Event::Key(key)
                        if key.kind == KeyEventKind::Press
                            && key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        // Ctrl+C / Ctrl+D: the universal "stop this program" reflex.
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
                        {
                            let _ = tx.send("q".into());
                        }
                        // anything else modified is deliberately ignored
                    }

                    Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                        KeyCode::Char('/') => {
                            let Some(screen) = PromptScreen::try_enter() else {
                                // A chooser already owns the input channel and screen.
                                continue;
                            };
                            with_output_lock(|| {
                                execute!(io::stdout(), crossterm::cursor::Show).ok();
                            });
                            let mut query = String::new();
                            let prompt = "> ".bright_blue().bold();

                            set_status_line(Some("Search a Song".to_string()));
                            let completion = loop {
                                with_output_lock(|| {
                                    print!("\r\x1b[2K{} {}█", prompt, query);
                                    let _ = io::stdout().flush();
                                });

                                let k = match event::read() {
                                    Ok(Event::Key(k)) => k,
                                    Ok(_) => continue,
                                    Err(_) => break None,
                                };
                                if k.kind != KeyEventKind::Press {
                                    continue;
                                }

                                // Ctrl+C / Ctrl+D cancel the search. Without this they fell
                                // into the Char(c) arm below and typed a literal "c"/"d"
                                // into the query instead of getting the user out.
                                if k.modifiers.contains(KeyModifiers::CONTROL) {
                                    if matches!(k.code, KeyCode::Char('c') | KeyCode::Char('d')) {
                                        break Some(SearchPromptCompletion::Close);
                                    }
                                    // other Ctrl combinations are not text
                                    continue;
                                }

                                match k.code {
                                    KeyCode::Enter => {
                                        if query.is_empty() {
                                            break Some(SearchPromptCompletion::Close);
                                        }
                                        break Some(SearchPromptCompletion::Query(query));
                                    }
                                    KeyCode::Esc => {
                                        break Some(SearchPromptCompletion::Close);
                                    }
                                    KeyCode::Backspace => {
                                        query.pop();
                                    }

                                    KeyCode::Char(c) => query.push(c),
                                    _ => {}
                                }
                            };
                            if let Some(completion) = completion {
                                let token = screen.token();
                                let message = match completion {
                                    SearchPromptCompletion::Query(query) => {
                                        format!("{}{}:{}", SEARCH_QUERY_PREFIX, token, query)
                                    }
                                    SearchPromptCompletion::Close => {
                                        format!("{}{}", SEARCH_CLOSE_PREFIX, token)
                                    }
                                };
                                if tx.send(message).is_ok() {
                                    screen.handoff();
                                }
                            }
                        }

                        KeyCode::Esc => {
                            let _ = tx.send("".to_string());
                            let _ = tx.send("REFRESH_UI".into());
                        }

                        KeyCode::Char(c) if c.is_ascii_digit() => {
                            let _ = tx.send(c.to_string());
                        }

                        KeyCode::Enter => {
                            let _ = tx.send("enter".into());
                        }

                        KeyCode::Backspace => {
                            let _ = tx.send("backspace".into());
                        }

                        KeyCode::Char('L') => {
                            let _ = tx.send("L".into());
                        }
                        KeyCode::Char('l') => {
                            let _ = tx.send("l".into());
                        }
                        KeyCode::Char('a') => {
                            let _ = tx.send("a".into());
                        }
                        KeyCode::Char('u') => {
                            let _ = tx.send("u".into());
                        }
                        // "c" | "clear" is handled in handle_global_commands and documented in
                        // the README, but nothing ever sent it, so the key did nothing.
                        KeyCode::Char('c') => {
                            let _ = tx.send("c".into());
                        }
                        KeyCode::Char(' ') => {
                            let _ = tx.send("pause".into());
                        }
                        KeyCode::Char('p') => {
                            let _ = tx.send("p".into());
                        }
                        KeyCode::Char('n') => {
                            let _ = tx.send("n".into());
                        }
                        KeyCode::Char('v') => {
                            let _ = tx.send("v".into());
                        }
                        KeyCode::Char('t') => {
                            let _ = tx.send("t".into());
                        }
                        KeyCode::Char('r') => {
                            let _ = tx.send("r".into());
                        }
                        KeyCode::Char('R') => {
                            let _ = tx.send("R".into());
                        }
                        // Needed by the library pager's advertised "[s]huffle" option, which
                        // was unreachable (and with it shuffled-playlist autoplay). Harmless
                        // at top level: unrecognised tokens are ignored there.
                        KeyCode::Char('s') => {
                            let _ = tx.send("s".into());
                        }
                        KeyCode::Char('g') => {
                            let _ = tx.send("g".into());
                        }

                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            let _ = tx.send("+".into());
                        }
                        KeyCode::Char('-') => {
                            let _ = tx.send("-".into());
                        }
                        KeyCode::Char('[') => {
                            let _ = tx.send("[".into());
                        }
                        KeyCode::Char(']') => {
                            let _ = tx.send("]".into());
                        }
                        KeyCode::Right => {
                            let _ = tx.send(">5".into());
                        }
                        KeyCode::Left => {
                            let _ = tx.send("<5".into());
                        }

                        // Just report the keypress; the main loop owns teardown (including
                        // leaving raw mode). Returning here used to drop `tx`, which left
                        // every blocking rx.recv() in a nested prompt with a dead channel.
                        KeyCode::Char('q') => {
                            let _ = tx.send("q".into());
                        }
                        KeyCode::Char('!') => {
                            let _ = tx.send("q1".into());
                        }
                        KeyCode::Char('@') => {
                            let _ = tx.send("q2".into());
                        }
                        KeyCode::Char('#') => {
                            let _ = tx.send("q3".into());
                        }
                        KeyCode::Char('$') | KeyCode::Char('€') => {
                            let _ = tx.send("q4".into());
                        }
                        KeyCode::Char('%') => {
                            let _ = tx.send("q5".into());
                        }
                        _ => {}
                    },

                    Event::Resize(_, _) => {
                        clear_screen();
                        let _ = tx.send("REFRESH_UI".into());
                    }

                    _ => {}
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    static PROMPT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn reset_prompt_test_state() {
        let mut ownership = PROMPT_OWNERSHIP.lock().unwrap_or_else(|e| e.into_inner());
        ownership.current_token = 0;
        PROMPT_ACTIVE.store(false, Ordering::SeqCst);
    }

    #[test]
    fn shutdown_gate_suppresses_late_output_callbacks() {
        let shutdown = AtomicBool::new(false);
        let writes = AtomicUsize::new(0);

        assert!(run_if_output_active(&shutdown, || {
            writes.fetch_add(1, Ordering::SeqCst);
        }));
        shutdown.store(true, Ordering::SeqCst);
        assert!(!run_if_output_active(&shutdown, || {
            writes.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    /// Serialises the tests that read or write the process-wide shutdown atomics, so one flipping
    /// them cannot be observed by another running in parallel.
    static SHUTDOWN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn panic_path_sets_the_same_no_new_output_gate_as_normal_shutdown() {
        let _serialise = SHUTDOWN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Save and restore the globals so this cannot leak into the rest of the suite.
        let prev_shutdown = SHUTTING_DOWN.load(Ordering::SeqCst);
        let prev_quit = SHOULD_QUIT.load(Ordering::SeqCst);
        SHUTTING_DOWN.store(false, Ordering::SeqCst);
        SHOULD_QUIT.store(false, Ordering::SeqCst);

        assert!(!shutdown_started());
        signal_panic_shutdown();
        // A renderer keys on SHUTTING_DOWN (via shutdown_started); the panic path must set it, exactly
        // like begin_shutdown, so nothing repaints or re-hides the cursor after the restore.
        assert!(shutdown_started(), "panic path did not gate output");
        assert!(
            SHOULD_QUIT.load(Ordering::SeqCst),
            "panic path did not request quit"
        );

        SHUTTING_DOWN.store(prev_shutdown, Ordering::SeqCst);
        SHOULD_QUIT.store(prev_quit, Ordering::SeqCst);
    }

    #[test]
    fn shutdown_restores_terminal_before_recording_cleanup() {
        let steps = std::cell::RefCell::new(Vec::new());

        run_shutdown_sequence(
            || steps.borrow_mut().push("playback"),
            || steps.borrow_mut().push("monitor"),
            || steps.borrow_mut().push("restore"),
            || steps.borrow_mut().push("recording"),
            || steps.borrow_mut().push("join"),
        );

        assert_eq!(
            steps.into_inner(),
            ["playback", "monitor", "restore", "recording", "join"]
        );
    }

    #[test]
    fn prompt_screen_drop_releases_current_owner() {
        let _serialise = PROMPT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_prompt_test_state();

        let screen = PromptScreen::try_enter().expect("prompt should be available");
        assert!(PROMPT_ACTIVE.load(Ordering::SeqCst));
        drop(screen);

        assert!(!PROMPT_ACTIVE.load(Ordering::SeqCst));
        assert_eq!(
            PROMPT_OWNERSHIP
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .current_token,
            0
        );
    }

    #[test]
    fn prompt_screen_stale_drop_keeps_new_owner_active() {
        let _serialise = PROMPT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_prompt_test_state();

        let old = PromptScreen::enter_replacing_for_test();
        let newer = PromptScreen::enter_replacing_for_test();
        let newer_token = newer.token.expect("new prompt has a token");
        drop(old);

        assert!(PROMPT_ACTIVE.load(Ordering::SeqCst));
        assert_eq!(
            PROMPT_OWNERSHIP
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .current_token,
            newer_token
        );

        drop(newer);
        assert!(!PROMPT_ACTIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn prompt_screen_drop_skips_normal_completion_action() {
        let _serialise = PROMPT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_prompt_test_state();
        let completions = AtomicUsize::new(0);

        let cancelled = PromptScreen::try_enter().expect("prompt should be available");
        drop(cancelled);
        assert_eq!(completions.load(Ordering::SeqCst), 0);

        let completed = PromptScreen::try_enter().expect("prompt should be available");
        assert!(completed.finish(|| {
            completions.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(completions.load(Ordering::SeqCst), 1);
    }

    /// Guards the runaway: the idle path advances every 250ms, so with unplayable tracks it drained
    /// the queue at four a second and re-armed autoplay each time — measured at 80 mpv spawns in 20
    /// seconds, which is what got the client rate-limited into the 403s it was reacting to.
    #[test]
    fn auto_advance_gives_up_after_repeated_failures() {
        reset_playback_failures();
        assert!(!playback_is_failing());

        for i in 1..MAX_CONSECUTIVE_PLAYBACK_FAILURES {
            note_playback_failed();
            assert!(!playback_is_failing(), "gave up early after {i}");
        }
        note_playback_failed();
        assert!(playback_is_failing(), "should have stopped advancing");

        // an explicit user action is what resumes it
        reset_playback_failures();
        assert!(!playback_is_failing());
    }

    fn chan(msgs: &[&str]) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel::<String>();
        for m in msgs {
            tx.send((*m).to_string()).unwrap();
        }
        // keep the sender alive so recv_timeout waits rather than seeing a disconnect
        std::mem::forget(tx);
        rx
    }

    /// The bug the user hit three times: a single digit had to be followed by Enter, so "press the
    /// number" looked like a dead key — while the library pager one screen over selected on the digit.
    #[test]
    fn a_single_digit_selects_immediately_without_enter() {
        let rx = chan(&["2"]);
        assert_eq!(read_number_selection(&rx, 5, || {}), Some(2));

        let rx = chan(&["3"]);
        assert_eq!(read_song_selection(&rx, 5, || {}), Some("3".to_string()));
    }

    #[test]
    fn every_digit_in_range_is_reachable_with_one_keypress() {
        for n in 1..=9usize {
            let rx = chan(&[&n.to_string()]);
            assert_eq!(read_number_selection(&rx, 9, || {}), Some(n), "digit {n}");
        }
    }

    #[test]
    fn an_out_of_range_digit_is_ignored_rather_than_cancelling() {
        // pressing 7 on a 4-item list must not close the chooser; the next valid digit still works
        let rx = chan(&["7", "0", "2"]);
        assert_eq!(read_number_selection(&rx, 4, || {}), Some(2));
    }

    #[test]
    fn ten_or_more_options_still_buffer_and_wait_for_enter() {
        // with 12 items a lone "1" is ambiguous (1 or 1x), so it must wait
        let rx = chan(&["1", "2", "enter"]);
        assert_eq!(read_number_selection(&rx, 12, || {}), Some(12));

        let rx = chan(&["1", "enter"]);
        assert_eq!(read_number_selection(&rx, 12, || {}), Some(1));
    }

    #[test]
    fn backspace_still_corrects_a_multi_digit_entry() {
        let rx = chan(&["1", "9", "backspace", "2", "enter"]);
        assert_eq!(read_number_selection(&rx, 12, || {}), Some(12));
    }

    #[test]
    fn shift_digit_queue_tokens_are_preserved() {
        // the input thread delivers Shift+digit pre-assembled as "q3"
        let rx = chan(&["q3"]);
        assert_eq!(read_song_selection(&rx, 5, || {}), Some("q3".to_string()));
    }

    #[test]
    fn a_chooser_with_nothing_to_pick_does_not_wait() {
        let rx = chan(&[]);
        assert_eq!(read_number_selection(&rx, 0, || {}), None);
        assert_eq!(read_song_selection(&rx, 0, || {}), None);
    }

    #[test]
    fn only_near_expiry_googlevideo_urls_are_refreshed() {
        let now = 1_000_000;
        assert!(youtube_stream_is_expiring(
            "https://rr.googlevideo.com/videoplayback?expire=1000100&id=x",
            now
        ));
        assert!(!youtube_stream_is_expiring(
            "https://rr.googlevideo.com/videoplayback?expire=1001000&id=x",
            now
        ));
        assert!(!youtube_stream_is_expiring(
            "https://example.com/audio?expire=1",
            now
        ));
    }

    fn source_test_track(video_id: &str, source: &str) -> Track {
        Track::new(
            video_id.to_string(),
            vec!["artist".to_string()],
            String::new(),
            "1:00".to_string(),
            None,
            Some(video_id.to_string()),
            source.to_string(),
        )
    }

    #[tokio::test]
    async fn pending_resolution_does_not_block_ticks_quit_or_ordered_input() {
        let mut resolutions = SourceResolutionDriver::new();
        resolutions.start(
            source_test_track("pending", ""),
            SourceRequestAction::PlayNow,
            std::future::pending::<Result<String, String>>(),
        );

        let (tx, rx) = mpsc::channel();
        tx.send("second".to_string()).unwrap();
        tx.send("q".to_string()).unwrap();
        tx.send("third".to_string()).unwrap();
        drop(tx);
        let mut buffered = VecDeque::from([PendingCommand::now("first".to_string())]);

        // The earlier buffered command is not re-read while pending. New non-quit commands are
        // retained, but quit passes through immediately instead of waiting behind them.
        assert!(resolutions.take_completion().is_none());
        assert_eq!(
            poll_input_while_source_pending(&mut buffered, &rx),
            InputPoll::Buffered
        );
        assert!(resolutions.take_completion().is_none());
        assert_eq!(
            poll_input_while_source_pending(&mut buffered, &rx),
            InputPoll::Ready("q".to_string())
        );
        assert!(resolutions.take_completion().is_none());
        assert_eq!(
            poll_input_while_source_pending(&mut buffered, &rx),
            InputPoll::Buffered
        );
        let commands: Vec<&str> = buffered.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(commands, ["first", "second", "third"]);
        assert_eq!(
            poll_input_while_source_pending(&mut buffered, &rx),
            InputPoll::Disconnected
        );
        assert!(resolutions.pending.is_some());

        resolutions.cancel_pending();
        // These are all navigation/search tokens (not track-relative), so none are dropped on replay.
        let replayed: Vec<String> =
            std::iter::from_fn(|| match poll_main_input(&mut buffered, &rx) {
                InputPoll::Ready(input) => Some(input),
                _ => None,
            })
            .collect();
        assert_eq!(replayed, ["first", "second", "third"]);
    }

    #[test]
    fn a_stale_track_relative_command_is_dropped_not_retargeted() {
        // pause/seek buffered while generation 5 was playing must not fire after the track advanced.
        assert!(buffered_command_is_stale("pause", 5, 6));
        assert!(buffered_command_is_stale(">10", 5, 6));
        assert!(buffered_command_is_stale("<10", 5, 6));
        // ...but they still apply when the same track is current (e.g. a resolution that never
        // replaced it).
        assert!(!buffered_command_is_stale("pause", 5, 5));
        assert!(!buffered_command_is_stale(">10", 5, 5));
        // Track-independent and screen-global commands are never dropped, even across a transition.
        for command in [
            "+", "-", "[", "]", "q", "quit", "n", "p", "v", "L", "3", "q2",
        ] {
            assert!(
                !buffered_command_is_stale(command, 5, 6),
                "wrongly dropped non-track-relative command {command:?}"
            );
        }
    }

    #[test]
    fn poll_main_input_drops_a_stale_buffered_pause() {
        // A "pause" buffered under a generation that no longer matches the live player is skipped
        // (Buffered => the caller loops), never dispatched as Ready.
        let (_tx, rx) = mpsc::channel::<String>();
        let stale = player::current_playback_generation().wrapping_sub(1);
        let mut buffered = VecDeque::from([PendingCommand {
            command: "pause".to_string(),
            playback_generation: stale,
        }]);
        assert_eq!(poll_main_input(&mut buffered, &rx), InputPoll::Buffered);
        assert!(
            buffered.is_empty(),
            "the stale command should be consumed, not left to retry"
        );
        // keep _tx alive so the empty channel does not report Disconnected mid-test
        drop(_tx);
    }

    #[test]
    fn late_a_completion_cannot_commit_after_b_then_a_again() {
        let mut resolutions = SourceResolutionDriver::new();
        let first_a = source_test_track("a", "");
        let b = source_test_track("b", "");
        let second_a = source_test_track("a", "");

        let first_generation =
            resolutions.install_for_test(first_a.clone(), SourceRequestAction::PlayNow);
        let b_generation = resolutions.install_for_test(b.clone(), SourceRequestAction::PlayNow);
        let current_generation =
            resolutions.install_for_test(second_a.clone(), SourceRequestAction::PlayNow);
        assert_ne!(first_generation, current_generation);

        resolutions.complete_for_test(
            first_generation,
            track_identity::track_key(&first_a),
            Ok("stale-a".to_string()),
        );
        resolutions.complete_for_test(
            b_generation,
            track_identity::track_key(&b),
            Ok("stale-b".to_string()),
        );
        resolutions.complete_for_test(
            current_generation,
            track_identity::track_key(&second_a),
            Ok("current-a".to_string()),
        );

        let (request, result) = resolutions
            .take_completion()
            .expect("current A completion should be accepted");
        assert_eq!(request.generation, current_generation);
        assert_eq!(result.unwrap(), "current-a");
        assert!(resolutions.take_completion().is_none());
    }

    /// Both queue tests mutate SONG_QUEUE and USER_QUEUED_KEYS, which are process-wide statics, so
    /// they must not run concurrently — cargo runs tests in parallel by default and they raced.
    static QUEUE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn failed_or_cancelled_queue_resolution_keeps_manual_track_and_key() {
        let _serialise = QUEUE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        SONG_QUEUE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        USER_QUEUED_KEYS
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        let track = source_test_track("manual", "");
        let queued = TrackRequestIdentity::new(&track);
        queue_add_by_user(track.clone());

        let mut resolutions = SourceResolutionDriver::new();
        let generation = resolutions.install_for_test(
            track.clone(),
            SourceRequestAction::QueueHead {
                queued: queued.clone(),
            },
        );
        resolutions.complete_for_test(
            generation,
            track_identity::track_key(&track),
            Err("resolver failed".to_string()),
        );
        let (request, result) = resolutions.take_completion().unwrap();
        assert!(result.is_err());
        preserve_failed_queue_request(&mut resolutions, &request.action);

        assert_eq!(queue_front(), Some(track.clone()));
        assert_eq!(
            USER_QUEUED_KEYS
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_slice(),
            [track_identity::track_key(&track)]
        );
        assert!(resolutions.queue_is_blocked(&queued));

        resolutions.clear_queue_failure();
        resolutions.install_for_test(
            track.clone(),
            SourceRequestAction::QueueHead {
                queued: queued.clone(),
            },
        );
        resolutions.cancel_pending();
        assert_eq!(queue_front(), Some(track.clone()));
        assert_eq!(
            USER_QUEUED_KEYS
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_slice(),
            [track_identity::track_key(&track)]
        );

        let _ = queue_next();
    }

    /// Feed a chooser a canned sequence of input tokens, exactly as spawn_input_handler emits them.
    fn feed(tokens: &[&str]) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel::<String>();
        for t in tokens {
            tx.send((*t).to_string()).unwrap();
        }
        // dropped tx: after the tokens run out the channel disconnects, so a chooser that is still
        // waiting returns None instead of hanging the test
        rx
    }

    /// The bug the user hit three times: pressing the number did nothing because the chooser was
    /// waiting for an Enter the prompt never mentioned.

    #[test]
    fn ten_or_more_options_still_buffer_for_multi_digit_entry() {
        // with 12 options "1" is ambiguous (1 or 1x), so it must wait for Enter
        assert_eq!(
            read_number_selection(&feed(&["1", "2", "enter"]), 12, || {}),
            Some(12)
        );
        assert_eq!(
            read_number_selection(&feed(&["4", "enter"]), 12, || {}),
            Some(4)
        );
        // backspace still corrects a buffered entry
        assert_eq!(
            read_number_selection(&feed(&["1", "9", "backspace", "2", "enter"]), 12, || {}),
            Some(12)
        );
    }

    #[test]
    fn shift_digit_still_queues_from_the_results_prompt() {
        // the input thread delivers Shift+digit pre-assembled as "q1".."q5"
        assert_eq!(
            read_song_selection(&feed(&["q2"]), 5, || {}),
            Some("q2".to_string())
        );
    }

    /// Shift+digit is documented as "add it to the queue", so several picks must keep their order.
    /// Front-insertion made queueing 1 then 2 play 2 first.
    #[test]
    fn queueing_several_songs_keeps_the_order_they_were_picked() {
        let _serialise = QUEUE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fn t(title: &str) -> Track {
            Track::new(
                title.to_string(),
                vec!["A".to_string()],
                String::new(),
                "0:10".to_string(),
                None,
                Some(title.to_string()),
                "http://x".to_string(),
            )
        }

        SONG_QUEUE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        USER_QUEUED_KEYS
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        queue_add(t("first"));
        queue_add(t("second"));
        queue_add(t("third"));

        let order: Vec<String> = SONG_QUEUE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|x| x.title.clone())
            .collect();
        assert_eq!(order, vec!["first", "second", "third"]);

        // 'p' deliberately still front-inserts, so the track you left resumes next
        queue_add_front(t("came_back"));
        assert_eq!(queue_next().map(|x| x.title), Some("came_back".to_string()));

        SONG_QUEUE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Starting a picked song replaces the autoplay mix, but must not throw away tracks the user
    /// queued by hand — that discarded their explicit picks with no indication.
    #[test]
    fn starting_a_new_song_keeps_hand_queued_tracks() {
        let _serialise = QUEUE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fn t(title: &str) -> Track {
            Track::new(
                title.to_string(),
                vec!["A".to_string()],
                String::new(),
                "0:10".to_string(),
                None,
                Some(title.to_string()),
                "http://x".to_string(),
            )
        }

        SONG_QUEUE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        USER_QUEUED_KEYS
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        queue_add(t("mix_a")); // autoplay
        queue_add_by_user(t("my_pick")); // Shift+digit
        queue_add(t("mix_b")); // autoplay

        retain_only_user_queued();

        let left: Vec<String> = SONG_QUEUE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|x| x.title.clone())
            .collect();
        assert_eq!(left, vec!["my_pick"]);

        // and once it has played, it is no longer tracked
        assert_eq!(queue_next().map(|x| x.title), Some("my_pick".to_string()));
        assert!(
            USER_QUEUED_KEYS
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );

        SONG_QUEUE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    #[test]
    fn retaining_manual_queue_entries_respects_duplicate_counts() {
        let _serialise = QUEUE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let track = Track::new(
            "same".to_string(),
            vec!["artist".to_string()],
            String::new(),
            "1:00".to_string(),
            None,
            Some("same-id".to_string()),
            "http://x".to_string(),
        );
        SONG_QUEUE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        USER_QUEUED_KEYS
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        queue_add(track.clone());
        queue_add_by_user(track.clone());
        queue_add(track);
        retain_only_user_queued();

        assert_eq!(
            SONG_QUEUE.read().unwrap_or_else(|e| e.into_inner()).len(),
            1
        );
        assert_eq!(
            USER_QUEUED_KEYS
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1
        );
        let _ = queue_next();
    }

    #[test]
    fn command_exists_finds_tools_that_reject_double_dash_version() {
        // The regression that mattered: ffmpeg exits 8 on `--version`, so probing it that way
        // reported it missing and made `--download` refuse to start.
        assert!(
            command_exists("ffmpeg"),
            "ffmpeg is a documented dependency and is installed, but was not detected"
        );
        assert!(command_exists("ffprobe"));
        assert!(command_exists("mpv"));
    }

    #[test]
    fn command_exists_rejects_a_name_that_is_not_on_path() {
        assert!(!command_exists("whytui-definitely-not-a-real-binary"));
    }

    #[test]
    fn command_exists_handles_an_absolute_path() {
        assert!(command_exists("/usr/bin/env") || command_exists("/bin/env"));
        assert!(!command_exists("/nonexistent/definitely/not/here"));
        // a directory is not an executable
        assert!(!command_exists("/tmp"));
    }
}
