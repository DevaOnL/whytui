use reqwest::{
    Client,
    header::{
        ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER,
        USER_AGENT,
    },
};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::process::{Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;

/// How long to give yt-dlp before killing it. It is resolved on the playback path, so a hung yt-dlp
/// must not be allowed to stall a song start indefinitely.
const YTDLP_TIMEOUT: Duration = Duration::from_secs(30);

fn spawn_resolver_child(
    command: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    command.kill_on_drop(true);
    command.spawn()
}

async fn wait_for_resolver_output(
    mut child: tokio::process::Child,
    timeout: Duration,
) -> std::io::Result<Output> {
    let mut stdout = child.stdout.take();
    let waited = tokio::time::timeout(timeout, async {
        let read_stdout = async move {
            let mut bytes = Vec::new();
            if let Some(pipe) = stdout.as_mut() {
                pipe.read_to_end(&mut bytes).await?;
            }
            Ok::<_, std::io::Error>(bytes)
        };
        let (status, stdout) = tokio::try_join!(child.wait(), read_stdout)?;
        Ok::<_, std::io::Error>((status, stdout))
    })
    .await;

    match waited {
        Ok(Ok((status, stdout))) => Ok(Output {
            status,
            stdout,
            stderr: Vec::new(),
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            // Tokio's kill waits for termination; the extra wait covers the race where the process
            // exited between the deadline firing and the kill request.
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "yt-dlp timed out",
            ))
        }
    }
}

/// Cap for the assembled `Cookie` header.
///
/// Google answers HTTP 413 well before this, and the whole point is to stay far away from that.
const MAX_COOKIE_HEADER_BYTES: usize = 8 * 1024;

/// Whether a cookie from the export is worth sending to the API at all.
///
/// A browser only sends the cookies matching the page it is loading, but a Netscape export contains
/// every cookie the browser has ever accumulated for the domain — and whytui concatenated all of them
/// into one header. `ST-*` are YouTube's per-surface widget-state tokens, each up to ~1.8 KB and none
/// of them used for authentication; one real export had 78 of them totalling 86 KB, producing a 90 KB
/// `Cookie` header. Google replied **HTTP 413**, which surfaced in the UI as an authentication error
/// and made the library unfetchable.
fn is_sendable_cookie(name: &str) -> bool {
    !name.starts_with("ST-")
}

/// Cookies that authentication genuinely depends on, used as the fallback if an export is so large
/// that dropping the obvious noise is not enough.
fn is_essential_cookie(name: &str) -> bool {
    name.starts_with("__Secure-")
        || matches!(
            name,
            "SID" | "HSID" | "SSID" | "APISID" | "SAPISID" | "SIDCC" | "LOGIN_INFO" | "PREF"
        )
}

fn render_cookie_header(jar: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in jar {
        out.push_str(name);
        out.push('=');
        out.push_str(value);
        out.push_str("; ");
    }
    out
}

fn cookie_applies_to_music_youtube(parts: &[&str], now_secs: u64) -> bool {
    if parts.len() < 7 {
        return false;
    }

    let domain = parts[0]
        .trim()
        .trim_start_matches("#HttpOnly_")
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if domain != "music.youtube.com" && domain != "youtube.com" {
        return false;
    }
    let include_subdomains = parts[1].trim().eq_ignore_ascii_case("TRUE");
    if domain == "youtube.com" && !include_subdomains {
        return false;
    }

    // The API request path is `/youtubei/...`; cookies scoped to unrelated browser paths must not be
    // replayed there. Netscape exports use `/` for the authentication cookies.
    let path = parts[2].trim();
    if !path.starts_with('/') || !"/youtubei/".starts_with(path) {
        return false;
    }

    let expiry = parts[4].trim().parse::<u64>().unwrap_or(0);
    expiry == 0 || expiry > now_secs
}

/// Assemble the `Cookie` header, shedding non-essential cookies if it would otherwise be too big.
fn build_cookie_header(jar: &[(String, String)]) -> String {
    let full = render_cookie_header(jar);
    if full.len() <= MAX_COOKIE_HEADER_BYTES {
        return full;
    }

    // Still oversized after dropping the known noise: fall back to just the auth-critical cookies
    // rather than sending a header the server will reject outright.
    let essential: Vec<(String, String)> = jar
        .iter()
        .filter(|(name, _)| is_essential_cookie(name))
        .cloned()
        .collect();
    let mut bounded = Vec::new();
    for cookie in essential {
        let mut candidate = bounded.clone();
        candidate.push(cookie);
        if render_cookie_header(&candidate).len() <= MAX_COOKIE_HEADER_BYTES {
            bounded = candidate;
        }
    }
    render_cookie_header(&bounded)
}

#[derive(Debug, Clone)]
pub struct SongDetails {
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration: String,
    pub thumbnail_url: Option<String>,
    pub video_id: String,
}

#[derive(Debug, Clone)]
pub struct PlaylistDetails {
    pub title: String,
    pub playlist_id: String,
    pub count: String,
    pub continuation_token: Option<String>,
}

#[derive(Clone)]
pub struct YTMusic {
    auth_client: Client,
    /// Unauthenticated client for the native `/player` stream resolution that is still commented
    /// out below; unused while `fetch_stream_url` shells out to yt-dlp instead.
    #[allow(dead_code)]
    guest_client: Client,
    /// Kept so the SAPISIDHASH can be re-derived per request; see `sapisid_hash`.
    sapisid: String,
}

impl YTMusic {
    pub fn new_with_cookies(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        // The cookies file holds auth secrets; refuse to read it if the path is not a regular file
        // (e.g. a symlink swapped in to point us at a foreign file). prepare_cookies_file created it as
        // a 0600 regular file moments earlier; this rejects a race that replaced it since.
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_file() {
            return Err(format!("{} is not a regular file", path).into());
        }
        let file = File::open(path)?;
        let reader = BufReader::new(file);

        let mut jar: Vec<(String, String)> = Vec::new();
        let mut sapisid = String::new();
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty()
                || (trimmed.starts_with('#') && !trimmed.starts_with("#HttpOnly_"))
            {
                continue;
            }

            let parts: Vec<&str> = trimmed.split('\t').collect();
            if cookie_applies_to_music_youtube(&parts, now_secs) {
                let name = parts[5].trim();
                let value = parts[6].trim();

                if name == "SAPISID" {
                    sapisid = value.to_string();
                }
                if is_sendable_cookie(name) {
                    jar.push((name.to_string(), value.to_string()));
                }
            }
        }

        let cookie_string = build_cookie_header(&jar);

        let user_agent = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

        let mut common_headers = HeaderMap::new();
        common_headers.insert(USER_AGENT, HeaderValue::from_static(user_agent));
        common_headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        common_headers.insert(
            "Accept-Language",
            HeaderValue::from_static("en-GB,en;q=0.9"),
        );
        common_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // common_headers.insert(
        //     "Sec-Ch-Ua",
        //     HeaderValue::from_static("\"Not_A Brand\";v=\"99\", \"Chromium\";v=\"142\""),
        // );
        // common_headers.insert("Sec-Ch-Ua-Mobile", HeaderValue::from_static("?0"));
        // common_headers.insert("Sec-Ch-Ua-Platform", HeaderValue::from_static("\"Linux\""));
        common_headers.insert(
            ORIGIN,
            HeaderValue::from_static("https://music.youtube.com"),
        );
        common_headers.insert(
            REFERER,
            HeaderValue::from_static("https://music.youtube.com/"),
        );

        let mut auth_headers = common_headers.clone();
        auth_headers.insert("X-Goog-AuthUser", HeaderValue::from_static("0"));

        auth_headers.insert("X-Youtube-Client-Name", HeaderValue::from_static("67"));
        auth_headers.insert(
            "X-Youtube-Client-Version",
            HeaderValue::from_static("1.20251215.03.00"),
        );

        // NOTE: no AUTHORIZATION header here on purpose — it is timestamped and is attached
        // per request in `post_auth`.

        if !cookie_string.is_empty()
            && let Ok(val) = HeaderValue::from_str(&cookie_string)
        {
            auth_headers.insert(COOKIE, val);
        }

        // a request with no timeout can wedge the whole app: fetch_account_name() is awaited
        // during startup, before the event loop is running
        let auth_client = Client::builder()
            .default_headers(auth_headers)
            .timeout(Duration::from_secs(15))
            .build()?;

        let mut guest_headers = common_headers.clone();

