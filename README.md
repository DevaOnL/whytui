# WhyTUI

A terminal-based YouTube music player written in Rust.  
Search, download, and play songs directly from the terminal.

UI 1             |  UI 2                               | UI 3
:-------------------------:|:-------------------------:|:-------------------------:
<img width="700" height="600" alt="image" src="https://github.com/user-attachments/assets/0e6f7643-ec98-446b-b361-5f3b7a4c77b0" /> |<img width="700" height="700" alt="image" src="https://github.com/user-attachments/assets/88dbb7d5-6c83-4e2e-a6ae-4b79abc90d55" />  | <img width="500" height="600" alt="image" src="https://github.com/user-attachments/assets/6285ef05-387f-4867-9532-216e4cb7347e" />


## Features

- Search for songs using YouTubeMusic
- Play songs using `mpv`
- Caches songs automatically to `~/Music/whytui`
- Auto adds related songs to queue
- Can play directly from Cache
- Minimal dependencies and fast startup

## Installation

### Runtime Dependencies

**Required for all modes:**
- `mpv` — for audio playback on all platforms

**Required for online playback:**
- `yt-dlp` — for resolving YouTube Music stream URLs

**Required for downloading/saving tracks:**
- `ffmpeg` — for audio format conversion and tagging

**For authenticated YouTube Music features:**
- Netscape-format cookies file (see usage notes below)

### Install dependencies

**Linux (Ubuntu/Debian):**
```bash
sudo apt-get install mpv yt-dlp ffmpeg
```

**Linux (Fedora):**
```bash
sudo dnf install mpv yt-dlp ffmpeg
```

**macOS:**
```bash
brew install mpv yt-dlp ffmpeg
```

**Windows (via Winget):**
```bash
winget install vidMob.mpv yt-dlp.yt-dlp ffmpeg
```

### Installation from releases
 
## Usage
- Press `/` and type to search for a song
- Type the song’s number to start playing it
- Hold Shift while typing the number to add it to the queue instead of playing

| Keybind       | Action                                |
|---------------|----------------------------------------|
| `/`           | Search songs                           |
| `SPACEBAR`    | Play/Pause the song                    |
| `n`           | Play next song in the queue            |
| `p`           | Play previous song in the queue        |
| `←`   `→`     | Seek 5 seconds                         |
| `-`   `+`     | Change volume by 5%                    |
| `[`   `]`     | Offset lyric by 100ms                  |
| `c`           | Clear the queue                        |
| `r`           | Toggle recently played                 |
| `R`           | Repeat song (once or ∞)                |
| `v`           | Toggle display modes                   |
| `g`           | Take a guess of the quality            |
| `t`           | Toggle romanize/translate              |
| `L`           | Show YouTube Music libraries           |
| `l`           | Like the currently playing song        |
| `a`           | Add song to playlist                   |
| `q`           | Quit the application                   |


* Arguments:
  * `-d` | `--download` — download/save selected tracks for offline playback (requires ffmpeg)
  * `-o` | `--offline` — play only from the local offline library
  * `-n` | `--nomix` — disable autoplay/mix mode
  * `-l` | `--lossless` — attempt to fetch lossless (FLAC) audio where available
  * `-g` | `--guess` — try guessing current track quality

* Setup:
  * Netscape-format cookies file can be placed at `~/.config/whytui/config/cookies.txt` for authenticated YouTube Music features
  * To export cookies: Use a browser extension like "NetscapeHttpCookieFormat" on sites.google.com or similar
  * Without cookies, the app works in guest/limited mode


## TODO

- [X] pause,seek
- [X] progress bar
- [X] queues
- [X] cache songs/store to disk from memory
- [X] autoplay similar songs
- [X] lyrics
- [ ] reimpliment a fully featured ytmusic api
- [ ] proper tui using ratatui


## CREDITS

- [lyrics](https://lrclib.net)
- [lossless](https://github.com/uimaxbai/hifi-api)
- Inspirations: [spotify-tui](https://github.com/Rigellute/spotify-tui) [kew](https://github.com/ravachol/kew)
