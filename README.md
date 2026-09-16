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

#### Runtime requirements

- `mpv` — required for audio playback.
- `yt-dlp` — required for online YouTube Music stream resolution.
- A **yt-dlp PO token provider** — required for playback to work at all. YouTube now refuses to serve
  the stream URLs of the clients that need no token (`android_vr` resolves fine, then every request
  for the audio returns HTTP 403), while the `web` clients are SABR-only and hand back no direct URL.
  whytui therefore asks yt-dlp for the `tv_simply` client, which needs a PO token but does still yield
  a servable URL. Without a provider installed, yt-dlp cannot produce that token, whytui falls back to
  the default client, and every song fails with "Stream refused by YouTube (403)".

  Install [bgutil-ytdlp-pot-provider](https://github.com/Brainicism/bgutil-ytdlp-pot-provider) — the
  script mode needs no server, just Node or Deno:

  ```bash
  # 1. the yt-dlp plugin
  mkdir -p ~/.config/yt-dlp/plugins/bgutil
  curl -sL -o /tmp/pot.zip https://github.com/Brainicism/bgutil-ytdlp-pot-provider/releases/latest/download/bgutil-ytdlp-pot-provider.zip
  unzip -q -o /tmp/pot.zip -d ~/.config/yt-dlp/plugins/bgutil

  # 2. the token generator it shells out to
  curl -sL https://github.com/Brainicism/bgutil-ytdlp-pot-provider/archive/refs/tags/1.3.2.tar.gz | tar xz -C /tmp
  mkdir -p ~/bgutil-ytdlp-pot-provider
  cp -r /tmp/bgutil-ytdlp-pot-provider-1.3.2/server ~/bgutil-ytdlp-pot-provider/server
  cd ~/bgutil-ytdlp-pot-provider/server && deno install --allow-scripts   # or: npm install && npm run build
  ```

  Check it took with `yt-dlp -v --simulate <any youtube url> 2>&1 | grep "PO Token Providers"` — the
  bgutil entry should not say `unavailable`.
- `ffmpeg` — required for downloading/caching tracks.
- YouTube Music cookies — required for authenticated features such as library, like, and playlist modification.

1. Linux:

```bash
curl -L -o whytui https://github.com/shreyas-sha3/whytui/releases/download/Latest/whytui-linux-x86_64 && chmod +x whytui && sudo mv whytui /usr/local/bin/
````

2. MacOS:
```bash
brew install mpv
```
```bash
curl -L -o whytui https://github.com/shreyas-sha3/whytui/releases/download/Latest/whytui-macos-x86_64 && chmod +x whytui && sudo mv whytui /usr/local/bin/
```

3. Windows:
```bash
winget install mpv
```
```bash
curl.exe -L -o whytui.exe https://github.com/shreyas-sha3/whytui/releases/download/Latest/whytui-win-x86_64.exe && move whytui.exe C:\Windows\
```
[oneliner for cmd as administrator]
 
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
  * `-d`, `--download` — save/cache tracks while playing. A track is kept once you have heard at
    least 90% of it, so skipping near the end still saves it while skipping early does not. The copy
    is taken from the stream as it plays (no second download), tagged with title/artist/album and
    cover art, and dropped into the music dir where offline mode picks it up.
  * `-o`, `--offline` — play only from the local offline library.
  * `-n`, `--nomix` — disable autoplay/auto-queue.
  * `-l`, `--lossless` — attempt lossless stream fetching.
  * `-pl`, `--peak-lossless` — request peak lossless quality where supported.
  * `-g`, `--guess` — enable quality guessing mode.

* Cookies can be placed at `$MusicDir/whytui/config/cookies.txt` in Netscape cookie format.


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