        guest_headers.insert("X-Youtube-Client-Name", HeaderValue::from_static("67"));
        guest_headers.insert(
            "X-Youtube-Client-Version",
            HeaderValue::from_static("1.20251215.03.00"),
        );

        let guest_client = Client::builder()
            .default_headers(guest_headers)
            .timeout(Duration::from_secs(15))
            .build()?;

        Ok(Self {
            auth_client,
            guest_client,
            sapisid,
        })
    }

    /// Build a fresh `SAPISIDHASH` credential.
    ///
    /// The hash commits to the current unix time and YouTube rejects it once that is stale, so it
    /// has to be recomputed for every request. Deriving it once at construction meant every
    /// authenticated call — search included — began failing after the app had been open a while,
    /// surfacing to the user as a permanent "Search failed (retry)".
    fn sapisid_hash(&self) -> Option<String> {
        if self.sapisid.is_empty() {
            return None;
        }
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        let mut hasher = Sha1::new();
        hasher.update(format!(
            "{} {} {}",
            timestamp, self.sapisid, "https://music.youtube.com"
        ));
        Some(format!(
            "SAPISIDHASH {}_{}",
            timestamp,
            hex::encode(hasher.finalize())
        ))
    }

    async fn post_auth(&self, endpoint: &str, body: &Value) -> Result<Value, Box<dyn Error>> {
        let mut req = self.auth_client.post(endpoint).json(body);
        if let Some(hash) = self.sapisid_hash() {
            req = req.header(AUTHORIZATION, hash);
        }
        let res = req.send().await?;

        let status = res.status();
        if !status.is_success() {
            // Kept short and actionable on purpose. This message lands on a ~31-column status line,
            // where the old version — which appended the endpoint and the entire response body —
            // showed the user nothing but "Error in Library: Authentica...". Google's error bodies are
            // multi-KB HTML pages, so none of that was ever readable.
            return Err(match status.as_u16() {
                401 | 403 => "Not signed in: check cookies".into(),
                413 => "Cookies too big: re-export".into(),
                429 => "Rate limited - wait a bit".into(),
                other => format!("YouTube API error {}", other).into(),
            });
        }

        Ok(res.json().await?)
    }

    // pub async fn fetch_stream_url(&self, video_id: &str) -> Result<String, Box<dyn Error>> {
    //     let payload = json!({
    //         "videoId": video_id,
    //         "context": {
    //             "client": {
    //                 "hl": "en",
    //                 "gl": "IN",
    //                 "remoteHost": "123.185.130.321",
    //                 "deviceMake": "",
    //                 "deviceModel": "",
    //                 "visitorData": "CgtYTkZURlh1U0hIdyjrzYrKBjIKCgJJThIEGgAgOw%3D%3D",
    //                 "userAgent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36,gzip(gfe)",
    //                 "clientName": "ANDROID",
    //                 "clientVersion": "19.09.37",
    //                 "osName": "X11",
    //                 "osVersion": "",
    //                 "originalUrl": "https://music.youtube.com/",
    //                 "platform": "DESKTOP",
    //                 "clientFormFactor": "UNKNOWN_FORM_FACTOR",
    //                 "configInfo": {
    //                     "appInstallData": "COvNisoGEMj3zxwQudnOHBDyndAcEL22rgUQlLbQHBCZjbEFEJX3zxwQ0eDPHBDhgoATENr3zhwQlP6wBRDatNAcEKefqRcQg57QHBCNsNAcEMGP0BwQmrnQHBDYltAcEI-50BwQvKTQHBD2q7AFENPhrwUQzN-uBRDKu9AcEK7WzxwQpbbQHBCd0LAFEIzpzxwQooW4IhDxnLAFEIeszhwQzrPQHBCBzc4cEJOD0BwQnNfPHBD8ss4cEMn3rwUQg6zQHBDevM4cELnA0BwQ5ofQHBC8s4ATEMzrzxwQvZmwBRC8v9AcEL2KsAUQlPLPHBDHttAcEKudzxwQ28HQHBC36v4SEKafqRcQ8rPQHBDwtNAcEIiHsAUQu9nOHBCL988cEPCdzxwQltvPHBDlpNAcELjkzhwQ4cGAExCJsM4cEMWM0BwQr7CAEypMQ0FNU014VW8tWnEtRE1lVUVvY09xZ0xNQmJiUDhBc3l2MV9wMVFVRHpmOEZvWUFHb2k3UDFBYjBMX1lQN3pDVzNRV1R2Z1lkQnc9PTAA"
    //                 },
    //                 "browserName": "Chrome",
    //                 "browserVersion": "142.0.0.0"
    //             },
    //             "user": {
    //                 "lockedSafetyMode": false
    //             },
    //             "request": {
    //                 "useSsl": true,
    //                 "internalExperimentFlags": [],
    //                 "consistencyTokenJars": []
    //             },
    //             "playbackContext": {
    //                 "contentPlaybackContext": {
    //                     "html5Preference": "HTML5_PREF_WANTS",
    //                     "signatureTimestamp": 20436,
    //                     "autoCaptionsDefaultOn": false
    //                 }
    //             }
    //         }
    //     });

    //     if let Ok(mut file) = File::create("debug_req.json") {
    //         let _ = file.write_all(serde_json::to_string_pretty(&payload).unwrap().as_bytes());
    //         println!("> Debug: Request payload written to debug_req.json");
    //     }

    //     let res = self.guest_client
    //         .post("https://music.youtube.com/youtubei/v1/player?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30")
    //         .json(&payload)
    //         .send()
    //         .await?;

    //     let data: Value = res.json().await?;

    //     if let Ok(mut file) = File::create("debug_res.json") {
    //         let _ = file.write_all(serde_json::to_string_pretty(&data).unwrap().as_bytes());
    //         println!("> Debug: Response data written to debug_res.json");
    //     }

    //     if let Some(status) = data
    //         .pointer("/playabilityStatus/status")
    //         .and_then(|s| s.as_str())
    //     {
    //         if status != "OK" {
    //             let reason = data
    //                 .pointer("/playabilityStatus/reason")
    //                 .and_then(|s| s.as_str())
    //                 .unwrap_or("Unknown error");
    //             return Err(format!("Video unavailable: {}", reason).into());
    //         }
    //     }

    //     let formats = data
    //         .pointer("/streamingData/adaptiveFormats")
    //         .and_then(|v| v.as_array())
    //         .ok_or("No formats found")?;

    //     let best = formats
    //         .iter()
    //         .filter(|f| {
    //             f["mimeType"]
    //                 .as_str()
    //                 .unwrap_or("")
    //                 .starts_with("audio/webm")
    //         })
    //         .max_by_key(|f| f["bitrate"].as_i64().unwrap_or(0))
    //         .ok_or("No suitable audio stream found")?;

    //     if let Some(url) = best["url"].as_str() {
    //         Ok(url.to_string())
    //     } else {

    //         Err("URL is encrypted (Signature Cipher). Check debug_res.json".into())
    //     }
    // }

    //

    //OG method not working? temp replacement

    pub async fn fetch_stream_url(&self, video_id: &str) -> Result<String, Box<dyn Error>> {
        let video_url = youtube_music_watch_url(video_id)?;

        let mut cmd = tokio::process::Command::new("yt-dlp");
        cmd.arg("-f")
            .arg("bestaudio")
            // Ask for the tv_simply client first, falling back to yt-dlp's own preference.
            //
            // This is what fixes the HTTP 403 "stream refused" failures. Left to itself
            // yt-dlp picks `android_vr`, because that client needs no PO token to *extract* —
            // but YouTube now refuses to *serve* the URLs it hands out: an unbounded GET, and
            // even a mid-file range request, both come back 403. Meanwhile the `web`/
            // `web_safari` clients can obtain a token but are SABR-only, so they return no
            // direct URL at all ("Only images are available"). `tv_simply` is the one client
            // that both takes a PO token and still yields a plain HTTPS URL, and its URLs
            // serve normally.
            //
            // It needs a PO token provider plugin installed for yt-dlp; without one this
            // client fails to resolve, which is why `default` stays in the list as a fallback
            // rather than being replaced outright.
            .arg("--extractor-args")
            .arg("youtube:player_client=tv_simply,default")
            .arg("-g")
            .arg(video_url)
            // Null stdin: yt-dlp is non-interactive and must not sit on the TUI's terminal input.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // stderr is discarded on purpose: callers drop our error text anyway, and an
            // undrained pipe could fill up and wedge yt-dlp before it ever exits.
            .stderr(Stdio::null());
        crate::player::die_with_parent(cmd.as_std_mut());

        // The child belongs to this future. A hard timeout kills and reaps it explicitly; cancelling
        // the future (for a newer playback request or shutdown) drops it with kill_on_drop enabled.
        let child = spawn_resolver_child(&mut cmd)?;
        let output = wait_for_resolver_output(child, YTDLP_TIMEOUT).await?;

        if !output.status.success() {
            return Err(format!("yt-dlp failed to get stream URL: exit {}", output.status).into());
        }

        let stream_url = String::from_utf8(output.stdout)?.trim().to_string();

        if stream_url.is_empty() {
            return Err("yt-dlp returned an empty URL".into());
        }

        Ok(stream_url)
    }
    pub async fn like_song(&self, video_id: &str) -> Result<(), Box<dyn Error>> {
        let url = "https://music.youtube.com/youtubei/v1/like/like";
        let body = json!({
            "context": {
                "client": {
                    "clientName": "WEB_REMIX",
                    "clientVersion": "1.20251215.03.00",
                    "hl": "en",
                    "gl": "IN"
                }
            },
            "target": {
                "videoId": video_id
            }
        });
        self.post_auth(url, &body).await?;
        Ok(())
    }

    pub async fn add_to_playlist(
        &self,
        playlist_id: &str,
        video_id: &str,
    ) -> Result<(), Box<dyn Error>> {
        let url = "https://music.youtube.com/youtubei/v1/browse/edit_playlist";
        let body = json!({
            "context": {
                "client": {
                    "clientName": "WEB_REMIX",
                    "clientVersion": "1.20251215.03.00",
                    "hl": "en",
                    "gl": "IN"
                }
            },
            "actions":[{
                "addedVideoId":video_id,
                "action":"ACTION_ADD_VIDEO",
                "dedupeOption":"DEDUPE_OPTION_CHECK"}],
            "playlistId":playlist_id
        });
        self.post_auth(url, &body).await?;
        Ok(())
    }
    pub async fn fetch_account_name(&self) -> Result<String, Box<dyn Error>> {
        let url = "https://music.youtube.com/youtubei/v1/account/account_menu";
        let body = json!({
            "context": {
                "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20251215.03.00", "hl": "en", "gl": "IN" }
            }
        });

        let res = self.post_auth(url, &body).await?;

        // The account name is the authoritative answer, so look for it FIRST. The `logged_in`
        // tracking flag was checked first and returned early on "Guest", but YouTube does not set that
        // flag consistently — so the greeting flapped between the real name and "Guest" between runs
        // of the very same build, with the same cookies, while the library loaded fine either way.
        if let Some(name) = res
            .pointer("/actions/0/openPopupAction/popup/multiPageMenuRenderer/header/activeAccountHeaderRenderer/accountName/runs/0/text")
            .and_then(|v| v.as_str())
            && !name.trim().is_empty() {
                return Ok(name.to_string());
            }

        // No name in the payload: fall back to the flag to distinguish "signed in but unnamed" from
        // a genuinely anonymous session.
        let logged_in = res
            .pointer("/responseContext/serviceTrackingParams")
            .and_then(|v| v.as_array())
            .map(|params| {
                serde_json::to_string(params)
                    .unwrap_or_default()
                    .contains(r#""key":"logged_in","value":"1""#)
            })
            .unwrap_or(false);

        Ok(if logged_in { "Logged In" } else { "Guest" }.to_string())
    }

    pub async fn search_songs(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SongDetails>, Box<dyn Error>> {
        let url = "https://music.youtube.com/youtubei/v1/search";
        let body = json!({
            "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20251215.03.00", "hl": "en", "gl": "IN" } },
            "query": query,
            "params": "EgWKAQIIAWoKEAMQBRAKEAoQCQ=="
        });
        let res = self.post_auth(url, &body).await?;
        self.parse_search_results(res, limit)
    }

    pub async fn fetch_library_playlists(&self) -> Result<Vec<PlaylistDetails>, Box<dyn Error>> {
        let url = "https://music.youtube.com/youtubei/v1/browse";
        let body = json!({
            "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20251215.03.00", "hl": "en", "gl": "IN" } },
            "browseId": "FEmusic_liked_playlists"
        });
        let mut res = self.post_auth(url, &body).await?;
        let mut playlists = Vec::new();
        let mut seen_tokens = std::collections::HashSet::new();
        const MAX_LIBRARY_PAGES: usize = 32;

        for _ in 0..MAX_LIBRARY_PAGES {
            let (page, continuation) = self.parse_library_playlists_page(&res)?;
            for playlist in page {
                if !playlists
                    .iter()
                    .any(|existing: &PlaylistDetails| existing.playlist_id == playlist.playlist_id)
                {
                    playlists.push(playlist);
                }
            }

            let Some(token) = continuation else { break };
            if !seen_tokens.insert(token.clone()) {
                break;
            }
            let continuation_body = json!({
                "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20251215.03.00", "hl": "en", "gl": "IN" } },
                "continuation": token
            });
            res = self.post_auth(url, &continuation_body).await?;
        }

        // An expired cookie jar still answers 200, just with a logged-out payload that carries no
        // playlists at all. Returning that as an empty Ok opened a chooser reading "Select (1-0)" that
        // could not accept any key — indistinguishable, from the user's side, from the app hanging.
        if playlists.is_empty() {
            return Err("No playlists / cookies expired".into());
        }

        Ok(playlists)
    }

    pub async fn fetch_continuation(
        &self,
        token: &str,
    ) -> Result<(Vec<SongDetails>, Option<String>), Box<dyn Error>> {
        let body = json!({
            "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20251215.03.00", "hl": "en", "gl": "IN" } },
            "continuation": token
        });
        let res = self
            .post_auth("https://music.youtube.com/youtubei/v1/browse", &body)
            .await?;
        self.parse_playlist_songs(res)
    }

    pub async fn fetch_playlist_songs(
        &self,
        playlist_id: &str,
        limit: usize,
    ) -> Result<(Vec<SongDetails>, Option<String>), Box<dyn Error>> {
        let mut songs = Vec::new();
        let mut next_token: Option<String> = None;
        let mut is_first = true;

        // Bound the paging. A page can hand back a continuation token while yielding no *playable*
        // rows (every entry deleted or region-blocked), and the length-based condition alone would
        // then keep issuing requests indefinitely, hanging the library browser on the API.
        const MAX_PAGES: usize = 32;
        let mut pages = 0;
        let mut seen_tokens: std::collections::HashSet<String> = std::collections::HashSet::new();

        while songs.len() < limit && pages < MAX_PAGES {
            pages += 1;
            let body = if is_first {
                let bid = if playlist_id.starts_with("VL") {
                    playlist_id.to_string()
                } else {
                    format!("VL{}", playlist_id)
                };
                json!({
                    "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20251215.03.00", "hl": "en", "gl": "IN" } },
                    "browseId": bid
                })
            } else {
                let t = next_token.take().ok_or("Token missing")?;
                if !seen_tokens.insert(t.clone()) {
                    next_token = None;
                    break;
                }
                json!({
                    "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20251215.03.00", "hl": "en", "gl": "IN" } },
                    "continuation": t
                })
            };

            let res = self
                .post_auth("https://music.youtube.com/youtubei/v1/browse", &body)
                .await?;
            let (batch, new_token) = self.parse_playlist_songs(res)?;

            if batch.is_empty() && new_token.is_none() {
                break;
            }

            // Keep the WHOLE batch. Truncating it to `limit` while still storing the next page's
            // continuation token made the discarded tail permanently unreachable: the caller resumed
            // from the page *after* the truncated one, so a run of playlist entries could never be
            // browsed, selected or shuffled. `limit` is a soft target, and overshooting it by less
            // than one page costs nothing.
            songs.extend(batch);

            next_token = new_token;
            is_first = false;

            if next_token.is_none() {
                break;
            }
        }
        Ok((songs, next_token))
    }

    pub async fn fetch_related_songs(
        &self,
        video_id: &str,
        playlist_id: Option<&str>,
        limit: usize,
        shuffle: bool,
    ) -> Result<Vec<SongDetails>, Box<dyn Error>> {
        let url = "https://music.youtube.com/youtubei/v1/next";

        let resolved_playlist_id = match playlist_id {
            Some(id) => id.to_string(),
            None => format!("RDAMVM{}", video_id),
        };

        let params = if shuffle {
            Some("wAEB8gECKAE%3D")
        } else if playlist_id.is_none() {
            Some("wAEB")
        } else {
            None
        };

        let mut payload = json!({
            "context": {
                "client": {
                    "clientName": "WEB_REMIX",
                    "clientVersion": "1.20251215.03.00",
                    "hl": "en",
                    "gl": "IN"
                }
            },
            "videoId": video_id,
            "playlistId": resolved_playlist_id,
            "isAudioOnly": true
        });
        if let Some(params) = params {
            payload["params"] = json!(params);
        }

        let mut res = self.post_auth(url, &payload).await?;
        let mut songs = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_tokens = std::collections::HashSet::new();
        const MAX_RELATED_PAGES: usize = 32;

        for _ in 0..MAX_RELATED_PAGES {
            let (page, continuation) = self.parse_related_songs_page(res, video_id, limit)?;
            for song in page {
                if seen_ids.insert(song.video_id.clone()) {
                    songs.push(song);
                    if songs.len() >= limit {
                        songs.truncate(limit);
                        return Ok(songs);
                    }
                }
            }

            let Some(token) = continuation else { break };
            if !seen_tokens.insert(token.clone()) {
                break;
            }
            let continuation_payload = json!({
                "context": {
                    "client": {
                        "clientName": "WEB_REMIX",
                        "clientVersion": "1.20251215.03.00",
                        "hl": "en",
                        "gl": "IN"
                    }
                },
                "continuation": token
            });
            res = self.post_auth(url, &continuation_payload).await?;
        }
        Ok(songs)
    }

    fn parse_search_results(
        &self,
        res: Value,
        limit: usize,
    ) -> Result<Vec<SongDetails>, Box<dyn Error>> {
        let mut songs = Vec::new();
        if let Some(tabs) = res.pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents").and_then(|v| v.as_array()) {
            for section in tabs {
                if let Some(contents) = section.pointer("/musicShelfRenderer/contents").and_then(|v| v.as_array()) {
                    for item in contents {
                        if let Some(song) = parse_music_item(item) {
                            songs.push(song);
                            if songs.len() >= limit { return Ok(songs); }
                        }
                    }
                }
            }
        }
        Ok(songs)
    }

    #[cfg(test)]
    fn parse_library_playlists(&self, res: Value) -> Result<Vec<PlaylistDetails>, Box<dyn Error>> {
        Ok(self.parse_library_playlists_page(&res)?.0)
    }

    fn parse_library_playlists_page(
        &self,
        res: &Value,
    ) -> Result<(Vec<PlaylistDetails>, Option<String>), Box<dyn Error>> {
        let mut playlists = Vec::new();
        let mut renderers = Vec::new();
        collect_objects_named(res, "musicTwoRowItemRenderer", &mut renderers);

        for data in renderers {
            let title = data
                .pointer("/title/runs/0/text")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();

            if title == "Episodes for Later" {
                continue;
            }

            let id = data
                .pointer("/navigationEndpoint/browseEndpoint/browseId")
                .and_then(|v| v.as_str())
                .map(|s| s.trim_start_matches("VL").to_string())
                .unwrap_or_default();

            let mut count = data
                .pointer("/subtitle/runs")
                .and_then(|v| v.as_array())
                .and_then(|runs| runs.last())
                .and_then(|run| run.pointer("/text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if count == "Auto playlist" {
                count = "∞".to_string();
            }
            if !id.is_empty() {
                playlists.push(PlaylistDetails {
                    title,
                    playlist_id: id,
                    count,
                    continuation_token: None,
                });
            }
        }
        Ok((playlists, find_continuation_token(res)))
    }

    fn parse_playlist_songs(
        &self,
        res: Value,
    ) -> Result<(Vec<SongDetails>, Option<String>), Box<dyn Error>> {
        let mut songs = Vec::new();
        let mut token = None;

        let action_items = res
            .pointer("/onResponseReceivedActions")
            .and_then(|value| value.as_array())
            .and_then(|actions| {
                actions.iter().find_map(|action| {
                    action
                        .pointer("/appendContinuationItemsAction/continuationItems")
                        .and_then(|value| value.as_array())
                })
            });
        let items = res.pointer("/contents/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer/contents/0/musicPlaylistShelfRenderer/contents")
                .or_else(|| res.pointer("/onResponseReceivedActions/0/appendContinuationItemsAction/continuationItems"))
                .or_else(|| res.pointer("/continuationContents/musicPlaylistShelfContinuation/contents"))
                .or_else(|| res.pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicPlaylistShelfRenderer/contents"))
                .and_then(|v| v.as_array())
                .or(action_items);

        if let Some(items) = items {
            for item in items {
                if let Some(data) = item.pointer("/musicResponsiveListItemRenderer") {
                    let title = data.pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
                    let video_id = music_item_video_id(data).unwrap_or_default().to_string();

                    if video_id.is_empty() {
                        continue;
                    }

                    let duration = renderer_text(data.pointer(
                        "/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text",
                    ))
                    .unwrap_or("0:00")
                    .to_string();
                    let mut artists = Vec::new();
                    let mut album = "Unknown".to_string();

                    if let Some(runs) = data
                        .pointer(
                            "/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs",
                        )
                        .and_then(|v| v.as_array())
                    {
                        for run in runs {
                            let text = run.pointer("/text").and_then(|v| v.as_str()).unwrap_or("");
                            if text == " • " || text.trim().is_empty() {
                                continue;
                            }

                            // Dispatch on pageType FIRST. The year/duration heuristics used to be
                            // applied to every run, which also threw away real all-digit albums
                            // ("1989", "21") and artists ("311") that YTM had explicitly tagged —
                            // leaving album at "Unknown", which then went into the file's ALBUM tag
                            // and into the lrclib lookup.
                            match run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType").and_then(|v| v.as_str()) {
                                    Some("MUSIC_PAGE_TYPE_ARTIST") => artists.push(text.to_string()),
                                    Some("MUSIC_PAGE_TYPE_ALBUM") => album = text.to_string(),
                                    // untagged run: now it is safe to guess it is a year or a duration
                                    _ => {
                                        let is_year = text.len() == 4
                                            && text.chars().all(|c| c.is_ascii_digit());
                                        let is_duration = text.contains(':');
                                        if !is_year && !is_duration && artists.is_empty() && text != "E" {
                                            artists.push(text.to_string());
                                        }
                                    }
                                }
                        }
                    }

                    songs.push(SongDetails {
                        title,
                        video_id,
                        artists,
                        album,
                        duration,
                        thumbnail_url: parse_thumbnail(data),
                    });
                } else if let Some(t) = item
                    .pointer(
                        "/continuationItemRenderer/continuationEndpoint/continuationCommand/token",
                    )
                    .and_then(|v| v.as_str())
                {
                    token = Some(t.to_string());
                }
            }
        }
        if token.is_none() {
            token = find_continuation_token(&res);
        }
        Ok((songs, token))
    }

    #[cfg(test)]
    fn parse_related_songs(
        &self,
        res: Value,
        video_id: &str,
        limit: usize,
    ) -> Result<Vec<SongDetails>, Box<dyn Error>> {
        Ok(self.parse_related_songs_page(res, video_id, limit)?.0)
    }

    fn parse_related_songs_page(
        &self,
        res: Value,
        video_id: &str,
        limit: usize,
    ) -> Result<(Vec<SongDetails>, Option<String>), Box<dyn Error>> {
        let continuation = find_continuation_token(&res);
        // Split around the seed track. For a library playlist the /next panel comes back in natural
        // order with the playing entry marked `selected`, so entries BEFORE the seed are ones the
        // user has already moved past — playing them first would jump the mix back to the start of
        // the playlist. Entries after the seed come first; the earlier ones are only a wrap-around
        // fallback, which is also what makes the "seed is the last entry" case work at all.
        let mut after_seed: Vec<SongDetails> = Vec::new();
        let mut before_seed: Vec<SongDetails> = Vec::new();
        let mut seen_seed = false;

        let mut renderers = Vec::new();
        collect_objects_named(&res, "playlistPanelVideoRenderer", &mut renderers);
        for r in renderers {
            let item_id = r.pointer("/videoId").and_then(|v| v.as_str()).unwrap_or("");
            let is_selected = r
                .pointer("/selected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if is_selected || item_id == video_id {
                seen_seed = true;
                continue;
            }
            if item_id.is_empty() {
                continue;
            }

            let title = r
                .pointer("/title/runs/0/text")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
            let duration = renderer_text(r.pointer("/lengthText"))
                .map(parse_duration)
                .unwrap_or("0:00".to_string());

            let mut artists = Vec::new();
            let mut album = "Unknown".to_string();

            if let Some(runs) = r.pointer("/longBylineText/runs").and_then(|v| v.as_array()) {
                for run in runs {
                    let text = run.pointer("/text").and_then(|v| v.as_str()).unwrap_or("");

                    if text == " • " || text == ", " || text == " & " || text.trim().is_empty() {
                        continue;
                    }

                    let page_type = run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                        .and_then(|v| v.as_str());

                    match page_type {
                        Some("MUSIC_PAGE_TYPE_ARTIST") | Some("MUSIC_PAGE_TYPE_USER_CHANNEL") => {
                            artists.push(text.to_string());
                        }
                        Some("MUSIC_PAGE_TYPE_ALBUM") => {
                            album = text.to_string();
                        }
                        _ => {
                            let is_year =
                                text.len() == 4 && text.chars().all(|c| c.is_ascii_digit());
                            if !is_year {
                                if artists.is_empty() {
                                    artists.push(text.to_string());
                                } else if album == "Unknown" {
                                    album = text.to_string();
                                }
                            }
                        }
                    }
                }
            } else if let Some(artist_text) = r
                .pointer("/shortBylineText/runs/0/text")
                .and_then(|v| v.as_str())
            {
                artists.push(artist_text.to_string());
            }

            let song = SongDetails {
                title,
                video_id: item_id.to_string(),
                artists,
                album,
                duration,
                thumbnail_url: parse_thumbnail(r),
            };

            if seen_seed {
                after_seed.push(song);
            } else {
                before_seed.push(song);
            }
        }

        // Continue past the seed first, then wrap around to what came before it.
        let mut related = after_seed;
        related.extend(before_seed);
        related.truncate(limit);
        Ok((related, continuation))
    }
}

fn parse_music_item(item: &Value) -> Option<SongDetails> {
    let r = item.pointer("/musicResponsiveListItemRenderer")?;
    let raw_title = r
        .pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text")?
        .as_str()?
        .to_string();
    let video_id = music_item_video_id(r)?.to_string();

    let acc_label = r.pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/accessibility/accessibilityData/label").and_then(|v| v.as_str()).unwrap_or("");

    if !video_id.is_empty() {
        let parts: Vec<&str> = acc_label.split(" • ").collect();

        // Artists: prefer the pageType-tagged runs, exactly like parse_playlist_songs. Each ARTIST run
        // is one artist, so a band name that itself contains '&' or ',' ("Earth, Wind & Fire") stays
        // whole while genuinely distinct artists still arrive as separate runs. Splitting the
        // accessibility label on '&'/',' shattered such names into bogus artists that then poisoned
        // the ARTIST tag and the lrclib lyrics query. Fall back to the label split only when the
        // tagged runs are absent, so behaviour is unchanged wherever they are.
        let tagged_artists: Vec<String> = r
            .pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs")
            .and_then(|v| v.as_array())
            .map(|runs| {
                runs.iter()
                    .filter(|run| {
                        matches!(
                            run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType").and_then(|v| v.as_str()),
                            Some("MUSIC_PAGE_TYPE_ARTIST") | Some("MUSIC_PAGE_TYPE_USER_CHANNEL")
                        )
                    })
                    .filter_map(|run| run.pointer("/text").and_then(|v| v.as_str()))
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| t.to_string())
                    .collect()
            })
            .unwrap_or_default();

        let artists: Vec<String> = if !tagged_artists.is_empty() {
            tagged_artists
        } else {
            parts
                .iter()
                .find(|part| {
                    let text = part.trim();
                    !text.is_empty()
                        && !matches!(text.to_ascii_lowercase().as_str(), "song" | "video")
                        && parse_labeled_duration(text).is_none()
                })
                .copied()
                .unwrap_or("")
                .split(&['&', ','][..])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };

        let runs = r
            .pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs")
            .and_then(|value| value.as_array());
        let tagged_album = runs.and_then(|runs| {
            runs.iter().find_map(|run| {
                let page_type = run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType").and_then(|value| value.as_str());
                (page_type == Some("MUSIC_PAGE_TYPE_ALBUM"))
                    .then(|| run.pointer("/text").and_then(|value| value.as_str()))
                    .flatten()
            })
        });
        let duration = parts
            .iter()
            .rev()
            .find_map(|part| parse_labeled_duration(part))
            .or_else(|| {
                renderer_text(
                    r.pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text"),
                )
                .map(parse_duration)
                .filter(|duration| duration != "0:00")
            })
            .unwrap_or_else(|| "0:00".to_string());
        let album = tagged_album
            .map(str::to_string)
            .or_else(|| {
                let duration_index = parts
                    .iter()
                    .rposition(|part| parse_labeled_duration(part).is_some())?;
                let candidates: Vec<&str> = parts[..duration_index]
                    .iter()
                    .copied()
                    .filter(|part| {
                        let text = part.trim();
                        !text.is_empty()
                            && !matches!(text.to_ascii_lowercase().as_str(), "song" | "video")
                            && !(text.len() == 4
                                && text.chars().all(|character| character.is_ascii_digit()))
                    })
                    .collect();
                (candidates.len() >= 2).then(|| candidates[candidates.len() - 1].to_string())
            })
            .unwrap_or_else(|| "Single".to_string());
        let thumbnail_url = parse_thumbnail(r);

        return Some(SongDetails {
            title: raw_title,
            video_id,
            artists,
            album,
            duration,
            thumbnail_url,
        });
    }
    None
}

fn renderer_text(value: Option<&Value>) -> Option<&str> {
    let value = value?;
    value
        .pointer("/simpleText")
        .and_then(|text| text.as_str())
        .or_else(|| value.pointer("/runs/0/text").and_then(|text| text.as_str()))
}

fn music_item_video_id(item: &Value) -> Option<&str> {
    item.pointer("/playlistItemData/videoId")
        .or_else(|| item.pointer("/navigationEndpoint/watchEndpoint/videoId"))
        .or_else(|| item.pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/watchEndpoint/videoId"))
        .or_else(|| item.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId"))
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty())
}

fn collect_objects_named<'a>(value: &'a Value, key: &str, output: &mut Vec<&'a Value>) {
    match value {
        Value::Object(object) => {
            if let Some(found) = object.get(key) {
                output.push(found);
            }
            for child in object.values() {
                collect_objects_named(child, key, output);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_objects_named(child, key, output);
            }
        }
        _ => {}
    }
}

fn find_continuation_token(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            if let Some(token) = object
                .get("continuationCommand")
                .and_then(|command| command.get("token"))
                .or_else(|| {
                    object
                        .get("nextContinuationData")
                        .and_then(|data| data.get("continuation"))
                })
                .or_else(|| {
                    object
                        .get("reloadContinuationData")
                        .and_then(|data| data.get("continuation"))
                })
                .and_then(|token| token.as_str())
            {
                return Some(token.to_string());
            }
            object.values().find_map(find_continuation_token)
        }
        Value::Array(array) => array.iter().find_map(find_continuation_token),
        _ => None,
    }
}

fn parse_duration(s: &str) -> String {
    if s.contains(':') {
        let nums: Vec<u64> = s
            .split(':')
            .filter_map(|part| part.trim().parse().ok())
            .collect();
        return match nums.as_slice() {
            [hours, minutes, seconds] => format!("{}:{:02}:{:02}", hours, minutes, seconds),
            [minutes, seconds] => format!("{}:{:02}", minutes, seconds),
            _ => "0:00".to_string(),
        };
    }

    let parts: Vec<&str> = s
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect();
    let mut total_seconds = 0u64;
    let mut found_unit = false;
    for pair in parts.windows(2) {
        let Ok(value) = pair[0].parse::<u64>() else {
            continue;
        };
        let unit = pair[1].to_ascii_lowercase();
        if unit.starts_with("hour") {
            total_seconds = total_seconds.saturating_add(value.saturating_mul(3600));
            found_unit = true;
        } else if unit.starts_with("minute") {
            total_seconds = total_seconds.saturating_add(value.saturating_mul(60));
            found_unit = true;
        } else if unit.starts_with("second") {
            total_seconds = total_seconds.saturating_add(value);
            found_unit = true;
        }
    }
    if !found_unit {
        total_seconds = parts
            .first()
            .and_then(|part| part.parse::<u64>().ok())
            .unwrap_or(0);
    }
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

fn parse_labeled_duration(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if !text.contains(':')
        && !lower.contains("hour")
        && !lower.contains("minute")
        && !lower.contains("second")
    {
        return None;
    }
    let duration = parse_duration(text);
    (duration != "0:00").then_some(duration)
}

fn parse_thumbnail(r: &Value) -> Option<String> {
    let thumbs = r
        .pointer("/thumbnail/thumbnails")
        .or_else(|| r.pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails"));

    thumbs
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.last())
        .and_then(|obj| obj.pointer("/url"))
        .and_then(|v| v.as_str())
        .map(|s| {
            let url = s.to_string();
            let target_res = "=w1200-h1200-l90-rj-c";

            // Only googleusercontent-style URLs carry an "=w60-h60-..." size suffix. In a URL with
            // a query string (i.ytimg.com/vi/ID/hq.jpg?sqp=...) the first '=' sits inside a
            // parameter, so rewriting from there produced a broken URL and the cover-art fetch
            // silently failed. Leave those alone.
            if url.contains('?') {
                return url;
            }

            if let Some(pos) = url.find('=') {
                return format!("{}{}", &url[..pos], target_res);
            }

            if url.contains("/s") {
                return url
                    .split('/')
                    .map(|part| {
                        if part.starts_with('s')
                            && part.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
                        {
                            "s800"
                        } else {
                            part
                        }
                    })
                    .collect::<Vec<&str>>()
                    .join("/");
            }

            format!("{}{}", url, target_res)
        })
}
pub fn split_title_artist(input: &str) -> (String, String) {
    if let (Some(start), Some(end)) = (input.rfind('['), input.rfind(']'))
        && end > start
    {
        let title = input[..start].trim().to_string();
        let artist = input[start + 1..end].trim().to_string();
        return (title, artist);
    }
    (input.trim().to_string(), String::new())
}

fn youtube_music_watch_url(video_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    let video_id = video_id.trim();
    if video_id.is_empty() {
        return Err("empty video_id".into());
    }
    Ok(format!("https://music.youtube.com/watch?v={}", video_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSON parsers do not touch the HTTP clients, but they hang off `YTMusic`, so tests need an
    /// instance. An empty cookie jar is enough — it just yields a guest client.
    ///
    /// Built exactly once and cloned. Creating the temp cookie file per call raced when tests ran in
    /// parallel: `line!()` expands where it is written, not at the call site, so every caller shared
    /// one filename and whichever test finished first deleted it out from under the others.
    fn test_client() -> YTMusic {
        static CLIENT: std::sync::OnceLock<YTMusic> = std::sync::OnceLock::new();
        CLIENT
            .get_or_init(|| {
                let path = std::env::temp_dir()
                    .join(format!("whytui-test-cookies-{}.txt", std::process::id()));
                std::fs::write(&path, "").unwrap();
                let client = YTMusic::new_with_cookies(path.to_str().unwrap()).unwrap();
                let _ = std::fs::remove_file(&path);
                client
            })
            .clone()
    }

    fn watch_next_panel(items: Value) -> Value {
        json!({"contents":{"singleColumnMusicWatchNextResultsRenderer":{"tabbedRenderer":
            {"watchNextTabbedResultsRenderer":{"tabs":[{"tabRenderer":{"content":
            {"musicQueueRenderer":{"content":{"playlistPanelRenderer":{"contents": items}}}}}}]}}}}})
    }

    fn panel_song(video_id: &str, title: &str, selected: bool) -> Value {
        json!({"playlistPanelVideoRenderer":{
            "videoId": video_id,
            "selected": selected,
            "title": {"runs":[{"text": title}]},
            "lengthText": {"runs":[{"text":"3:00"}]},
            "longBylineText": {"runs":[{"text":"Some Artist"}]}
        }})
    }

    #[test]
    fn parse_related_songs_keeps_entries_before_the_seed() {
        // The seed is last in the panel — what YTM returns for the final entry of a playlist.
        // Gating on "have we passed the seed yet?" returned nothing at all here, so autoplay died.
        let res = watch_next_panel(json!([
            panel_song("before1", "Before One", false),
            panel_song("before2", "Before Two", false),
            panel_song("seedvid", "The Seed", true),
        ]));

        let related = test_client()
            .parse_related_songs(res, "seedvid", 50)
            .unwrap();

        let ids: Vec<&str> = related.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(ids, vec!["before1", "before2"]);
    }

    #[test]
    fn parse_related_songs_plays_after_the_seed_before_wrapping_around() {
        // A library playlist comes back in natural order with the playing entry `selected`. The
        // tracks before it are ones the user already passed, so they must not be played first.
        let res = watch_next_panel(json!([
            panel_song("early1", "Early One", false),
            panel_song("early2", "Early Two", false),
            panel_song("seedvid", "The Seed", true),
            panel_song("later1", "Later One", false),
            panel_song("later2", "Later Two", false),
        ]));

        let related = test_client()
            .parse_related_songs(res, "seedvid", 50)
            .unwrap();

        let ids: Vec<&str> = related.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(ids, vec!["later1", "later2", "early1", "early2"]);
    }

    #[test]
    fn parse_related_songs_limit_prefers_tracks_after_the_seed() {
        let res = watch_next_panel(json!([
            panel_song("early1", "Early One", false),
            panel_song("early2", "Early Two", false),
            panel_song("seedvid", "The Seed", true),
            panel_song("later1", "Later One", false),
        ]));

        let related = test_client()
            .parse_related_songs(res, "seedvid", 1)
            .unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].video_id, "later1");
    }

    /// Build a search-result item as parse_music_item consumes it. `artist_runs` are the tagged runs
    /// of flexColumns[1]; pass an empty slice to omit them (exercising the label-split fallback).
    fn search_item(title: &str, vid: &str, acc_label: &str, artist_runs: &[(&str, &str)]) -> Value {
        let mut flex1 = json!({
            "musicResponsiveListItemFlexColumnRenderer": {
                "text": {
                    "accessibility": {"accessibilityData": {"label": acc_label}}
                }
            }
        });
        if !artist_runs.is_empty() {
            let runs: Vec<Value> = artist_runs
                .iter()
                .map(|(text, page_type)| json!({
                    "text": text,
                    "navigationEndpoint": {"browseEndpoint": {"browseEndpointContextSupportedConfigs":
                        {"browseEndpointContextMusicConfig": {"pageType": page_type}}}}
                }))
                .collect();
            flex1["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"] = json!(runs);
        }
        json!({"musicResponsiveListItemRenderer": {
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": title}]}}},
                flex1
            ],
            "playlistItemData": {"videoId": vid}
        }})
    }

    #[test]
    fn parse_music_item_keeps_a_band_name_with_ampersand_whole() {
        // "Earth, Wind & Fire" is ONE artist. Splitting the label on '&'/',' used to shatter it into
        // ["Earth","Wind","Fire"]; the pageType-tagged run keeps it intact.
        let item = search_item(
            "September",
            "vid1",
            "Earth, Wind & Fire • The Best Of • 3:35",
            &[("Earth, Wind & Fire", "MUSIC_PAGE_TYPE_ARTIST")],
        );
        let song = parse_music_item(&item).expect("item should parse");
        assert_eq!(song.artists, vec!["Earth, Wind & Fire".to_string()]);
        assert_eq!(song.album, "The Best Of");
        assert_eq!(song.duration, "3:35");
    }

    #[test]
    fn parse_music_item_returns_distinct_tagged_artists_separately() {
        let item = search_item(
            "Collab",
            "vid2",
            "Drake, Future • Album • 4:00",
            &[
                ("Drake", "MUSIC_PAGE_TYPE_ARTIST"),
                ("Future", "MUSIC_PAGE_TYPE_ARTIST"),
            ],
        );
        let song = parse_music_item(&item).expect("item should parse");
        assert_eq!(
            song.artists,
            vec!["Drake".to_string(), "Future".to_string()]
        );
    }

    #[test]
    fn parse_music_item_falls_back_to_label_split_without_tagged_runs() {
        // No tagged runs -> old behaviour (split the accessibility label) is preserved exactly.
        let item = search_item("Song", "vid3", "A & B • Album • 3:00", &[]);
        let song = parse_music_item(&item).expect("item should parse");
        assert_eq!(song.artists, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn parse_music_item_accepts_overlay_video_id_and_no_accessibility_label() {
        let item = json!({"musicResponsiveListItemRenderer": {
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Song"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    {"text": "Artist", "navigationEndpoint": {"browseEndpoint":
                        {"browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig":
                        {"pageType": "MUSIC_PAGE_TYPE_ARTIST"}}}}},
                    {"text": "Album", "navigationEndpoint": {"browseEndpoint":
                        {"browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig":
                        {"pageType": "MUSIC_PAGE_TYPE_ALBUM"}}}}}
                ]}}}
            ],
            "fixedColumns": [{"musicResponsiveListItemFixedColumnRenderer":
                {"text": {"simpleText": "3:21"}}}],
            "overlay": {"musicItemThumbnailOverlayRenderer": {"content":
                {"musicPlayButtonRenderer": {"playNavigationEndpoint": {"watchEndpoint":
                {"videoId": "overlay-id"}}}}}}
        }});

        let song = parse_music_item(&item).expect("playable overlay item should parse");
        assert_eq!(song.video_id, "overlay-id");
        assert_eq!(song.artists, vec!["Artist"]);
        assert_eq!(song.album, "Album");
        assert_eq!(song.duration, "3:21");
    }

    #[test]
    fn parse_music_item_classifies_prefixed_accessibility_metadata() {
        let item = search_item(
            "Track",
            "vid-prefixed",
            "Song • Artist • Album • 2024 • 3 minutes, 9 seconds",
            &[],
        );
        let song = parse_music_item(&item).unwrap();
        assert_eq!(song.artists, vec!["Artist"]);
        assert_eq!(song.album, "Album");
        assert_eq!(song.duration, "3:09");
    }

    #[test]
    fn parse_duration_understands_named_units() {
        assert_eq!(parse_duration("3 minutes"), "3:00");
        assert_eq!(parse_duration("1 hour, 2 minutes"), "1:02:00");
        assert_eq!(parse_duration("3 minutes, 9 seconds"), "3:09");
        assert_eq!(parse_duration("185 seconds"), "3:05");
    }

    #[test]
    fn playlist_continuation_supports_continuation_contents_and_simple_text() {
        let response = json!({"continuationContents": {"musicPlaylistShelfContinuation": {
            "contents": [
                {"musicResponsiveListItemRenderer": {
                    "flexColumns": [
                        {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{
                            "text": "Continued Song",
                            "navigationEndpoint": {"watchEndpoint": {"videoId": "continued-id"}}
                        }]}}},
                        {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": []}}}
                    ],
                    "fixedColumns": [{"musicResponsiveListItemFixedColumnRenderer":
                        {"text": {"simpleText": "4:02"}}}]
                }}
            ],
            "continuations": [{"nextContinuationData": {"continuation": "next-page"}}]
        }}});

        let (songs, token) = test_client().parse_playlist_songs(response).unwrap();
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].video_id, "continued-id");
        assert_eq!(songs[0].duration, "4:02");
        assert_eq!(token.as_deref(), Some("next-page"));
    }

    #[test]
    fn parse_related_songs_keeps_a_four_digit_album_name() {
        // "1989" is a real album; the 4-digit skip used to run before the pageType dispatch and drop
        // it, leaving album as "Unknown".
        let res = watch_next_panel(json!([
            {"playlistPanelVideoRenderer": {
                "videoId": "vidX",
                "selected": false,
                "title": {"runs": [{"text": "Style"}]},
                "lengthText": {"runs": [{"text": "3:51"}]},
                "longBylineText": {"runs": [
                    {"text": "Taylor Swift", "navigationEndpoint": {"browseEndpoint":
                        {"browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig":
                        {"pageType": "MUSIC_PAGE_TYPE_ARTIST"}}}}},
                    {"text": " • "},
                    {"text": "1989", "navigationEndpoint": {"browseEndpoint":
                        {"browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig":
                        {"pageType": "MUSIC_PAGE_TYPE_ALBUM"}}}}}
                ]}
            }}
        ]));
        let related = test_client()
            .parse_related_songs(res, "seedvid", 50)
            .unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].album, "1989");
        assert_eq!(related[0].artists, vec!["Taylor Swift".to_string()]);
    }

    #[test]
    fn parse_related_songs_excludes_the_seed_track() {
        let res = watch_next_panel(json!([
            panel_song("seedvid", "The Seed", false),
            panel_song("after1", "After One", false),
        ]));

        let related = test_client()
            .parse_related_songs(res, "seedvid", 50)
            .unwrap();

        assert!(related.iter().all(|s| s.video_id != "seedvid"));
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].video_id, "after1");
    }

    #[test]
    fn parse_related_songs_respects_the_limit() {
        let res = watch_next_panel(json!([
            panel_song("a", "A", false),
            panel_song("b", "B", false),
            panel_song("c", "C", false),
        ]));

        let related = test_client()
            .parse_related_songs(res, "seedvid", 2)
            .unwrap();
        assert_eq!(related.len(), 2);
    }

    #[test]
    fn related_parser_reads_playlist_panel_continuations() {
        let response = json!({"continuationContents": {"playlistPanelContinuation": {
            "contents": [panel_song("continued", "Continued", false)],
            "continuations": [{"nextContinuationData": {"continuation": "more-related"}}]
        }}});
        let (songs, token) = test_client()
            .parse_related_songs_page(response, "seedvid", 10)
            .unwrap();
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].video_id, "continued");
        assert_eq!(token.as_deref(), Some("more-related"));
    }

    /// A realistic library payload must parse into selectable playlists.
    ///
    /// The live library could not be exercised (the session's cookies had expired, so YouTube returned
    /// a logged-out payload with zero playlists), so the shape is reproduced here instead of leaving
    /// the parse → select path untested.
    #[test]
    fn a_real_library_payload_parses_into_selectable_playlists() {
        fn row(title: &str, browse_id: &str, subtitle: &str) -> Value {
            json!({"musicTwoRowItemRenderer": {
                "title": {"runs": [{"text": title}]},
                "subtitle": {"runs": [{"text": "Playlist"}, {"text": " • "}, {"text": subtitle}]},
                "navigationEndpoint": {"browseEndpoint": {"browseId": browse_id}}
            }})
        }

        let res = json!({"contents": {"singleColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer":
        {"content": {"sectionListRenderer": {"contents": [{"gridRenderer": {"items": [
            row("Liked Music", "VLLM", "Auto playlist"),
            row("March-May Recap '25", "VLPL_recap", "50 songs"),
            row("Episodes for Later", "VLSE", "3 episodes"),
            row("good game", "VLPL_gg", "69 tracks"),
        ]}}]}}}}]}}});

        let playlists = test_client().parse_library_playlists(res).unwrap();

        // "Episodes for Later" is deliberately skipped; the rest keep their order
        let titles: Vec<&str> = playlists.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Liked Music", "March-May Recap '25", "good game"]
        );
        // the VL prefix is stripped so the id can be reused for a browse request
        assert_eq!(playlists[1].playlist_id, "PL_recap");
        // an auto playlist reports an unbounded count
        assert_eq!(playlists[0].count, "\u{221e}");
        assert_eq!(playlists[2].count, "69 tracks");

        // and the count is what the chooser prompt is sized from
        assert_eq!(playlists.len(), 3);
    }

    #[test]
    fn library_parser_finds_nested_grids_and_their_continuation() {
        let response = json!({"contents": {"unexpectedWrapper": {"sections": [
            {"unrelated": {}},
            {"gridRenderer": {"items": [
                {"musicTwoRowItemRenderer": {
                    "title": {"runs": [{"text": "Nested Playlist"}]},
                    "subtitle": {"runs": [{"text": "12 songs"}]},
                    "navigationEndpoint": {"browseEndpoint": {"browseId": "VLnested"}}
                }}
            ], "continuations": [{"nextContinuationData": {"continuation": "more-lists"}}]}}
        ]}}});

        let (playlists, token) = test_client()
            .parse_library_playlists_page(&response)
            .unwrap();
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].playlist_id, "nested");
        assert_eq!(token.as_deref(), Some("more-lists"));
    }

    #[test]
    fn widget_state_cookies_are_not_sent() {
        // ST-* are per-surface widget tokens; they are never needed for auth and are what blew the
        // header past Google's limit.
        assert!(!is_sendable_cookie("ST-1tu8iv0"));
        assert!(!is_sendable_cookie("ST-xqq1io"));
        // everything an export actually needs survives
        for name in [
            "SAPISID",
            "__Secure-3PAPISID",
            "__Secure-1PSIDTS",
            "SID",
            "HSID",
            "SSID",
            "APISID",
            "SIDCC",
            "LOGIN_INFO",
            "PREF",
            "VISITOR_INFO1_LIVE",
            "YSC",
        ] {
            assert!(is_sendable_cookie(name), "{name} would be dropped");
        }
    }

    #[test]
    fn cookie_header_stays_under_the_server_limit() {
        // Shaped like the export that triggered HTTP 413: a handful of real cookies plus 78 large
        // ST-* tokens. Values here are filler, not credentials.
        let mut jar: Vec<(String, String)> = vec![
            ("SAPISID".into(), "a".repeat(70)),
            ("__Secure-3PAPISID".into(), "b".repeat(70)),
            ("SID".into(), "c".repeat(120)),
            ("LOGIN_INFO".into(), "d".repeat(300)),
        ];
        let auth_only = build_cookie_header(&jar);

        for i in 0..78 {
            jar.push((format!("ST-{:06}", i), "x".repeat(1100)));
        }
        assert!(
            render_cookie_header(&jar).len() > 80_000,
            "test fixture should reproduce an oversized export"
        );

        let header = build_cookie_header(&jar);
        assert!(
            header.len() <= MAX_COOKIE_HEADER_BYTES,
            "header is {} bytes, over the {} byte budget",
            header.len(),
            MAX_COOKIE_HEADER_BYTES
        );
        // the ST-* noise is gone and nothing else changed
        assert_eq!(header, auth_only);
        assert!(!header.contains("ST-"));
        assert!(header.contains("SAPISID="));
        assert!(header.contains("__Secure-3PAPISID="));
    }

    #[test]
    fn cookie_scope_rejects_unrelated_expired_and_wrong_path_entries() {
        let now = 2_000;
        assert!(cookie_applies_to_music_youtube(
            &[".youtube.com", "TRUE", "/", "TRUE", "0", "SAPISID", "x"],
            now
        ));
        assert!(cookie_applies_to_music_youtube(
            &[
                "#HttpOnly_.youtube.com",
                "TRUE",
                "/",
                "TRUE",
                "3000",
                "SID",
                "x"
            ],
            now
        ));
        assert!(!cookie_applies_to_music_youtube(
            &[".example.com", "TRUE", "/", "TRUE", "0", "secret", "x"],
            now
        ));
        assert!(!cookie_applies_to_music_youtube(
            &[".youtube.com", "TRUE", "/", "TRUE", "1000", "SID", "x"],
            now
        ));
        assert!(!cookie_applies_to_music_youtube(
            &[".youtube.com", "TRUE", "/account", "TRUE", "0", "SID", "x"],
            now
        ));
    }

    #[test]
    fn pathological_essential_cookie_cannot_break_header_cap() {
        let header = build_cookie_header(&[("SAPISID".to_string(), "x".repeat(20_000))]);
        assert!(header.len() <= MAX_COOKIE_HEADER_BYTES);
    }

    #[test]
    fn oversized_header_falls_back_to_essential_cookies_only() {
        // Pathological: even after dropping ST-*, the remaining cookies are too big. Auth-critical
        // ones must survive; the rest are shed rather than sending a header the server will reject.
        let jar: Vec<(String, String)> = vec![
            ("SAPISID".into(), "a".repeat(80)),
            ("__Secure-1PSID".into(), "b".repeat(80)),
            ("PREF".into(), "p".repeat(80)),
            ("VISITOR_INFO1_LIVE".into(), "v".repeat(9000)),
            ("CONSISTENCY".into(), "c".repeat(2000)),
        ];
        let header = build_cookie_header(&jar);
        assert!(header.len() <= MAX_COOKIE_HEADER_BYTES, "{}", header.len());
        assert!(header.contains("SAPISID="));
        assert!(header.contains("__Secure-1PSID="));
        assert!(header.contains("PREF="));
        assert!(!header.contains("VISITOR_INFO1_LIVE="));
    }

    #[test]
    fn small_exports_are_left_completely_alone() {
        let jar: Vec<(String, String)> = vec![
            ("SAPISID".into(), "abc".into()),
            ("YSC".into(), "xyz".into()),
        ];
        assert_eq!(build_cookie_header(&jar), "SAPISID=abc; YSC=xyz; ");
    }

    #[test]
    fn youtube_music_watch_url_constructs_correctly() {
        let url = youtube_music_watch_url("abc123").unwrap();
        assert_eq!(url, "https://music.youtube.com/watch?v=abc123");
    }

    #[test]
    fn youtube_music_watch_url_has_no_literal_braces() {
        let url = youtube_music_watch_url("abc123").unwrap();
        assert!(!url.contains('{'));
        assert!(!url.contains('}'));
    }

    #[test]
    fn youtube_music_watch_url_rejects_empty_id() {
        assert!(youtube_music_watch_url("").is_err());
        assert!(youtube_music_watch_url("   ").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_resolver_child_is_killed_and_reaped() {
        let mut command = tokio::process::Command::new("sleep");
        command
            .arg("60")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = spawn_resolver_child(&mut command).expect("test child should start");
        let pid = child.id().expect("spawned child should have a pid") as libc::pid_t;

        // SAFETY: signal 0 does not modify the process; it only checks that the pid exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let resolver = tokio::spawn(async move {
            let _ = started_tx.send(());
            wait_for_resolver_output(child, Duration::from_secs(60)).await
        });
        started_rx
            .await
            .expect("pending resolver task should start");
        resolver.abort();
        assert!(
            resolver
                .await
                .expect_err("resolver should be cancelled")
                .is_cancelled()
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                // A zombie still has a pid, so ESRCH proves both termination and reaping.
                // SAFETY: signal 0 does not modify the process.
                if unsafe { libc::kill(pid, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled resolver child was not reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_resolver_child_is_killed_and_reaped() {
        let mut command = tokio::process::Command::new("sleep");
        command
            .arg("60")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let child = spawn_resolver_child(&mut command).expect("test child should start");
        let pid = child.id().expect("spawned child should have a pid") as libc::pid_t;

        let error = wait_for_resolver_output(child, Duration::ZERO)
            .await
            .expect_err("pending child should hit the deadline");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        // A zombie still responds to signal 0. ESRCH proves the timeout path waited for reaping.
        // SAFETY: signal 0 does not modify the process.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn parse_thumbnail_rewrites_googleusercontent_size_suffix() {
        let v = json!({"thumbnail": {"thumbnails": [
            {"url": "https://lh3.googleusercontent.com/abc=w60-h60-l90-rj"}
        ]}});
        assert_eq!(
            parse_thumbnail(&v).unwrap(),
            "https://lh3.googleusercontent.com/abc=w1200-h1200-l90-rj-c"
        );
    }

    #[test]
    fn parse_thumbnail_leaves_query_string_urls_intact() {
        // truncating at the first '=' used to corrupt this into ".../hqdefault.jpg?sqp=w1200-..."
        let u = "https://i.ytimg.com/vi/ID/hqdefault.jpg?sqp=abc&rs=def";
        let v = json!({"thumbnail": {"thumbnails": [{"url": u}]}});
        assert_eq!(parse_thumbnail(&v).unwrap(), u);
    }

    #[test]
    fn split_title_artist_with_brackets() {
        let (title, artist) = split_title_artist("Song Title [Artist Name]");
        assert_eq!(title, "Song Title");
        assert_eq!(artist, "Artist Name");
    }

    #[test]
    fn split_title_artist_without_brackets() {
        let (title, artist) = split_title_artist("Song Title Only");
        assert_eq!(title, "Song Title Only");
        assert_eq!(artist, "");
    }

    #[test]
    fn split_title_artist_handles_multibyte_titles() {
        // The byte offsets come from rfind, which only ever returns char boundaries, and the byte
        // after an ASCII '[' is a boundary too — so this does not panic on CJK/emoji titles.
        assert_eq!(
            split_title_artist("日本語Song [Artist]"),
            ("日本語Song".to_string(), "Artist".to_string())
        );
        assert_eq!(
            split_title_artist("🎵🎶 emoji [歌手]"),
            ("🎵🎶 emoji".to_string(), "歌手".to_string())
        );
        // brackets in the wrong order must fall through rather than slice backwards
        assert_eq!(
            split_title_artist("]weird["),
            ("]weird[".to_string(), String::new())
        );
    }
}
