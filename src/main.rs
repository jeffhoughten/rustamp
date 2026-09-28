mod audio;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};

use std::sync::OnceLock;

const PRODUCT_NAME: &str = "RustAmp";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const LISTEN_PORT: u16 = 32500;

// What we tell plex.tv this device can do. Same values Plexamp headless
// advertises; the phone app uses these to decide whether to show us as a
// playback target. "pubsub-player" is deliberately left out until the
// websocket command channel is implemented — advertising it now would make
// the phone try to relay commands we can't handle yet.
const PROVIDES: &str = "client,player";
const PROTOCOL_CAPABILITIES: &str = "timeline,playback,playqueues,playqueues-creation";
const PROTOCOL_VERSION: &str = "1";
const DEVICE_CLASS: &str = "pc";

// ---------- Persistent config ----------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Config {
    // Generated once per install and never changed — Plex identifies this
    // device by it. Regenerating it would make Plex see a "new" device.
    client_identifier: String,
    token: String,
    user_id: String,
    player_name: String,
    server_url: String,
}

fn config_path() -> anyhow::Result<PathBuf> {
    let mut dir = dirs_config_dir()?;
    dir.push("rustamp");
    std::fs::create_dir_all(&dir)?;
    dir.push("config.json");
    Ok(dir)
}

fn dirs_config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home).join(".config"));
    }
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        return Ok(PathBuf::from(userprofile).join(".config"));
    }
    anyhow::bail!("could not determine a config directory (no HOME/USERPROFILE set)")
}

fn load_config() -> Option<Config> {
    let path = config_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let cfg: Config = serde_json::from_str(&text).ok()?;
    if cfg.client_identifier.is_empty() || cfg.token.is_empty() {
        return None;
    }
    Some(cfg)
}

fn save_config(cfg: &Config) -> anyhow::Result<()> {
    let path = config_path()?;
    std::fs::write(path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

// The client identifier is needed by plex_headers() everywhere, so it's
// stashed in a global once at startup rather than threaded through every call.
static CLIENT_IDENTIFIER: OnceLock<String> = OnceLock::new();

fn client_identifier() -> &'static str {
    CLIENT_IDENTIFIER
        .get()
        .map(String::as_str)
        .unwrap_or("uninitialized")
}

fn plex_headers(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    builder
        .header("Accept", "application/json")
        .header("X-Plex-Product", PRODUCT_NAME)
        .header("X-Plex-Version", VERSION)
        .header("X-Plex-Client-Identifier", client_identifier())
        .header("X-Plex-Platform", std::env::consts::OS)
        .header("X-Plex-Device", std::env::consts::OS)
        .header("X-Plex-Provides", PROVIDES)
}

fn prompt(label: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

// Pulls a single attribute value out of an XML blob without a full parser —
// the claim response is small and we only need two attributes from it.
fn xml_attr(xml: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = xml.find(&needle)? + needle.len();
    let end = xml[start..].find('"')? + start;
    Some(xml[start..end].to_string())
}

// Exchanges a claim code from https://plex.tv/claim for a long-lived auth
// token. This is the same first-run flow Plexamp headless uses.
async fn claim_exchange(
    client: &reqwest::Client,
    claim_code: &str,
) -> anyhow::Result<(String, String)> {
    let url = format!("https://plex.tv/api/claim/exchange?token={claim_code}");
    // Deliberately not plex_headers(): that adds Accept: application/json,
    // and this endpoint is parsed here as XML.
    let resp = client
        .post(&url)
        .header("Accept", "application/xml")
        .header("X-Plex-Product", PRODUCT_NAME)
        .header("X-Plex-Version", VERSION)
        .header("X-Plex-Client-Identifier", client_identifier())
        .send()
        .await?;

    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("claim exchange failed: {status} — {body}");
    }

    let token = xml_attr(&body, "authToken")
        .ok_or_else(|| anyhow::anyhow!("no authToken in claim response:\n{body}"))?;
    let user_id = xml_attr(&body, "id").unwrap_or_default();
    Ok((token, user_id))
}

// Interactive first-run setup: claim code, player name, server address.
async fn first_run_setup(client: &reqwest::Client) -> anyhow::Result<Config> {
    println!("First run — let's link this player to your Plex account.\n");
    println!("1. Open https://plex.tv/claim in a browser and sign in.");
    println!("2. Copy the claim code shown there (starts with \"claim-\").\n");

    let claim_code = prompt("Claim code: ")?;
    let (token, user_id) = claim_exchange(client, &claim_code).await?;
    println!("Linked.\n");

    let mut player_name = prompt("Player name (as it should appear in Plexamp) [RustAmp]: ")?;
    if player_name.is_empty() {
        player_name = "RustAmp".to_string();
    }

    let mut server_url = prompt("Plex server URL (e.g. http://192.168.1.50:32400): ")?;
    while server_url.is_empty() {
        server_url = prompt("Plex server URL: ")?;
    }
    let server_url = server_url.trim_end_matches('/').to_string();

    Ok(Config {
        client_identifier: client_identifier().to_string(),
        token,
        user_id,
        player_name,
        server_url,
    })
}

// ---------- Companion device registration ----------

// Finds the LAN IP the Pi is reachable on. Connecting a UDP socket doesn't
// send anything; it just makes the OS pick the outbound interface.
fn local_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

// Tells plex.tv where this player lives and what it's called, so other Plex
// clients on the account list it as a playback target. plex.tv forgets
// registrations after a while, so this is re-run periodically.
async fn register_device(client: &reqwest::Client, cfg: &Config) -> anyhow::Result<()> {
    let ip = local_ip().ok_or_else(|| anyhow::anyhow!("could not determine local IP"))?;
    let url = format!(
        "https://plex.tv/devices/{}?Connection[][uri]=http://{}:{}&X-Plex-Device-Name={}",
        cfg.client_identifier,
        ip,
        LISTEN_PORT,
        urlencoding::encode(&cfg.player_name),
    );

    let resp = plex_headers(client.put(&url))
        .header("X-Plex-Token", &cfg.token)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("device registration failed: {status} — {body}");
    }

    println!("Registered '{}' at http://{}:{}", cfg.player_name, ip, LISTEN_PORT);
    Ok(())
}

fn spawn_registration_task(client: reqwest::Client, cfg: Config) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = register_device(&client, &cfg).await {
                eprintln!("registration: {e}");
            }
            // Plexamp re-registers every 12 hours.
            tokio::time::sleep(Duration::from_secs(12 * 60 * 60)).await;
        }
    });
}


// ---------- Plex library browsing ----------

#[derive(Deserialize, Serialize, Debug, Clone)]
struct Directory {
    key: String,
    title: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize, Debug)]
struct SectionsMediaContainer {
    #[serde(rename = "Directory", default)]
    directory: Vec<Directory>,
}

#[derive(Deserialize, Debug)]
struct LibrarySectionsResponse {
    #[serde(rename = "MediaContainer")]
    media_container: SectionsMediaContainer,
}

#[derive(Deserialize, Debug, Clone)]
struct Part {
    key: String,
    // Lyrics ride along as a stream on the part; streamType 4 is the lyric
    // one. PMS includes these in a plain metadata fetch.
    #[serde(rename = "Stream", default)]
    stream: Vec<Stream>,
}

#[derive(Deserialize, Debug, Clone)]
struct Stream {
    #[serde(rename = "streamType")]
    stream_type: Option<u32>,
    key: Option<String>,
    // "lrc" means timed; "txt" is a plain block.
    format: Option<String>,
}

const STREAM_TYPE_LYRIC: u32 = 4;

#[derive(Deserialize, Debug, Clone)]
struct Media {
    #[serde(rename = "Part", default)]
    part: Vec<Part>,
    // Used only to decide whether to bother direct-playing; see
    // codec_unsupported.
    #[serde(rename = "audioCodec")]
    audio_codec: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct Metadata {
    #[serde(rename = "ratingKey")]
    rating_key: String,
    key: Option<String>,
    title: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "parentTitle")]
    parent_title: Option<String>, // album title, when this is a track
    #[serde(rename = "grandparentTitle")]
    grandparent_title: Option<String>, // album artist, when this is a track
    // The track's own artist. On a compilation grandparentTitle is "Various
    // Artists" while this names who actually performed it, so it wins when
    // present — the same precedence Plexamp uses throughout.
    #[serde(rename = "originalTitle")]
    original_title: Option<String>,
    #[serde(rename = "parentRatingKey")]
    parent_rating_key: Option<String>, // the album
    #[serde(rename = "grandparentRatingKey")]
    grandparent_rating_key: Option<String>, // the artist
    // Plex stores a 0-10 rating; the UI shows it as five stars.
    #[serde(rename = "userRating")]
    user_rating: Option<f32>,
    duration: Option<u64>, // ms
    #[serde(rename = "playQueueItemID")]
    play_queue_item_id: Option<u64>,
    thumb: Option<String>,
    #[serde(rename = "parentThumb")]
    parent_thumb: Option<String>,
    #[serde(rename = "grandparentThumb")]
    grandparent_thumb: Option<String>,
    year: Option<u32>,
    index: Option<u32>, // track number
    #[serde(rename = "Media", default, skip_serializing)]
    media: Vec<Media>,
}

#[derive(Deserialize, Debug, Default)]
struct MetadataContainer {
    #[serde(rename = "Metadata", default)]
    metadata: Vec<Metadata>,
    #[serde(rename = "playQueueID")]
    play_queue_id: Option<u64>,
    #[serde(rename = "playQueueVersion")]
    play_queue_version: Option<u64>,
    #[serde(rename = "playQueueSelectedItemID")]
    play_queue_selected_item_id: Option<u64>,
    #[serde(rename = "playQueueTotalCount")]
    play_queue_total_count: Option<u64>,
}

#[derive(Deserialize, Debug)]
struct MetadataResponse {
    #[serde(rename = "MediaContainer")]
    media_container: MetadataContainer,
}

// /hubs/search groups its results into one Hub per result type rather than
// returning a flat Metadata list like the browse endpoints do.
#[derive(Deserialize, Debug, Default)]
struct Hub {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "Metadata", default)]
    metadata: Vec<Metadata>,
}

#[derive(Deserialize, Debug, Default)]
struct HubsContainer {
    #[serde(rename = "Hub", default)]
    hub: Vec<Hub>,
}

#[derive(Deserialize, Debug)]
struct HubsResponse {
    #[serde(rename = "MediaContainer")]
    media_container: HubsContainer,
}

async fn plex_get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> anyhow::Result<T> {
    let req = client.get(url).header("X-Plex-Token", token);
    let resp = plex_headers(req).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GET {url} failed: {status} — {body}");
    }
    Ok(resp.json::<T>().await?)
}

async fn fetch_sections(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
) -> anyhow::Result<Vec<Directory>> {
    let r: LibrarySectionsResponse =
        plex_get_json(client, &format!("{server_url}/library/sections"), token).await?;
    Ok(r.media_container.directory)
}

#[allow(dead_code)]
async fn fetch_section_items(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    section_key: &str,
) -> anyhow::Result<Vec<Metadata>> {
    let r: MetadataResponse = plex_get_json(
        client,
        &format!("{server_url}/library/sections/{section_key}/all"),
        token,
    )
    .await?;
    Ok(r.media_container.metadata)
}

async fn fetch_children(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    rating_key: &str,
) -> anyhow::Result<Vec<Metadata>> {
    let r: MetadataResponse = plex_get_json(
        client,
        &format!("{server_url}/library/metadata/{rating_key}/children"),
        token,
    )
    .await?;
    Ok(r.media_container.metadata)
}

async fn fetch_item(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    rating_key: &str,
) -> anyhow::Result<Metadata> {
    let r: MetadataResponse = plex_get_json(
        client,
        &format!("{server_url}/library/metadata/{rating_key}"),
        token,
    )
    .await?;
    r.media_container
        .metadata
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no metadata for rating key {rating_key}"))
}

// Builds the direct file URL for a track from its Media/Part data. If the
// listing didn't include Media (section listings don't), fetch the item.
async fn stream_url_for(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    track: &Metadata,
) -> anyhow::Result<String> {
    let part_key = match track.media.first().and_then(|m| m.part.first()) {
        Some(p) => p.key.clone(),
        None => {
            let full = fetch_item(client, server_url, token, &track.rating_key).await?;
            full.media
                .first()
                .and_then(|m| m.part.first())
                .map(|p| p.key.clone())
                .ok_or_else(|| anyhow::anyhow!("track \"{}\" has no playable Part", track.title))?
        }
    };
    Ok(format!("{server_url}{part_key}?X-Plex-Token={token}"))
}

// PMS hands back a 21-item window of a play queue by default, and ignores
// `window` on the POST/PUT/DELETE that change one — only a plain GET honours
// it. Anything longer than the window is silently missing, so a long album
// would just stop partway. Verified against PMS 1.43.4.
//
// 1000 covers any album or artist while capping what a Pi has to parse; at
// ~1.8KB an item that is a couple of MB in the worst case.
const QUEUE_WINDOW: u32 = 1000;

async fn fetch_full_queue(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    pq_id: u64,
) -> anyhow::Result<MetadataContainer> {
    // Plain read — no own=1, which would claim the queue for this client and
    // can retire the one we're playing.
    let url =
        format!("{server_url}/playQueues/{pq_id}?includeChapters=1&window={QUEUE_WINDOW}");
    let r: MetadataResponse = plex_get_json(client, &url, token).await?;
    Ok(r.media_container)
}

// A queue response is complete when it holds every item PMS says it has.
fn is_windowed(mc: &MetadataContainer) -> bool {
    mc.play_queue_total_count
        .is_some_and(|total| (mc.metadata.len() as u64) < total)
}

fn redact(url: &str) -> String {
    match url.find("X-Plex-Token=") {
        Some(i) => format!("{}X-Plex-Token=…", &url[..i]),
        None => url.to_string(),
    }
}

// A body that stopped short of its Content-Length. reqwest reports it as a
// decode error, and download_track decodes nothing else, so inside that call
// this only ever means the transfer died partway.
fn is_truncated_body(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>()
        .map(|e| e.is_decode())
        .unwrap_or(false)
}

// Downloads a track, re-resolving its part key once if the server rejects
// it. Plex embeds a version timestamp in part keys, so a key captured when
// the queue was built goes stale if the server re-analyzes the file.
// Codecs the Symphonia build has no decoder for at all. This is only a hint
// that saves downloading a file we could never play — the probe after a
// download is what actually decides, so a missing entry here costs bandwidth,
// not correctness.
const UNDECODABLE_CODECS: &[&str] = &[
    "wmav1", "wmav2", "wmapro", "wmalossless", "wmavoice", "opus", "ape", "wavpack",
    "musepack", "mpc", "tta", "shorten", "speex", "dsd_lsbf", "dsd_msbf",
    "dsd_lsbf_planar", "dsd_msbf_planar", "ra_144", "ra_288", "atrac3", "atrac3p",
];

fn codec_unsupported(track: &Metadata) -> bool {
    track
        .media
        .first()
        .and_then(|m| m.audio_codec.as_deref())
        .is_some_and(|c| UNDECODABLE_CODECS.contains(&c.to_ascii_lowercase().as_str()))
}

// Does Symphonia actually have a decoder for these bytes? Building the decoder
// reads no further than the container header and codec setup, so this is cheap
// even with a whole track resident.
fn is_decodable(bytes: &Arc<[u8]>) -> bool {
    make_decoder(bytes).is_ok()
}

// Transcode sessions we have opened and not yet confirmed stopped.
//
// PMS keeps a session alive until told otherwise — it does not close one when
// the download finishes — and it refuses to start a new transcode while any is
// open. So a download that never reaches its own stop call (an axum handler
// dropped when the browser navigates away, or a start that failed after PMS had
// already registered the session) would block every later transcode with a 400.
//
// This only ever holds ids we generated ourselves. Sessions carry no client
// attribution, so stopping one we did not create could kill the phone's stream.
static OPEN_TRANSCODES: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

fn open_transcodes() -> &'static Mutex<std::collections::HashSet<String>> {
    OPEN_TRANSCODES.get_or_init(Default::default)
}

async fn stop_transcode(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    session: &str,
) {
    let url = format!("{server_url}/video/:/transcode/universal/stop?session={session}");
    let _ = client
        .get(&url)
        .header("X-Plex-Token", token)
        .header("X-Plex-Client-Identifier", client_identifier())
        .send()
        .await;
    open_transcodes().lock().unwrap().remove(session);
}

// Every transcode request carries the same identifying headers. The
// `X-Plex-Client-Profile-Name` one is load-bearing: `X-Plex-Client-Profile-Extra`
// only *adds* targets to a base profile, and PMS ships none for a product it has
// never heard of, so without a recognised profile name it answers 400 "unable to
// find a matching profile". "Generic" matches; "Chrome" and "Plexamp" do not.
fn transcode_request(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header("X-Plex-Token", token)
        .header("X-Plex-Client-Identifier", client_identifier())
        .header("X-Plex-Client-Profile-Name", "Generic")
        .header("X-Plex-Product", PRODUCT_NAME)
        .header("X-Plex-Version", VERSION)
        .header("X-Plex-Device-Name", PRODUCT_NAME)
}

// Two calls, not one. PMS records a transcode *decision* against the session and
// refuses to serve the stream to a session that lacks one — "Denying access due
// to session lacking decision for transcode of key ...". Skipping it is not a
// clean failure: the first transcode after a restart usually works and later
// ones 400, and a session can be terminated mid-stream, which surfaces as a
// track cutting out partway. Verified against PMS 1.43.4: decision-then-start
// succeeded three times running where start alone failed on the second attempt.
async fn transcode_exchange(
    client: &reqwest::Client,
    token: &str,
    decision_url: &str,
    start_url: &str,
) -> anyhow::Result<Vec<u8>> {
    let decision = transcode_request(client, decision_url, token).send().await?;
    if !decision.status().is_success() {
        anyhow::bail!(
            "transcode decision failed: {} for {}",
            decision.status(),
            redact(decision_url)
        );
    }
    let resp = transcode_request(client, start_url, token).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "transcode failed: {} for {}",
            resp.status(),
            redact(start_url)
        );
    }
    Ok(resp.bytes().await?.to_vec())
}

// Ask PMS to re-encode the track to MP3, which Symphonia always decodes.
async fn transcode_attempt(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    track: &Metadata,
) -> anyhow::Result<Vec<u8>> {
    const BITRATE: u32 = 320;
    let session = Uuid::new_v4().to_string();
    open_transcodes().lock().unwrap().insert(session.clone());

    let key = track
        .key
        .clone()
        .unwrap_or_else(|| format!("/library/metadata/{}", track.rating_key));
    let profile_extra = "add-transcode-target(replace=true&type=musicProfile&context=streaming&protocol=http&container=mp3&audioCodec=mp3)";
    // The decision and the stream must agree on every parameter, session
    // included, or the decision does not apply to the request that follows.
    let query = format!(
        "path={}&session={session}&X-Plex-Session-Identifier={session}&musicBitrate={BITRATE}&protocol=http&directPlay=0&directStream=0&X-Plex-Client-Identifier={}&X-Plex-Client-Profile-Extra={}",
        urlencoding::encode(&key),
        client_identifier(),
        urlencoding::encode(profile_extra),
    );
    let decision_url = format!("{server_url}/music/:/transcode/universal/decision?{query}");
    let start_url = format!("{server_url}/music/:/transcode/universal/start.mp3?{query}");

    let out = transcode_exchange(client, token, &decision_url, &start_url).await;
    // Stop either way: a start that failed can still have registered a session.
    stop_transcode(client, server_url, token, &session).await;
    out
}

async fn fetch_transcoded(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    track: &Metadata,
) -> anyhow::Result<Vec<u8>> {
    match transcode_attempt(client, server_url, token, track).await {
        Ok(b) => Ok(b),
        Err(first) => {
            // Most likely a session we could not clean up is still holding the
            // transcoder. Clear ours and give it one more go.
            let stale: Vec<String> =
                open_transcodes().lock().unwrap().iter().cloned().collect();
            if stale.is_empty() {
                return Err(first);
            }
            for session in stale {
                stop_transcode(client, server_url, token, &session).await;
            }
            transcode_attempt(client, server_url, token, track).await
        }
    }
}

async fn fetch_track_bytes(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    track: &Metadata,
) -> anyhow::Result<Arc<[u8]>> {
    // Nothing to gain from pulling down a file with no decoder behind it.
    if codec_unsupported(track) {
        match fetch_transcoded(client, server_url, token, track).await {
            Ok(b) => return Ok(b.into()),
            // Fall through and fetch the original anyway: the player will fail
            // to decode it and move on, which beats stalling the queue on a
            // track that can be neither played nor converted.
            Err(e) => eprintln!("transcode failed for \"{}\": {e:#}", track.title),
        }
    }

    let url = stream_url_for(client, server_url, token, track).await?;
    let bytes: Arc<[u8]> = match download_track(client, &url).await {
        Ok(b) => b.into(),
        Err(e) if e.to_string().contains("404") => {
            let fresh = fetch_item(client, server_url, token, &track.rating_key).await?;
            let url = stream_url_for(client, server_url, token, &fresh).await?;
            download_track(client, &url).await?.into()
        }
        // The part key is fine here — the connection just dropped mid-body,
        // which whole-track downloads are big enough to hit routinely. Same
        // URL, one more go; a persistent fault falls through to the
        // prefetcher's own retry interval rather than looping here.
        Err(e) if is_truncated_body(&e) => download_track(client, &url).await?.into(),
        Err(e) => return Err(e),
    };

    // The codec hint only knows what PMS reported. This catches damaged files
    // and codecs it named but Symphonia still cannot open.
    if is_decodable(&bytes) {
        return Ok(bytes);
    }
    match fetch_transcoded(client, server_url, token, track).await {
        Ok(b) => Ok(b.into()),
        Err(e) => {
            eprintln!("transcode failed for \"{}\": {e:#}", track.title);
            Ok(bytes)
        }
    }
}

async fn download_track(client: &reqwest::Client, stream_url: &str) -> anyhow::Result<Vec<u8>> {
    let resp = client.get(stream_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "stream download failed: {} for {}",
            resp.status(),
            redact(stream_url)
        );
    }
    Ok(resp.bytes().await?.to_vec())
}

// Server identity — the phone needs the server's machineIdentifier in every
// timeline so it knows which PMS the play queue lives on.
#[derive(Deserialize, Debug)]
struct IdentityContainer {
    #[serde(rename = "machineIdentifier")]
    machine_identifier: String,
}
#[derive(Deserialize, Debug)]
struct IdentityResponse {
    #[serde(rename = "MediaContainer")]
    media_container: IdentityContainer,
}

async fn fetch_server_machine_id(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
) -> anyhow::Result<String> {
    let r: IdentityResponse = plex_get_json(client, &format!("{server_url}/identity"), token).await?;
    Ok(r.media_container.machine_identifier)
}

// ---------- Background playback thread ----------
//
// Playback goes through the audio module (direct ALSA on Linux, cpal
// elsewhere).
// The thread owns the queue; web handlers send it commands over a channel
// and read its state back through SharedStatus.

struct QueuedTrack {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    key: String,
    rating_key: String,
    // Where the now playing view's "go to album/artist" links point.
    album_rating_key: Option<String>,
    artist_rating_key: Option<String>,
    artist_thumb: Option<String>,
    user_rating: Option<f32>,
    play_queue_item_id: Option<u64>,
    duration_ms: u64,
    thumb: Option<String>,
    // Kept (not consumed) while the track plays so seeking can restart the
    // decoder; cleared once the track is over.
    bytes: Option<Arc<[u8]>>,
}

// Where the queue came from, for timeline reporting. Everything except the
// machine fields is absent for playback started from the local web UI.
#[derive(Clone, Serialize, Default, Debug)]
struct QueueInfo {
    machine_identifier: String,
    address: String,
    port: u16,
    protocol: String,
    container_key: Option<String>,
    play_queue_id: Option<u64>,
    play_queue_version: Option<u64>,
}

enum PlayerCmd {
    PlayQueue {
        serial: u64,
        tracks: Vec<QueuedTrack>,
        start_index: usize,
        start_offset_ms: u64,
        info: QueueInfo,
        paused: bool,
    },
    // Swap the queue contents without interrupting playback (queue edits).
    ReplaceQueue {
        serial: u64,
        tracks: Vec<QueuedTrack>,
        info: QueueInfo,
    },
    SetBytes(usize, Arc<[u8]>),
    Pause,
    Resume,
    PlayPause,
    SkipOne,
    SkipPrevious,
    SkipTo(usize),
    Seek(u64),
    SetVolume(u8),
    SetRepeat(u8),
    SetShuffle(bool),
    // The player holds its own copy of each track, so a rating set from the
    // UI has to be reflected there or the next status poll undoes it.
    SetRating(String, f32),
    Stop,
}

// Plex's repeat vocabulary. Checked against Plexamp rather than assumed:
// 0 is off, 1 repeats the current track, 2 repeats the queue — not the
// 1-is-all ordering that gets quoted around.
const REPEAT_ONE: u8 = 1;
const REPEAT_ALL: u8 = 2;

#[derive(Clone, Serialize, Default)]
struct TrackInfo {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    key: String,
    rating_key: String,
    album_rating_key: Option<String>,
    artist_rating_key: Option<String>,
    artist_thumb: Option<String>,
    user_rating: Option<f32>,
    play_queue_item_id: Option<u64>,
    duration_ms: u64,
    thumb: Option<String>,
}

#[derive(Clone, Serialize, Default)]
struct PlayerStatus {
    queue: Vec<TrackInfo>,
    current_index: Option<usize>,
    paused: bool,
    // "playing" | "paused" | "stopped" — the Plex timeline vocabulary
    state: String,
    position_ms: u64,
    volume: u8,
    // 0 off, 1 this track, 2 the queue.
    repeat: u8,
    // Picks the next track at random from the queue instead of taking the
    // one after this. The queue itself keeps its order.
    shuffle: bool,
    info: QueueInfo,
    // Identifies which PlayQueue command this state belongs to, so a
    // prefetcher for an old queue knows to stop.
    queue_serial: u64,
    // Set while the player is stalled waiting for a track's audio bytes.
    waiting_for: Option<usize>,
    // Bumped on every change other than position; long-polls watch this.
    generation: u64,
}

type SharedStatus = Arc<Mutex<PlayerStatus>>;

fn set_status(
    status: &SharedStatus,
    queue: &[QueuedTrack],
    current_index: Option<usize>,
    paused: bool,
    volume: u8,
    info: &QueueInfo,
) {
    let mut s = status.lock().unwrap();
    s.queue = queue
        .iter()
        .map(|t| TrackInfo {
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            key: t.key.clone(),
            rating_key: t.rating_key.clone(),
            album_rating_key: t.album_rating_key.clone(),
            artist_rating_key: t.artist_rating_key.clone(),
            artist_thumb: t.artist_thumb.clone(),
            user_rating: t.user_rating,
            play_queue_item_id: t.play_queue_item_id,
            duration_ms: t.duration_ms,
            thumb: t.thumb.clone(),
        })
        .collect();
    s.current_index = current_index;
    s.paused = paused;
    s.state = match (current_index, paused) {
        (None, _) => "stopped",
        (Some(_), true) => "paused",
        (Some(_), false) => "playing",
    }
    .to_string();
    s.volume = volume;
    s.info = info.clone();
    s.waiting_for = None;
    if current_index.is_none() {
        s.position_ms = 0;
    }
    s.generation += 1;
}

fn set_waiting(status: &SharedStatus, index: usize) {
    let mut s = status.lock().unwrap();
    if s.waiting_for != Some(index) {
        s.waiting_for = Some(index);
    }
}

fn set_serial(status: &SharedStatus, serial: u64) {
    let mut s = status.lock().unwrap();
    s.queue_serial = serial;
}

// Separate from set_status so the repeat mode survives every ordinary status
// update without threading it through all of them.
fn set_repeat(status: &SharedStatus, repeat: u8) {
    let mut s = status.lock().unwrap();
    s.repeat = repeat;
    s.generation += 1;
}

fn set_shuffle(status: &SharedStatus, shuffle: bool) {
    let mut s = status.lock().unwrap();
    s.shuffle = shuffle;
    s.generation += 1;
}

// A random index, without pulling in a PRNG crate: uuid is already a
// dependency and its v4 generator is backed by the OS entropy source. The
// modulo bias is irrelevant for choosing a track.
fn rand_below(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    (Uuid::new_v4().as_u128() % n as u128) as usize
}

// Shuffle plays every track once before repeating any: pick at random from
// those not yet played this pass. Returns None when the pass is done and
// nothing should follow.
fn next_shuffled(
    len: usize,
    current: usize,
    played: &mut std::collections::HashSet<usize>,
    repeat: u8,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    if played.len() >= len {
        if repeat != REPEAT_ALL {
            return None;
        }
        played.clear();
    }
    // `current` is already in `played` mid-pass; the filter matters just after
    // the reset above, where it stops the track that has only just finished
    // from being the first pick of the new pass. A one-track queue is exempt,
    // since there is nothing else to play.
    let remaining: Vec<usize> = (0..len)
        .filter(|i| !(played.contains(i) || len > 1 && *i == current))
        .collect();
    remaining.get(rand_below(remaining.len())).copied()
}

fn set_position(status: &SharedStatus, position_ms: u64) {
    let mut s = status.lock().unwrap();
    s.position_ms = position_ms;
}

fn make_decoder(
    bytes: &Arc<[u8]>,
) -> anyhow::Result<rodio::Decoder<std::io::Cursor<Arc<[u8]>>>> {
    Ok(rodio::Decoder::new(std::io::Cursor::new(bytes.clone()))?)
}

// Stops output immediately, then rewinds the decoder to the last frame
// that was actually heard (ALSA reports how much was still queued), so
// resuming picks up exactly where the listener left off.
fn pause_now<F>(
    out: &mut Box<dyn audio::AudioOut>,
    source: &mut Box<dyn Iterator<Item = f32>>,
    frames_played: &mut u64,
    sample_rate: u64,
    make_source: &F,
) where
    F: Fn(u64) -> Option<Box<dyn Iterator<Item = f32>>>,
{
    let queued = out.queued_frames();
    out.discard();
    let heard = frames_played.saturating_sub(queued);
    if queued > 0 {
        if let Some(s) = make_source(heard * 1000 / sample_rate) {
            *source = s;
            *frames_played = heard;
        }
    }
}

// Applies a replacement queue. Returns the new index of the item that was
// at `current_index`, or None if it was removed.
fn merge_queue(
    queue: &mut Vec<QueuedTrack>,
    current_index: usize,
    mut new_tracks: Vec<QueuedTrack>,
) -> Option<usize> {
    let current_id = queue.get(current_index).and_then(|t| t.play_queue_item_id);
    // Carry audio bytes across by play queue item id.
    for nt in new_tracks.iter_mut() {
        if let Some(id) = nt.play_queue_item_id {
            if let Some(old) = queue.iter_mut().find(|o| o.play_queue_item_id == Some(id)) {
                nt.bytes = old.bytes.take();
            }
        }
    }
    let new_index = current_id
        .and_then(|id| new_tracks.iter().position(|t| t.play_queue_item_id == Some(id)));
    *queue = new_tracks;
    new_index
}

enum TrackOutcome {
    Ended,
    Next,
    Previous,
    JumpTo(usize),
    Stopped,
    Requeued,
}

fn spawn_player_thread(status: SharedStatus) -> std_mpsc::Sender<PlayerCmd> {
    use rodio::Source;

    let (tx, rx) = std_mpsc::channel::<PlayerCmd>();

    std::thread::spawn(move || {
        let mut queue: Vec<QueuedTrack> = Vec::new();
        let mut info = QueueInfo::default();
        let mut current_index: usize = 0;
        let mut paused = false;
        let mut volume: u8 = 100;
        let mut repeat: u8 = 0;
        let mut shuffle = false;
        // Indices already played this shuffle pass, so every track gets a turn
        // before any repeats. Reset whenever the queue itself changes.
        let mut played: std::collections::HashSet<usize> = Default::default();
        let mut pending_seek_ms: u64 = 0;

        'outer: loop {
            let idle = queue.is_empty() || current_index >= queue.len();
            let waiting_bytes = !idle && queue[current_index].bytes.is_none();

            if idle || waiting_bytes {
                let cmd = if idle {
                    rx.recv().map_err(|_| ())
                } else {
                    set_waiting(&status, current_index);
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(c) => Ok(c),
                        Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => Err(()),
                    }
                };
                match cmd {
                    Ok(PlayerCmd::PlayQueue {
                        serial,
                        tracks,
                        start_index,
                        start_offset_ms,
                        info: new_info,
                        paused: start_paused,
                    }) => {
                        queue = tracks;
                        info = new_info;
                        current_index = start_index;
                        pending_seek_ms = start_offset_ms;
                        paused = start_paused;
                        played.clear();
                        set_serial(&status, serial);
                        let idx = if current_index < queue.len() {
                            Some(current_index)
                        } else {
                            None
                        };
                        set_status(&status, &queue, idx, paused, volume, &info);
                    }
                    Ok(PlayerCmd::SetBytes(index, bytes)) => {
                        if let Some(t) = queue.get_mut(index) {
                            t.bytes = Some(bytes);
                        }
                    }
                    Ok(PlayerCmd::ReplaceQueue {
                        serial,
                        tracks,
                        info: new_info,
                    }) => {
                        let was_idle = idle;
                        let new_idx = merge_queue(&mut queue, current_index, tracks);
                        info = new_info;
                        played.clear();
                        if !was_idle {
                            current_index = new_idx.unwrap_or(current_index.min(queue.len()));
                        }
                        set_serial(&status, serial);
                        let idx = if !was_idle && current_index < queue.len() {
                            Some(current_index)
                        } else {
                            None
                        };
                        set_status(&status, &queue, idx, paused, volume, &info);
                    }
                    Ok(PlayerCmd::SetVolume(v)) => {
                        volume = v.min(100);
                        let idx = if idle { None } else { Some(current_index) };
                        set_status(&status, &queue, idx, paused, volume, &info);
                    }
                    Ok(PlayerCmd::SetRepeat(r)) => {
                        repeat = r.min(REPEAT_ALL);
                        set_repeat(&status, repeat);
                    }
                    Ok(PlayerCmd::SetShuffle(on)) => {
                        shuffle = on;
                        played.clear();
                        set_shuffle(&status, shuffle);
                    }
                    Ok(PlayerCmd::SetRating(rk, rating)) => {
                        for t in queue.iter_mut().filter(|t| t.rating_key == rk) {
                            t.user_rating = (rating >= 0.0).then_some(rating);
                        }
                        let idx = if idle { None } else { Some(current_index) };
                        set_status(&status, &queue, idx, paused, volume, &info);
                    }
                    Ok(PlayerCmd::Stop) => {
                        queue.clear();
                        current_index = 0;
                        set_status(&status, &queue, None, false, volume, &info);
                    }
                    Ok(PlayerCmd::SkipOne) if !idle => {
                        current_index += 1;
                        let idx = if current_index < queue.len() {
                            Some(current_index)
                        } else {
                            None
                        };
                        set_status(&status, &queue, idx, paused, volume, &info);
                    }
                    Ok(PlayerCmd::Pause) if !idle => {
                        paused = true;
                        set_status(&status, &queue, Some(current_index), true, volume, &info);
                    }
                    Ok(PlayerCmd::Resume) | Ok(PlayerCmd::PlayPause) if !idle => {
                        paused = false;
                        set_status(&status, &queue, Some(current_index), false, volume, &info);
                    }
                    Ok(_) => {}
                    Err(()) => break 'outer,
                }
                continue;
            }

            // ---- Play queue[current_index] ----
            played.insert(current_index);
            let bytes = queue[current_index].bytes.clone().unwrap();
            set_status(&status, &queue, Some(current_index), paused, volume, &info);

            let probe = match make_decoder(&bytes) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("decode error: {e}");
                    current_index += 1;
                    continue;
                }
            };
            let src_rate = probe.sample_rate();
            let src_channels = probe.channels();
            drop(probe);

            // The device may not accept the track's format (WASAPI shared
            // mode usually won't); it reports what it opened with instead.
            let (mut out, out_rate, out_channels) = match audio::open(src_rate, src_channels) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("could not open audio output: {e}");
                    // Don't silently walk the whole queue on a dead device.
                    queue.clear();
                    current_index = 0;
                    set_status(&status, &queue, None, false, volume, &info);
                    continue;
                }
            };

            let needs_resample = out_rate != src_rate || out_channels != src_channels;

            // Position counts frames handed to the device, so it uses the
            // device's rate and channel count.
            let mut frames_played: u64 = 0;
            let ch = out_channels.max(1) as u64;
            let sr = out_rate.max(1) as u64;

            // Builds the sample stream positioned at `start_ms`, resampled
            // to the device format when needed. Seeking rebuilds it rather
            // than trying to seek through the resampler.
            let make_source = |start_ms: u64| -> Option<Box<dyn Iterator<Item = f32>>> {
                let mut d = make_decoder(&bytes).ok()?;
                if start_ms > 0 && d.try_seek(Duration::from_millis(start_ms)).is_err() {
                    let skip = (start_ms * src_rate as u64 * src_channels.max(1) as u64) / 1000;
                    for _ in 0..skip {
                        if d.next().is_none() {
                            break;
                        }
                    }
                }
                if needs_resample {
                    Some(Box::new(rodio::source::UniformSourceIterator::new(
                        d,
                        out_channels,
                        out_rate,
                    )))
                } else {
                    Some(Box::new(d))
                }
            };

            let mut source = match make_source(pending_seek_ms) {
                Some(s) => s,
                None => {
                    eprintln!("could not start decoding");
                    current_index += 1;
                    continue;
                }
            };
            if pending_seek_ms > 0 {
                frames_played = pending_seek_ms * sr / 1000;
            }
            pending_seek_ms = 0;

            let mut buf = [0i16; 4096];
            let mut writes_since_status = 0u32;

            let outcome = 'track: loop {
                loop {
                    match rx.try_recv() {
                        Ok(PlayerCmd::Pause) => {
                            if !paused {
                                paused = true;
                                pause_now(&mut out, &mut source, &mut frames_played, sr, &make_source);
                                set_position(&status, frames_played * 1000 / sr);
                                set_status(&status, &queue, Some(current_index), true, volume, &info);
                            }
                        }
                        Ok(PlayerCmd::Resume) => {
                            if paused {
                                paused = false;
                                out.resume();
                                set_status(&status, &queue, Some(current_index), false, volume, &info);
                            }
                        }
                        Ok(PlayerCmd::PlayPause) => {
                            paused = !paused;
                            if paused {
                                pause_now(&mut out, &mut source, &mut frames_played, sr, &make_source);
                                set_position(&status, frames_played * 1000 / sr);
                            } else {
                                out.resume();
                            }
                            set_status(&status, &queue, Some(current_index), paused, volume, &info);
                        }
                        Ok(PlayerCmd::SkipOne) => break 'track TrackOutcome::Next,
                        Ok(PlayerCmd::SkipPrevious) => {
                            // Plex convention: restart the track if we're past
                            // a few seconds in, otherwise go to the previous one.
                            let pos_ms = frames_played * 1000 / sr;
                            if pos_ms > 3000 || current_index == 0 {
                                out.discard();
                                out.resume();
                                if let Some(s) = make_source(0) {
                                    source = s;
                                    frames_played = 0;
                                    set_position(&status, 0);
                                }
                            } else {
                                break 'track TrackOutcome::Previous;
                            }
                        }
                        Ok(PlayerCmd::SkipTo(i)) => break 'track TrackOutcome::JumpTo(i),
                        Ok(PlayerCmd::Seek(ms)) => {
                            out.discard();
                            out.resume();
                            if let Some(s) = make_source(ms) {
                                source = s;
                                frames_played = ms * sr / 1000;
                                set_position(&status, ms);
                            }
                        }
                        Ok(PlayerCmd::SetVolume(v)) => {
                            volume = v.min(100);
                            set_status(&status, &queue, Some(current_index), paused, volume, &info);
                        }
                        Ok(PlayerCmd::SetRepeat(r)) => {
                            repeat = r.min(REPEAT_ALL);
                            set_repeat(&status, repeat);
                        }
                        Ok(PlayerCmd::SetShuffle(on)) => {
                            shuffle = on;
                            played.clear();
                            set_shuffle(&status, shuffle);
                        }
                        Ok(PlayerCmd::SetRating(rk, rating)) => {
                            for t in queue.iter_mut().filter(|t| t.rating_key == rk) {
                                t.user_rating = (rating >= 0.0).then_some(rating);
                            }
                            set_status(&status, &queue, Some(current_index), paused, volume, &info);
                        }
                        Ok(PlayerCmd::Stop) => break 'track TrackOutcome::Stopped,
                        Ok(PlayerCmd::SetBytes(index, b)) => {
                            if let Some(t) = queue.get_mut(index) {
                                t.bytes = Some(b);
                            }
                        }
                        Ok(PlayerCmd::ReplaceQueue {
                            serial,
                            tracks,
                            info: new_info,
                        }) => {
                            // Keep our own bytes so the merge can't drop the
                            // track that's playing right now.
                            let keep = queue.get_mut(current_index).and_then(|t| t.bytes.take());
                            let new_idx = merge_queue(&mut queue, current_index, tracks);
                            info = new_info;
                            played.clear();
                            set_serial(&status, serial);
                            match new_idx {
                                Some(i) => {
                                    current_index = i;
                                    if let Some(t) = queue.get_mut(i) {
                                        t.bytes = keep;
                                    }
                                    set_status(&status, &queue, Some(i), paused, volume, &info);
                                }
                                None => {
                                    // The playing track was removed: move on.
                                    if queue.is_empty() {
                                        break 'track TrackOutcome::Stopped;
                                    }
                                    break 'track TrackOutcome::JumpTo(
                                        current_index.min(queue.len() - 1),
                                    );
                                }
                            }
                        }
                        Ok(PlayerCmd::PlayQueue {
                            serial,
                            tracks,
                            start_index,
                            start_offset_ms,
                            info: new_info,
                            paused: start_paused,
                        }) => {
                            queue = tracks;
                            info = new_info;
                            current_index = start_index;
                            pending_seek_ms = start_offset_ms;
                            paused = start_paused;
                            played.clear();
                            set_serial(&status, serial);
                            break 'track TrackOutcome::Requeued;
                        }
                        Err(std_mpsc::TryRecvError::Empty) => break,
                        Err(std_mpsc::TryRecvError::Disconnected) => {
                            break 'track TrackOutcome::Stopped
                        }
                    }
                }

                if paused {
                    std::thread::sleep(Duration::from_millis(20));
                    continue 'track;
                }

                let gain = {
                    let v = volume as f32 / 100.0;
                    v * v
                };
                let mut n = 0;
                while n < buf.len() {
                    match source.next() {
                        Some(s) => {
                            buf[n] = ((s * gain).clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                            n += 1;
                        }
                        None => break,
                    }
                }
                if n == 0 {
                    break 'track TrackOutcome::Ended;
                }
                if let Err(e) = out.write(&buf[..n]) {
                    eprintln!("audio write error: {e}");
                    break 'track TrackOutcome::Stopped;
                }
                frames_played += (n as u64) / ch;
                writes_since_status += 1;
                if writes_since_status >= 5 {
                    writes_since_status = 0;
                    set_position(&status, frames_played * 1000 / sr);
                }
            };

            if matches!(outcome, TrackOutcome::Ended) {
                out.drain();
            } else {
                out.discard();
            }

            // Repeat-one replays this index, so its audio has to survive —
            // otherwise the player would stall waiting for the prefetcher to
            // fetch a track it just finished. An explicit skip still moves
            // on: repeat-one governs what happens when a track runs out,
            // not what the listener asked for.
            let replaying = matches!(outcome, TrackOutcome::Ended) && repeat == REPEAT_ONE;

            // Free this track's audio now that it's done (or abandoned).
            if let Some(t) = queue.get_mut(current_index) {
                if !matches!(outcome, TrackOutcome::Requeued) && !replaying {
                    t.bytes = None;
                }
            }

            match outcome {
                TrackOutcome::Ended if replaying => pending_seek_ms = 0,
                TrackOutcome::Ended | TrackOutcome::Next => {
                    if shuffle {
                        // Any track in the queue, not the one that happens to
                        // sit next in it. queue.len() parks the player on the
                        // idle path, the same as running off the end.
                        current_index = next_shuffled(
                            queue.len(),
                            current_index,
                            &mut played,
                            repeat,
                        )
                        .unwrap_or(queue.len());
                    } else {
                        current_index += 1;
                        // Repeat-all wraps rather than falling off the end.
                        if repeat == REPEAT_ALL && !queue.is_empty() && current_index >= queue.len()
                        {
                            current_index = 0;
                        }
                    }
                }
                TrackOutcome::Previous => current_index = current_index.saturating_sub(1),
                TrackOutcome::JumpTo(i) => current_index = i,
                TrackOutcome::Stopped => {
                    queue.clear();
                    current_index = 0;
                    paused = false;
                }
                TrackOutcome::Requeued => {}
            }
            let idx = if current_index < queue.len() {
                Some(current_index)
            } else {
                None
            };
            set_status(&status, &queue, idx, paused, volume, &info);
        }
    });

    tx
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>RustAmp</title>
<style>
  :root {
    --bg: #0b0b0d;
    --panel: #161618;
    --panel-2: #1f1f22;
    --text: #f2f2f2;
    --muted: #9a9aa0;
    --accent: #ff8c42;
    --accent-2: #ffb27a;
    --radius: 10px;
  }
  * { box-sizing: border-box; }
  html, body { height: 100%; margin: 0; }
  body {
    background: var(--bg);
    color: var(--text);
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    display: flex; flex-direction: column;
  }
  header {
    display: flex; align-items: center; gap: 14px;
    padding: 14px 20px;
    background: linear-gradient(180deg, #121214, var(--bg));
    position: sticky; top: 0; z-index: 5;
  }
  .wordmark { font-weight: 800; letter-spacing: .5px; font-size: 18px; }
  .wordmark span { color: var(--accent); }
  #crumbs { display: flex; gap: 6px; align-items: center; color: var(--muted); font-size: 14px; flex-wrap: wrap; }
  #crumbs a { color: var(--muted); cursor: pointer; text-decoration: none; }
  #crumbs a:hover { color: var(--text); }
  #crumbs .sep { opacity: .5; }
  #crumbs .here { color: var(--text); }

  main { flex: 1; overflow-y: auto; padding: 8px 20px 120px; }

  .hero {
    display: flex; gap: 22px; align-items: flex-end; margin: 10px 0 22px;
  }
  .hero img {
    width: 180px; height: 180px; object-fit: cover; border-radius: var(--radius);
    box-shadow: 0 12px 40px rgba(0,0,0,.6); background: var(--panel-2);
  }
  .hero h1 { margin: 0 0 6px; font-size: 32px; }
  .hero .sub { color: var(--muted); }
  .hero .actions { margin-top: 14px; }
  .btn {
    background: var(--accent); color: #111; border: 0; border-radius: 999px;
    padding: 10px 18px; font-weight: 700; cursor: pointer; font-size: 14px;
  }
  .btn:hover { background: var(--accent-2); }

  .grid {
    display: grid; gap: 18px;
    grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
  }
  .card { cursor: pointer; }
  .card .art {
    width: 100%; aspect-ratio: 1; object-fit: cover;
    border-radius: var(--radius); background: var(--panel-2);
    transition: transform .15s ease;
  }
  .card.round .art { border-radius: 50%; }
  .card:hover .art { transform: scale(1.03); }
  .card .t { margin-top: 8px; font-weight: 600; font-size: 14px;
             white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .card .s { color: var(--muted); font-size: 12px;
             white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }

  .tracks { list-style: none; margin: 0; padding: 0; }
  .tracks li {
    display: grid; grid-template-columns: 36px 1fr auto; gap: 12px;
    align-items: center; padding: 10px 12px; border-radius: 8px; cursor: pointer;
  }
  .tracks li:hover { background: var(--panel); }
  .tracks li.now { color: var(--accent); }
  .tracks .n { color: var(--muted); text-align: right; font-variant-numeric: tabular-nums; }
  .tracks li.now .n { color: var(--accent); }
  .tracks .d { color: var(--muted); font-variant-numeric: tabular-nums; font-size: 13px; }

  .empty { color: var(--muted); padding: 40px 0; text-align: center; }
  .busy { color: var(--muted); padding: 12px 0; font-size: 14px; }

  /* Now playing view */
  #npv { display: none; }
  #npv.on { display: flex; gap: 40px; align-items: center; padding: 20px 0 40px; flex-wrap: wrap; }
  #npv .cover, #npv .cover img {
    width: 340px; height: 340px; border-radius: var(--radius); flex: none;
    box-shadow: 0 20px 60px rgba(0,0,0,.65); background: var(--panel-2);
  }
  #npv .cover img { display: block; object-fit: cover; box-shadow: none; }
  #npv .cover.ph { display: grid; place-items: center; font-size: 110px; color: #4a4a52; }
  #npv .info { min-width: 0; flex: 1; }
  #npv .sub { color: var(--muted); font-size: 13px; letter-spacing: .12em; text-transform: uppercase; }
  #npv h1 { margin: 10px 0 14px; font-size: 40px; line-height: 1.1; }
  #npv .meta { font-size: 17px; margin-bottom: 6px; }
  #npv .meta a { color: var(--accent-2); cursor: pointer; text-decoration: none; }
  #npv .meta a:hover { text-decoration: underline; }
  #npv .meta .muted { color: var(--muted); }
  #npv .empty { color: var(--muted); }
  #npv .stars { display: flex; gap: 4px; margin: 16px 0 4px; }
  #npv .stars button {
    background: none; border: 0; padding: 0; cursor: pointer; line-height: 1;
    font-size: 26px; color: #4a4a52;
  }
  #npv .stars button.lit { color: var(--accent); }
  #npv .stars button.pre { color: var(--accent-2); }
  #npv .lyrics {
    margin-top: 22px; max-height: 260px; overflow-y: auto;
    border-top: 1px solid #2a2a2e; padding-top: 14px;
  }
  #npv .lyrics .ln { color: var(--muted); padding: 3px 0; font-size: 15px; }
  #npv .lyrics .ln.on { color: var(--text); font-weight: 600; }
  #npv .lyrics .by { color: #5a5a62; font-size: 12px; margin-top: 10px; }
  @media (max-width: 720px) {
    #npv.on { gap: 20px; }
    #npv .cover, #npv .cover img { width: 200px; height: 200px; }
    #npv h1 { font-size: 26px; }
  }

  /* Artwork placeholder, for items the library has no image for */
  .ph { display: grid; place-items: center; background: var(--panel-2); color: #4a4a52; }
  .art.ph { font-size: 38px; }
  .hero .ph {
    width: 180px; height: 180px; border-radius: var(--radius); font-size: 56px;
    box-shadow: 0 12px 40px rgba(0,0,0,.6); flex: none;
  }
  .qi .ph { width: 40px; height: 40px; border-radius: 4px; font-size: 18px; }

  /* Section view switcher */
  .viewbar { display: flex; align-items: center; gap: 10px; margin: 6px 0 18px; }
  .tabs { display: flex; gap: 6px; }
  .tab {
    background: var(--panel-2); color: var(--muted); border: 0; border-radius: 999px;
    padding: 7px 14px; font-size: 13px; cursor: pointer;
  }
  .tab:hover { color: var(--text); }
  .tab.on { background: var(--accent); color: #111; font-weight: 700; }
  .viewbar .btn { margin-left: auto; }

  /* Search */
  #q {
    margin-left: auto; flex: none; width: 240px; max-width: 40vw;
    background: var(--panel-2); border: 1px solid #2a2a2e; color: var(--text);
    border-radius: 999px; padding: 8px 14px; font-size: 14px; outline: none;
  }
  #q:focus { border-color: var(--accent); }
  #q::placeholder { color: var(--muted); }
  h2.sec { font-size: 15px; color: var(--muted); font-weight: 600; margin: 24px 0 10px; }
  h2.sec:first-child { margin-top: 4px; }
  .tracks .sub2 { color: var(--muted); font-weight: 400; }

  /* Now playing bar */
  #np {
    position: fixed; left: 0; right: 0; bottom: 0; z-index: 10;
    background: rgba(22,22,24,.97); backdrop-filter: blur(12px);
    border-top: 1px solid #2a2a2e;
    display: grid; grid-template-columns: 1fr auto 1fr; align-items: center;
    gap: 16px; padding: 10px 20px;
  }
  #np .meta { display: flex; align-items: center; gap: 12px; min-width: 0; }
  #np .meta .cover {
    width: 56px; height: 56px; border-radius: 6px; flex: none;
    background: var(--panel-2); display: grid; place-items: center;
    color: #555; font-size: 22px; position: relative;
  }
  /* The placeholder glyph is a bare text node, so it is an anonymous grid
     item. Leaving the image in grid flow puts the two in separate rows and
     squashes the artwork into the lower half; take the image out of flow so
     it covers the tile instead. */
  #np .meta .cover img {
    position: absolute; inset: 0; width: 100%; height: 100%;
    border-radius: 6px; object-fit: cover; display: none;
  }
  #np .meta .cover.has-art img { display: block; }
  #np .meta .cover.has-art { color: transparent; }
  #np .meta .t { font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  #np .meta .a { color: var(--muted); font-size: 13px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  #np .center { display: flex; flex-direction: column; align-items: center; gap: 6px; min-width: 320px; }
  #np .ctl { display: flex; align-items: center; gap: 18px; }
  .ib {
    background: none; border: 0; color: var(--text); cursor: pointer; padding: 4px;
    display: inline-flex; align-items: center; justify-content: center;
  }
  .ib svg { width: 22px; height: 22px; fill: currentColor; }
  .ib.big { background: var(--text); color: #111; border-radius: 50%; width: 40px; height: 40px; }
  .ib.big svg { width: 20px; height: 20px; }
  .ib:hover { color: var(--accent-2); }
  .ib.big:hover { background: var(--accent-2); color: #111; }
  .prog { display: flex; align-items: center; gap: 10px; width: 100%; font-size: 12px; color: var(--muted); font-variant-numeric: tabular-nums; }
  .bar { flex: 1; height: 4px; background: #333; border-radius: 2px; position: relative; cursor: pointer; }
  .bar .fill { height: 100%; background: var(--accent); border-radius: 2px; width: 0%; }
  #np .right { display: flex; justify-content: flex-end; align-items: center; gap: 10px; }
  input[type=range] { accent-color: var(--accent); width: 110px; }
  .tracks li { grid-template-columns: 36px 1fr auto auto; }
  .tracks .acts { display: none; gap: 4px; }
  .tracks li:hover .acts { display: flex; }
  @media (hover: none) { .tracks .acts { display: flex; } }
  .mini {
    background: var(--panel-2); color: var(--text); border: 0; border-radius: 6px;
    padding: 4px 8px; font-size: 12px; cursor: pointer;
  }
  .mini:hover { background: var(--accent); color: #111; }
  .btn.ghost { background: transparent; color: var(--text); border: 1px solid #444; margin-left: 8px; }
  .btn.ghost:hover { border-color: var(--accent); color: var(--accent-2); background: transparent; }

  /* Queue drawer */
  #qd {
    position: fixed; top: 0; right: 0; bottom: 92px; width: 360px; max-width: 100%;
    background: var(--panel); border-left: 1px solid #2a2a2e; z-index: 9;
    transform: translateX(100%); transition: transform .2s ease;
    display: flex; flex-direction: column;
  }
  #qd.open { transform: none; }
  #qd header { position: static; background: none; padding: 16px 18px; justify-content: space-between; }
  #qd h2 { margin: 0; font-size: 16px; }
  #qd .list { overflow-y: auto; flex: 1; padding: 0 8px 12px; }
  .qi {
    display: grid; grid-template-columns: 40px 1fr auto; gap: 10px; align-items: center;
    padding: 8px; border-radius: 8px; cursor: pointer;
  }
  .qi:hover { background: var(--panel-2); }
  .qi img { width: 40px; height: 40px; border-radius: 4px; object-fit: cover; background: var(--panel-2); }
  .qi .t { font-size: 14px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .qi .a { font-size: 12px; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .qi.now .t { color: var(--accent); }
  .qi .x { background: none; border: 0; color: var(--muted); cursor: pointer; font-size: 18px; padding: 4px 8px; }
  .qi .x:hover { color: var(--accent-2); }
  .ib.on { color: var(--accent); }
  #toast {
    position: fixed; left: 50%; bottom: 110px; transform: translateX(-50%) translateY(20px);
    background: #3a1f1f; color: #ffd9d9; border: 1px solid #7a2e2e; border-radius: 8px;
    padding: 10px 14px; font-size: 13px; max-width: 80%; opacity: 0; transition: all .2s; z-index: 20;
  }
  #toast.show { opacity: 1; transform: translateX(-50%); }

  @media (max-width: 720px) {
    #np { grid-template-columns: 1fr; }
    #qd { bottom: 150px; }
    #np .center { min-width: 0; }
    #np .right { display: none; }
    .hero { flex-direction: column; align-items: flex-start; }
  }
</style>
</head>
<body>
<header>
  <div class="wordmark">Rust<span>Amp</span></div>
  <nav id="crumbs"></nav>
  <input id="q" type="search" placeholder="Search" autocomplete="off" spellcheck="false"
         oninput="onSearchInput(this.value)"
         onkeydown="if(event.key==='Escape'){this.value='';onSearchInput('')}">
</header>

<main>
  <div id="npv"></div>
  <div id="hero"></div>
  <div id="content"></div>
</main>

<div id="qd">
  <header><h2>Up next</h2><button class="ib" onclick="toggleQueue()" title="Close"><svg viewBox="0 0 24 24"><path d="M18.3 5.7 12 12l6.3 6.3-1.4 1.4L10.6 13.4 4.3 19.7 2.9 18.3 9.2 12 2.9 5.7l1.4-1.4 6.3 6.3 6.3-6.3z"/></svg></button></header>
  <div class="list" id="qlist"></div>
</div>

<div id="np">
  <div class="meta">
    <div class="cover" id="np-cover">♫<img id="np-art" alt=""></div>
    <div style="min-width:0">
      <div class="t" id="np-title">Nothing playing</div>
      <div class="a" id="np-artist"></div>
    </div>
  </div>
  <div class="center">
    <div class="ctl">
      <button class="ib" id="sh-btn" onclick="toggleShuffle()" title="Shuffle">
        <svg viewBox="0 0 24 24"><path d="M17 3l4 4-4 4V8h-2.2l-2.3 3.2 2.3 3.2H17v-3l4 4-4 4v-3h-3.2l-2.6-3.6L8.6 16H3v-2h4.6l2.6-3.6L7.6 8H3V6h5.6l2.6 3.6L13.8 6H17V3z"/></svg>
      </button>
      <button class="ib" onclick="post('/api/prev')" title="Previous">
        <svg viewBox="0 0 24 24"><path d="M6 6h2v12H6zm3.5 6 8.5 6V6z"/></svg>
      </button>
      <button class="ib big" id="np-play" onclick="post('/api/playpause')" title="Play/Pause">
        <svg id="ic-play" viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>
        <svg id="ic-pause" viewBox="0 0 24 24" style="display:none"><path d="M6 5h4v14H6zm8 0h4v14h-4z"/></svg>
      </button>
      <button class="ib" onclick="post('/api/skip')" title="Next">
        <svg viewBox="0 0 24 24"><path d="M16 6h2v12h-2zM6 18l8.5-6L6 6z"/></svg>
      </button>
      <button class="ib" id="npv-btn" onclick="toggleNowPlaying()" title="Now playing">
        <svg viewBox="0 0 24 24"><path d="M12 3v10.55A4 4 0 1 0 14 17V7h4V3h-6z"/></svg>
      </button>
      <button class="ib" id="rp-btn" onclick="cycleRepeat()" title="Repeat">
        <svg id="ic-rp" viewBox="0 0 24 24"><path d="M7 7h10v3l4-4-4-4v3H5v6h2V7zm10 10H7v-3l-4 4 4 4v-3h12v-6h-2v4z"/></svg>
        <svg id="ic-rp1" viewBox="0 0 24 24" style="display:none"><path d="M7 7h10v3l4-4-4-4v3H5v6h2V7zm10 10H7v-3l-4 4 4 4v-3h12v-6h-2v4zm-5-6h-1.5l-2 1v1.5l1.5-.7V16h2v-5z"/></svg>
      </button>
      <button class="ib" id="qbtn" onclick="toggleQueue()" title="Queue">
        <svg viewBox="0 0 24 24"><path d="M3 6h12v2H3zm0 5h12v2H3zm0 5h8v2H3zm14-6v6.3a2.5 2.5 0 1 0 2 2.45V12h3v-2h-5z"/></svg>
      </button>
    </div>
    <div class="prog">
      <span id="np-pos">0:00</span>
      <div class="bar" id="np-bar" onclick="seekClick(event)"><div class="fill" id="np-fill"></div></div>
      <span id="np-dur">0:00</span>
    </div>
  </div>
  <div class="right">
    <svg class="ib" viewBox="0 0 24 24" style="width:20px;height:20px;fill:var(--muted)"><path d="M3 9v6h4l5 5V4L7 9zm13.5 3A4.5 4.5 0 0 0 14 8v8a4.5 4.5 0 0 0 2.5-4z"/></svg>
    <input type="range" id="np-vol" min="0" max="100" value="100" oninput="setVol(this.value)">
    <button class="ib" onclick="post('/api/stop')" title="Stop">
      <svg viewBox="0 0 24 24"><path d="M6 6h12v12H6z"/></svg>
    </button>
  </div>
</div>

<script>
const $ = id => document.getElementById(id);
const thumb = (path, size) => path ? `/api/thumb?path=${encodeURIComponent(path)}&size=${size||300}` : "";

// Throttled artwork loader: browsers allow ~6 connections per host, and a
// grid of 200 covers would otherwise starve the API calls behind them.
// Only images near the viewport are requested, at most 3 at a time.
const artQueue = []; let artActive = 0; const ART_MAX = 3;
const artObserver = new IntersectionObserver((entries) => {
  for (const en of entries) if (en.isIntersecting) { artObserver.unobserve(en.target); artQueue.push(en.target); }
  pumpArt();
}, { rootMargin: "200px" });
function pumpArt() {
  while (artActive < ART_MAX && artQueue.length) {
    const img = artQueue.shift();
    if (!img.isConnected) continue;
    artActive++;
    const done = () => { artActive--; pumpArt(); };
    img.onload = done;
    // A thumb the server no longer has would otherwise stay a broken icon.
    img.onerror = () => {
      const ph = document.createElement("div");
      ph.className = `${img.className} ph`;
      ph.innerHTML = "&#9835;";
      img.replaceWith(ph);
      done();
    };
    img.src = img.dataset.src;
  }
}
function lazyArt(root) {
  root.querySelectorAll("img[data-src]").forEach(img => artObserver.observe(img));
}
// An <img> whose src resolves to nothing draws the browser's broken-image
// icon, so anything the library has no artwork for gets a placeholder tile.
function art(path, size, cls, lazy) {
  const c = cls || "";
  if (!path) return `<div class="${c} ph">&#9835;</div>`;
  const src = thumb(path, size);
  return lazy ? `<img class="${c}" data-src="${src}">` : `<img class="${c}" src="${src}">`;
}
// A track's own artist when it has one; grandparentTitle is the *album*
// artist, which on a compilation reads "Various Artists".
const trackArtist = t => t.originalTitle || t.grandparentTitle || "";
const fmt = ms => { const s = Math.floor((ms||0)/1000); return `${Math.floor(s/60)}:${String(s%60).padStart(2,"0")}`; };
const post = async (url) => {
  try {
    const r = await fetch(url, { method: "POST" });
    if (!r.ok) toast(`${r.status}: ${(await r.text()).slice(0, 200)}`);
  } catch (e) { toast(String(e)); }
};
let toastTimer;
function toast(msg) {
  let t = $("toast");
  if (!t) { t = document.createElement("div"); t.id = "toast"; document.body.appendChild(t); }
  t.textContent = msg; t.classList.add("show");
  clearTimeout(toastTimer); toastTimer = setTimeout(() => t.classList.remove("show"), 5000);
}

// Listings run to megabytes and barely change within a session, so keep what
// we have already parsed. Walking back up the breadcrumbs should cost nothing.
const listings = new Map();
const LISTING_TTL = 5 * 60 * 1000;
async function listing(url) {
  const hit = listings.get(url);
  if (hit && Date.now() - hit.at < LISTING_TTL) return hit.data;
  const data = await (await fetch(url)).json();
  listings.set(url, { at: Date.now(), data });
  return data;
}

// Says "loading" only once the wait is long enough to notice, so a cached
// view doesn't flash it.
let busyTimer;
function setBusy(on) {
  clearTimeout(busyTimer);
  const existing = $("busy");
  if (!on) { if (existing) existing.remove(); return; }
  if (existing) return;
  busyTimer = setTimeout(() => {
    if ($("busy")) return;
    $("content").prepend(el(`<div class="busy" id="busy">Loading…</div>`));
  }, 150);
}

let path = [];            // breadcrumb stack: {label, load: () => Promise}
let currentAlbumKey = null;
let nowRatingKey = null;

function renderCrumbs() {
  const c = $("crumbs"); c.innerHTML = "";
  path.forEach((p, i) => {
    if (i) { const s = document.createElement("span"); s.className = "sep"; s.textContent = "›"; c.appendChild(s); }
    const a = document.createElement("a");
    a.textContent = p.label;
    if (i === path.length - 1) a.className = "here";
    a.onclick = () => { path = path.slice(0, i + 1); renderCrumbs(); navId++; p.load(); };
    c.appendChild(a);
  });
}
let navId = 0;
function go(label, load) { path.push({ label, load }); renderCrumbs(); navId++; return load(); }
// Any load that awaited past a newer navigation must not touch the page.
function stale(my) { return my !== navId; }

function setHero(html) { $("hero").innerHTML = html || ""; }

async function showSections() {
  const my = navId;
  setHero("");
  const items = await listing("/api/sections");
  if (stale(my)) return;
  if (items.length === 1) return go(items[0].title, () => showSection(items[0].key));
  const g = document.createElement("div"); g.className = "grid";
  items.forEach(s => {
    const card = el(`<div class="card"><div class="art" style="display:grid;place-items:center;font-size:40px">♫</div><div class="t">${esc(s.title)}</div><div class="s">Music library</div></div>`);
    card.onclick = () => go(s.title, () => showSection(s.key));
    g.appendChild(card);
  });
  swap(g);
}

// Which of the three ways into the library is showing. Kept outside the
// function so the breadcrumb returns you to the view you were in.
let sectionView = "artists";

async function showSection(key, view) {
  if (view) sectionView = view;
  const my = ++navId;
  const load = { artists: sectionArtists, albums: sectionAlbums, songs: sectionSongs }[sectionView];
  let body;
  setBusy(true);
  // Leave the previous view on screen while this loads, rather than blanking
  // it and showing nothing for seconds.
  try { body = await load(key); }
  catch (e) { setBusy(false); return toast(String(e)); }
  finally { setBusy(false); }
  if (stale(my)) return;
  setHero("");

  const bar = el(`<div class="viewbar"><div class="tabs">
      <button class="tab" data-v="artists">Artists</button>
      <button class="tab" data-v="albums">Albums</button>
      <button class="tab" data-v="songs">Songs</button>
    </div><button class="btn ghost">Shuffle everything</button></div>`);
  bar.querySelectorAll(".tab").forEach(b => {
    b.classList.toggle("on", b.dataset.v === sectionView);
    b.onclick = () => showSection(key, b.dataset.v);
  });
  bar.querySelector(".btn").onclick = () => post(`/api/shuffle-library/${key}`);

  const box = document.createElement("div");
  box.appendChild(bar); box.appendChild(body);
  swap(box);
  markNow();
}

async function sectionArtists(key) {
  const items = (await listing(`/api/sections/${key}`)).MediaContainer.Metadata || [];
  const g = document.createElement("div"); g.className = "grid";
  items.forEach(a => {
    // Genre rather than the type, which just said "artist" on every card.
    // PMS sends it on the section listing (excludeFields=summary keeps it),
    // and it is blank for artists that have none.
    const genre = ((a.Genre || [])[0] || {}).tag || "";
    const card = el(`<div class="card round">${art(a.thumb, 300, "art", true)}<div class="t">${esc(a.title)}</div><div class="s">${esc(genre)}</div></div>`);
    card.onclick = () => go(a.title, () => showArtist(a));
    g.appendChild(card);
  });
  return g;
}

async function sectionAlbums(key) {
  const items = (await listing(`/api/sections/${key}/albums`)).MediaContainer.Metadata || [];
  const g = document.createElement("div"); g.className = "grid";
  items.forEach(al => {
    const card = el(`<div class="card">${art(al.thumb, 300, "art", true)}<div class="t">${esc(al.title)}</div><div class="s">${esc(al.parentTitle || "")}</div></div>`);
    card.onclick = () => go(al.title, () => showAlbum(al));
    g.appendChild(card);
  });
  return g;
}

async function sectionSongs(key) {
  const items = (await listing(`/api/sections/${key}/tracks`)).MediaContainer.Metadata || [];
  const ul = document.createElement("ul"); ul.className = "tracks";
  items.forEach(t => {
    const li = el(`<li data-rk="${t.ratingKey}"><span class="n">&#9834;</span>
      <span class="t">${esc(t.title)}<span class="sub2"> &mdash; ${esc(trackArtist(t))}</span></span>
      <span class="acts">
        <button class="mini" data-act="next" title="Play next">Next</button>
        <button class="mini" data-act="add" title="Add to queue">+</button>
      </span><span class="d">${fmt(t.duration)}</span></li>`);
    li.onclick = (e) => {
      const act = e.target.dataset && e.target.dataset.act;
      if (act === "next") { e.stopPropagation(); return post(`/api/queue/add?rating_key=${t.ratingKey}&next=1`); }
      if (act === "add")  { e.stopPropagation(); return post(`/api/queue/add?rating_key=${t.ratingKey}`); }
      post(`/api/play/${t.ratingKey}`);
    };
    ul.appendChild(li);
  });
  return ul;
}

async function showArtist(artist) {
  const my = navId;
  // An artist rating key seeds a play queue just like an album one does.
  setHero(`<div class="hero">${art(artist.thumb, 400, "", false)}<div>
    <div class="sub">Artist</div><h1>${esc(artist.title)}</h1>
    <div class="actions">
      <button class="btn" onclick="post('/api/play-album/${artist.ratingKey}')">&#9654; Play all</button>
      <button class="btn ghost" onclick="post('/api/play-album/${artist.ratingKey}?shuffle=1')">Shuffle</button>
    </div></div></div>`);
  const albums = (await listing(`/api/browse/${artist.ratingKey}`)).MediaContainer.Metadata || [];
  if (stale(my)) return;
  const g = document.createElement("div"); g.className = "grid";
  albums.forEach(al => {
    const card = el(`<div class="card">${art(al.thumb, 300, "art", true)}<div class="t">${esc(al.title)}</div><div class="s">${al.year || ""}</div></div>`);
    card.onclick = () => go(al.title, () => showAlbum(al));
    g.appendChild(card);
  });
  swap(g);
}

async function showAlbum(album) {
  const my = navId;
  currentAlbumKey = album.ratingKey;
  setHero(`<div class="hero">${art(album.thumb, 400, "", false)}<div>
    <div class="sub">${esc(album.parentTitle || "")}</div><h1>${esc(album.title)}</h1>
    <div class="sub">${album.year || ""}</div>
    <div class="actions">
      <button class="btn" onclick="post('/api/play-album/${album.ratingKey}')">▶ Play</button>
      <button class="btn ghost" onclick="post('/api/play-album/${album.ratingKey}?shuffle=1')">Shuffle</button>
      <button class="btn ghost" onclick="post('/api/queue/add?rating_key=${album.ratingKey}&next=1')">Play next</button>
      <button class="btn ghost" onclick="post('/api/queue/add?rating_key=${album.ratingKey}')">Add to queue</button>
    </div>
  </div></div>`);
  const tracks = (await listing(`/api/browse/${album.ratingKey}`)).MediaContainer.Metadata || [];
  if (stale(my)) return;
  const ul = document.createElement("ul"); ul.className = "tracks";
  tracks.forEach((t, i) => {
    const li = el(`<li data-rk="${t.ratingKey}"><span class="n">${t.index ?? i + 1}</span><span class="t">${esc(t.title)}</span>
      <span class="acts">
        <button class="mini" data-act="next" title="Play next">Next</button>
        <button class="mini" data-act="add" title="Add to queue">+</button>
      </span><span class="d">${fmt(t.duration)}</span></li>`);
    li.onclick = (e) => {
      const act = e.target.dataset && e.target.dataset.act;
      if (act === "next") { e.stopPropagation(); return post(`/api/queue/add?rating_key=${t.ratingKey}&next=1`); }
      if (act === "add")  { e.stopPropagation(); return post(`/api/queue/add?rating_key=${t.ratingKey}`); }
      post(`/api/play-album/${album.ratingKey}?start=${i}`);
    };
    ul.appendChild(li);
  });
  swap(ul);
  markNow();
}

function swap(node) {
  // Artwork queued for the view being replaced is dead weight; pumpArt skips
  // detached images but the queue would still be walked.
  artQueue.length = 0;
  const c = $("content"); c.innerHTML = ""; c.appendChild(node); lazyArt(c);
}
function el(html) { const t = document.createElement("template"); t.innerHTML = html.trim(); return t.content.firstChild; }
function esc(s) { return String(s ?? "").replace(/[&<>"]/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}[c])); }
function markNow() {
  document.querySelectorAll(".tracks li").forEach(li => li.classList.toggle("now", li.dataset.rk === nowRatingKey));
}

// ---- search ----
let searchTimer, lastQuery = "", searching = false;

function onSearchInput(v) {
  clearTimeout(searchTimer);
  const q = v.trim();
  searchTimer = setTimeout(() => {
    if (q === lastQuery) return;
    lastQuery = q;
    // One character matches most of a library; wait for a second one.
    if (q.length < 2) {
      // Only pull the view back to the library if search is what put us here.
      if (searching) { searching = false; path = []; go("Library", showSections); }
      return;
    }
    searching = true;
    showSearch(q);
  }, 250);
}

async function showSearch(q) {
  // Replace the crumb stack rather than pushing, so typing doesn't leave a
  // trail of one crumb per keystroke. Picking a result still pushes onto it.
  path = [
    { label: "Library", load: () => { $("q").value = ""; lastQuery = ""; searching = false; return showSections(); } },
    { label: `Search "${q}"`, load: () => showSearch(q) },
  ];
  renderCrumbs();
  const my = ++navId;
  setHero("");
  let r;
  try {
    r = await (await fetch(`/api/search?q=${encodeURIComponent(q)}`)).json();
  } catch (e) { return toast(String(e)); }
  if (stale(my)) return;

  const box = document.createElement("div");
  const section = (label, node) => {
    const h = document.createElement("h2");
    h.className = "sec"; h.textContent = label;
    box.appendChild(h); box.appendChild(node);
  };
  const cards = (items, round, pick) => {
    const g = document.createElement("div"); g.className = "grid";
    items.forEach(a => {
      const sub = round ? "Artist" : esc(a.parentTitle || "") || (a.year || "");
      const card = el(`<div class="card${round ? " round" : ""}">${art(a.thumb, 300, "art", true)}<div class="t">${esc(a.title)}</div><div class="s">${sub}</div></div>`);
      card.onclick = () => pick(a);
      g.appendChild(card);
    });
    return g;
  };

  if (r.artists.length) section("Artists", cards(r.artists, true, a => go(a.title, () => showArtist(a))));
  if (r.albums.length) section("Albums", cards(r.albums, false, al => go(al.title, () => showAlbum(al))));
  if (r.tracks.length) {
    const ul = document.createElement("ul"); ul.className = "tracks";
    r.tracks.forEach(t => {
      const li = el(`<li data-rk="${t.ratingKey}"><span class="n">♪</span>
        <span class="t">${esc(t.title)}<span class="sub2"> — ${esc(trackArtist(t))}</span></span>
        <span class="acts">
          <button class="mini" data-act="next" title="Play next">Next</button>
          <button class="mini" data-act="add" title="Add to queue">+</button>
        </span><span class="d">${fmt(t.duration)}</span></li>`);
      li.onclick = (e) => {
        const act = e.target.dataset && e.target.dataset.act;
        if (act === "next") { e.stopPropagation(); return post(`/api/queue/add?rating_key=${t.ratingKey}&next=1`); }
        if (act === "add")  { e.stopPropagation(); return post(`/api/queue/add?rating_key=${t.ratingKey}`); }
        // A track rating key makes a one-item play queue server-side, the
        // same route album playback takes.
        post(`/api/play/${t.ratingKey}`);
      };
      ul.appendChild(li);
    });
    section("Tracks", ul);
  }
  if (!box.childNodes.length) box.innerHTML = `<div class="empty">No results for "${esc(q)}"</div>`;
  swap(box);
  markNow();
}

// ---- now playing ----
// A toggled view rather than a route: the library stays exactly where it was,
// so turning it off puts you back without re-navigating.
let npvOpen = false, npvSig = "";

function toggleNowPlaying() {
  npvOpen = !npvOpen;
  $("npv-btn").classList.toggle("on", npvOpen);
  $("npv").classList.toggle("on", npvOpen);
  // Hide the library rather than discard it; nothing needs re-fetching.
  $("hero").style.display = npvOpen ? "none" : "";
  $("content").style.display = npvOpen ? "none" : "";
  npvSig = "";
  renderNowPlaying();
}

// Leaves the now playing view and navigates, so the links actually go
// somewhere visible.
function npvGo(label, load) {
  if (npvOpen) toggleNowPlaying();
  go(label, load);
}

function renderNowPlaying() {
  if (!npvOpen) return;
  const box = $("npv");
  const t = last && last.current_index !== null ? last.queue[last.current_index] : null;
  // Re-render only when the track changes; paint() runs four times a second.
  const sig = t ? `${t.rating_key}|${t.play_queue_item_id}` : "";
  if (sig === npvSig) {
    // Same track, but the rating can still have changed under us.
    if (t) renderStars(t);
    return;
  }
  npvSig = sig;

  if (!t) {
    box.innerHTML = '<div class="empty">Nothing playing.</div>';
    return;
  }
  box.innerHTML = "";
  const cover = el(`<div class="cover">${t.thumb ? `<img src="${thumb(t.thumb, 600)}">` : "&#9835;"}</div>`);
  if (!t.thumb) cover.classList.add("ph");
  const img = cover.querySelector("img");
  if (img) img.onerror = () => { cover.classList.add("ph"); cover.innerHTML = "&#9835;"; };

  const info = el(`<div class="info">
    <div class="sub">Now playing</div>
    <h1>${esc(t.title)}</h1>
    <div class="meta" id="npv-artist"></div>
    <div class="meta" id="npv-album"></div>
    <div class="stars" id="npv-stars"></div>
    <div class="lyrics" id="npv-lyrics" style="display:none"></div>
  </div>`);

  // Only link where we know the rating key; otherwise show plain text rather
  // than a link that goes nowhere.
  const line = (host, label, name, rk, pick) => {
    if (!name) return;
    host.appendChild(el(`<span class="muted">${label} </span>`));
    if (rk) {
      const a = el(`<a>${esc(name)}</a>`);
      a.onclick = () => pick();
      host.appendChild(a);
    } else {
      host.appendChild(document.createTextNode(name));
    }
  };
  box.appendChild(cover);
  box.appendChild(info);
  line($("npv-artist"), "by", t.artist, t.artist_rating_key,
       () => npvGo(t.artist, () => showArtist({
         ratingKey: t.artist_rating_key, title: t.artist, thumb: t.artist_thumb })));
  line($("npv-album"), "from", t.album, t.album_rating_key,
       () => npvGo(t.album, () => showAlbum({
         ratingKey: t.album_rating_key, title: t.album, thumb: t.thumb, parentTitle: t.artist })));
  renderStars(t);
  loadLyrics(t);
}

// Plex stores 0-10; five stars, so each is worth two.
function renderStars(t) {
  const box = $("npv-stars");
  if (!box) return;
  const filled = Math.round((t.user_rating || 0) / 2);
  if (box.dataset.filled === String(filled) && box.dataset.rk === t.rating_key) return;
  box.dataset.filled = String(filled); box.dataset.rk = t.rating_key;
  box.innerHTML = "";
  for (let n = 1; n <= 5; n++) {
    const b = el(`<button title="${n} star${n > 1 ? "s" : ""}">&#9733;</button>`);
    if (n <= filled) b.classList.add("lit");
    // Clicking the star you already have clears the rating, which is the only
    // way back to unrated.
    // -1 removes the rating; 0 would store an explicit zero instead.
    b.onclick = () => post(`/api/rate?rating_key=${encodeURIComponent(t.rating_key)}&rating=${n === filled ? -1 : n * 2}`);
    b.onmouseenter = () => [...box.children].forEach((c, i) => c.classList.toggle("pre", i < n));
    box.appendChild(b);
  }
  box.onmouseleave = () => [...box.children].forEach(c => c.classList.remove("pre"));
}

// Rows kept above the active lyric line, so the rest of the panel shows what
// is still to come.
const LYRIC_LEAD = 4;
let lyricsFor = null, lyricsData = null, lyricLine = -1;

async function loadLyrics(t) {
  const box = $("npv-lyrics");
  if (!box) return;
  lyricsFor = t.rating_key; lyricsData = null; lyricLine = -1;
  box.style.display = "none"; box.innerHTML = "";
  let r;
  try { r = await (await fetch(`/api/lyrics/${t.rating_key}`)).json(); } catch (e) { return; }
  // The track may have changed while that was in flight.
  if (lyricsFor !== t.rating_key || !r || !r.lines || !r.lines.length) return;
  lyricsData = r;
  r.lines.forEach((l, i) => {
    const d = el(`<div class="ln" data-i="${i}">${esc(l.text || "")}</div>`);
    if (r.timed && l.start_ms != null) d.onclick = () => post(`/api/seek?ms=${l.start_ms}`);
    box.appendChild(d);
  });
  if (r.provider) box.appendChild(el(`<div class="by">${esc(r.provider)}</div>`));
  box.style.display = "";
}

// Highlights the line matching playback position; timed lyrics only.
function syncLyrics() {
  if (!npvOpen || !lyricsData || !lyricsData.timed || !last) return;
  let pos = last.position_ms || 0;
  if (last.state === "playing") pos += Date.now() - lastAt;
  let idx = -1;
  for (let i = 0; i < lyricsData.lines.length; i++) {
    const st = lyricsData.lines[i].start_ms;
    if (st == null) continue;
    if (st <= pos) idx = i; else break;
  }
  if (idx === lyricLine) return;
  lyricLine = idx;
  const box = $("npv-lyrics");
  if (!box) return;
  const lines = [...box.querySelectorAll(".ln")];
  lines.forEach((d, i) => d.classList.toggle("on", i === idx));
  if (idx < 0 || !lines.length) return;
  // Hold the active line a fixed few rows down the panel. scrollIntoView with
  // "nearest" only moves the minimum distance, so the line creeps to the
  // bottom edge and stays pinned there with nothing upcoming visible below it.
  // Anchoring the line LYRIC_LEAD rows earlier to the top keeps the sung and
  // unsung sides of the panel in proportion.
  const anchor = lines[Math.max(0, idx - LYRIC_LEAD)];
  box.scrollTo({ top: anchor.offsetTop - lines[0].offsetTop, behavior: "smooth" });
}

// ---- queue drawer ----
let queueOpen = false, lastQueueSig = "";
function toggleQueue() {
  queueOpen = !queueOpen;
  $("qd").classList.toggle("open", queueOpen);
  $("qbtn").classList.toggle("on", queueOpen);
  lastQueueSig = ""; renderQueue();
}
function renderQueue() {
  if (!last || !queueOpen) return;
  const sig = last.queue.map(t => t.play_queue_item_id ?? t.rating_key).join(",") + "|" + last.current_index;
  if (sig === lastQueueSig) return;
  lastQueueSig = sig;
  const box = $("qlist"); box.innerHTML = "";
  if (!last.queue.length) { box.innerHTML = '<div class="empty">Queue is empty</div>'; return; }
  last.queue.forEach((t, i) => {
    const row = el(`<div class="qi ${i === last.current_index ? "now" : ""}">
      ${art(t.thumb, 80, "", true)}
      <div style="min-width:0"><div class="t">${esc(t.title)}</div><div class="a">${esc(t.artist || "")}</div></div>
      <button class="x" title="Remove">×</button></div>`);
    row.onclick = () => post(`/api/skipto?i=${i}`);
    row.querySelector(".x").onclick = (e) => {
      e.stopPropagation();
      if (t.play_queue_item_id != null) post(`/api/queue/remove?item=${t.play_queue_item_id}`);
    };
    box.appendChild(row);
  });
  lazyArt(box);
  const now = box.querySelector(".qi.now");
  if (now) now.scrollIntoView({ block: "nearest" });
}

// ---- now playing ----
let last = null, lastAt = 0, dragging = false;
function seekClick(e) {
  if (!last || last.current_index === null) return;
  const r = $("np-bar").getBoundingClientRect();
  const frac = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
  const dur = last.queue[last.current_index].duration_ms || 0;
  post(`/api/seek?ms=${Math.floor(frac * dur)}`);
}
function setVol(v) { post(`/api/volume?v=${v}`); }

// Reads back from status rather than tracking it here — the phone can
// change it too.
function toggleShuffle() {
  if (!last) return;
  post(`/api/shuffle?v=${last.shuffle ? 0 : 1}`);
}
// off -> whole queue -> this track -> off
function cycleRepeat() {
  if (!last) return;
  post(`/api/repeat?v=${({ 0: 2, 2: 1, 1: 0 })[last.repeat || 0]}`);
}

function paint() {
  if (!last) return;
  const idx = last.current_index;
  const playing = idx !== null && last.queue.length;
  const t = playing ? last.queue[idx] : null;
  $("np-title").textContent = t ? t.title : "Nothing playing";
  $("np-artist").textContent = t ? (t.artist || "") : "";
  const art = t ? thumb(t.thumb, 120) : "";
  const cover = $("np-cover");
  if (art) {
    if ($("np-art").getAttribute("src") !== art) $("np-art").src = art;
    cover.classList.add("has-art");
  } else {
    $("np-art").removeAttribute("src");
    cover.classList.remove("has-art");
  }
  $("ic-play").style.display = last.state === "playing" ? "none" : "";
  $("ic-pause").style.display = last.state === "playing" ? "" : "none";
  let pos = last.position_ms || 0;
  if (last.state === "playing") pos += Date.now() - lastAt;
  const dur = t ? t.duration_ms : 0;
  if (dur) pos = Math.min(pos, dur);
  $("np-pos").textContent = fmt(pos);
  $("np-dur").textContent = fmt(dur);
  $("np-fill").style.width = dur ? `${(pos / dur) * 100}%` : "0%";
  const rp = last.repeat || 0;
  $("rp-btn").classList.toggle("on", rp !== 0);
  $("ic-rp").style.display = rp === 1 ? "none" : "";
  $("ic-rp1").style.display = rp === 1 ? "" : "none";
  $("sh-btn").classList.toggle("on", !!last.shuffle);
  renderNowPlaying();
  syncLyrics();
  const rk = t ? t.rating_key : null;
  if (rk !== nowRatingKey) { nowRatingKey = rk; markNow(); }
}

async function poll() {
  try {
    const gen = last ? last.generation : -1;
    const s = await (await fetch(`/api/status?gen=${gen}`)).json();
    if (s.unchanged) { Object.assign(last, { state: s.state, position_ms: s.position_ms, volume: s.volume }); lastAt = Date.now(); return; }
    last = s; lastAt = Date.now();
    if (document.activeElement !== $("np-vol")) $("np-vol").value = s.volume;
    renderQueue();
  } catch (e) {}
}

go("Library", showSections);
poll(); setInterval(poll, 1000); setInterval(paint, 250);
</script>
</body>
</html>"#;

// ---------- Web server ----------

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    server_url: String,
    server_machine_id: String,
    token: String,
    player_name: String,
    player_tx: std_mpsc::Sender<PlayerCmd>,
    status: SharedStatus,
    queue_serial: Arc<std::sync::atomic::AtomicU64>,
    thumb_cache: Arc<Mutex<ThumbCache>>,
    listing_cache: Arc<Mutex<ListingCache>>,
    // Server URL and token the *active queue* came from. A queue created by
    // a phone may live on a different address (and use a different token)
    // than our configured one, and edits/refreshes must keep using it.
    queue_source: Arc<Mutex<(String, String)>>,
}

// Raw PMS JSON for library listings, cached briefly. Passing the bytes
// straight through avoids a serde round-trip per navigation on the Pi.
#[derive(Default)]
struct ListingCache {
    map: std::collections::HashMap<String, (std::time::Instant, Arc<Vec<u8>>)>,
}
impl ListingCache {
    const TTL: Duration = Duration::from_secs(300);
    fn get(&self, k: &str) -> Option<Arc<Vec<u8>>> {
        self.map
            .get(k)
            .filter(|(t, _)| t.elapsed() < Self::TTL)
            .map(|(_, b)| b.clone())
    }
    fn put(&mut self, k: String, bytes: Vec<u8>) -> Arc<Vec<u8>> {
        if self.map.len() > 200 {
            self.map.clear();
        }
        let b = Arc::new(bytes);
        self.map.insert(k, (std::time::Instant::now(), b.clone()));
        b
    }
}

async fn cached_listing(state: &AppState, path: &str) -> axum::response::Response {
    if let Some(b) = state.listing_cache.lock().unwrap().get(path) {
        return ([("Content-Type", "application/json")], b.as_ref().clone()).into_response();
    }
    let url = format!("{}{}", state.server_url, path);
    let req = state.client.get(&url).header("X-Plex-Token", &state.token);
    match plex_headers(req).send().await {
        Ok(resp) if resp.status().is_success() => match resp.bytes().await {
            Ok(b) => {
                let bytes = state
                    .listing_cache
                    .lock()
                    .unwrap()
                    .put(path.to_string(), b.to_vec());
                ([("Content-Type", "application/json")], bytes.as_ref().clone()).into_response()
            }
            Err(e) => err_response(e.into()),
        },
        Ok(resp) => (StatusCode::BAD_GATEWAY, format!("PMS returned {}", resp.status())).into_response(),
        Err(e) => err_response(e.into()),
    }
}

// Small in-memory cache of transcoded artwork so the browser's grids don't
// hit PMS's transcoder on every visit. Bounded by entry count; oldest out.
#[derive(Default)]
struct ThumbCache {
    map: std::collections::HashMap<String, (String, Vec<u8>)>,
    order: std::collections::VecDeque<String>,
}
impl ThumbCache {
    const MAX: usize = 600;
    fn get(&self, k: &str) -> Option<(String, Vec<u8>)> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: String, ct: String, bytes: Vec<u8>) {
        if self.map.insert(k.clone(), (ct, bytes)).is_none() {
            self.order.push_back(k);
        }
        while self.order.len() > Self::MAX {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

impl AppState {
    fn next_serial(&self) -> u64 {
        self.queue_serial
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    fn local_queue_info(&self) -> QueueInfo {
        let (protocol, address, port) = split_url(&self.server_url);
        QueueInfo {
            machine_identifier: self.server_machine_id.clone(),
            address,
            port,
            protocol,
            container_key: None,
            play_queue_id: None,
            play_queue_version: None,
        }
    }

}

// Everything PMS tells us about a queue it just handed back.
// `base` says where the queue lives; only the per-response fields come from
// `mc`. A caller that just created a queue on our own server passes
// local_queue_info(); an edit to an existing queue must pass that queue's
// current info, because a queue the phone built can live on a different
// server and the timeline has to keep pointing at it.
fn queue_info_from(base: QueueInfo, mc: &MetadataContainer) -> QueueInfo {
    let mut info = base;
    info.play_queue_id = mc.play_queue_id;
    info.play_queue_version = mc.play_queue_version;
    info.container_key = mc.play_queue_id.map(|id| format!("/playQueues/{id}"));
    info
}

// "http://192.168.1.50:32400" -> ("http", "192.168.1.50", 32400)
fn split_url(url: &str) -> (String, String, u16) {
    let (proto, rest) = url.split_once("://").unwrap_or(("http", url));
    let rest = rest.trim_end_matches('/');
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap()),
        _ => (
            rest.to_string(),
            if proto == "https" { 443 } else { 80 },
        ),
    };
    (proto.to_string(), host, port)
}

fn to_queued(t: &Metadata) -> QueuedTrack {
    QueuedTrack {
        title: t.title.clone(),
        artist: t
            .original_title
            .clone()
            .or_else(|| t.grandparent_title.clone()),
        album: t.parent_title.clone(),
        key: t
            .key
            .clone()
            .unwrap_or_else(|| format!("/library/metadata/{}", t.rating_key)),
        rating_key: t.rating_key.clone(),
        album_rating_key: t.parent_rating_key.clone(),
        artist_rating_key: t.grandparent_rating_key.clone(),
        artist_thumb: t.grandparent_thumb.clone(),
        user_rating: t.user_rating,
        play_queue_item_id: t.play_queue_item_id,
        duration_ms: t.duration.unwrap_or(0),
        thumb: t.thumb.clone().or_else(|| t.parent_thumb.clone()),
        bytes: None,
    }
}

// Asks the Plex server to build a play queue for `rating_key` (an album,
// artist, or track). Plex controllers expect remote players to always be
// playing from a server-side queue — the phone app stops a player whose
// timeline has no playQueueID when it tries to inspect its queue.
async fn create_play_queue(
    state: &AppState,
    rating_key: &str,
    start_track_key: Option<&str>,
    shuffle: bool,
) -> anyhow::Result<(Vec<Metadata>, usize, QueueInfo)> {
    let uri = format!(
        "server://{}/com.plexapp.plugins.library/library/metadata/{}",
        state.server_machine_id, rating_key
    );
    create_play_queue_uri(state, &uri, start_track_key, shuffle).await
}

// A whole library section as a queue source, rather than one item. The same
// endpoint takes either; only the uri differs.
fn section_uri(state: &AppState, section_key: &str) -> String {
    format!(
        "server://{}/com.plexapp.plugins.library/library/sections/{}/all",
        state.server_machine_id, section_key
    )
}

async fn create_play_queue_uri(
    state: &AppState,
    uri: &str,
    start_track_key: Option<&str>,
    shuffle: bool,
) -> anyhow::Result<(Vec<Metadata>, usize, QueueInfo)> {
    let (server_url, token) = (state.server_url.clone(), state.token.clone());
    create_play_queue_on(state, &server_url, &token, uri, start_track_key, shuffle).await
}

async fn create_play_queue_on(
    state: &AppState,
    server_url: &str,
    token: &str,
    uri: &str,
    start_track_key: Option<&str>,
    shuffle: bool,
) -> anyhow::Result<(Vec<Metadata>, usize, QueueInfo)> {
    let mut url = format!(
        "{}/playQueues?type=audio&uri={}&continuous=0&repeat=0&shuffle={}&includeChapters=1&own=1",
        server_url,
        urlencoding::encode(uri),
        u8::from(shuffle)
    );
    if let Some(k) = start_track_key {
        url.push_str(&format!("&key={}", urlencoding::encode(k)));
    }
    let req = state.client.post(&url).header("X-Plex-Token", token);
    let resp = plex_headers(req).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("create play queue failed: {status} — {body}");
    }
    let r: MetadataResponse = resp.json().await?;
    let mut mc = r.media_container;
    // The POST answers with a window, not the queue.
    if is_windowed(&mc) {
        if let Some(id) = mc.play_queue_id {
            mc = fetch_full_queue(&state.client, server_url, token, id).await?;
        }
    }
    let start = mc
        .metadata
        .iter()
        .position(|m| {
            m.play_queue_item_id.is_some()
                && m.play_queue_item_id == mc.play_queue_selected_item_id
        })
        .unwrap_or(0);
    let info = queue_info_from(state.local_queue_info(), &mc);
    Ok((mc.metadata, start, info))
}

// Starts playing `tracks` from `start_index`: downloads that one track,
// hands the whole (titles-only) queue to the player, then runs a prefetcher
// that keeps exactly the current and next tracks' audio resident.
async fn start_queue(
    state: &AppState,
    server_url: String,
    token: String,
    tracks: Vec<Metadata>,
    start_index: usize,
    start_offset_ms: u64,
    info: QueueInfo,
    paused: bool,
) -> anyhow::Result<()> {
    if tracks.is_empty() {
        anyhow::bail!("no tracks to play");
    }
    let start_index = start_index.min(tracks.len() - 1);
    let serial = state.next_serial();
    *state.queue_source.lock().unwrap() = (server_url.clone(), token.clone());

    let first_bytes: Arc<[u8]> =
        fetch_track_bytes(&state.client, &server_url, &token, &tracks[start_index]).await?;

    let mut queued: Vec<QueuedTrack> = tracks.iter().map(to_queued).collect();
    queued[start_index].bytes = Some(first_bytes);

    state
        .player_tx
        .send(PlayerCmd::PlayQueue {
            serial,
            tracks: queued,
            start_index,
            start_offset_ms,
            info,
            paused,
        })
        .map_err(|_| anyhow::anyhow!("player thread is gone"))?;

    spawn_prefetcher(state, serial, server_url, token, tracks, [start_index].into_iter().collect());
    Ok(())
}

// Prefetcher: fetches whatever the player is stalled on, plus one track
// ahead, and exits once a newer queue (serial) replaces this one.
fn spawn_prefetcher(
    state: &AppState,
    serial: u64,
    server_url: String,
    token: String,
    tracks: Vec<Metadata>,
    mut fetched: std::collections::HashSet<usize>,
) {
    let client = state.client.clone();
    let player_tx = state.player_tx.clone();
    let status = state.status.clone();
    tokio::spawn(async move {
        // When each index was last *attempted*, successfully or not.
        let mut last_try: std::collections::HashMap<usize, std::time::Instant> =
            Default::default();
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let (cur, waiting, alive) = {
                let s = status.lock().unwrap();
                (s.current_index, s.waiting_for, s.queue_serial == serial)
            };
            if !alive {
                return;
            }
            let Some(cur) = cur else { return }; // stopped
            // No index is retried more often than this, whether the last
            // attempt succeeded or failed. Gating on success alone let a
            // failing fetch retry on every tick — three times a second, with
            // a log line each — for the rest of the current track.
            const RETRY_AFTER: Duration = Duration::from_secs(5);
            let now = std::time::Instant::now();
            let due = |last: Option<&std::time::Instant>| {
                last.map(|t| now.duration_since(*t) > RETRY_AFTER)
                    .unwrap_or(true)
            };
            let mut wanted: Vec<usize> = Vec::new();
            if let Some(w) = waiting {
                // The player is stalled on `w`. Refetch it, but not more
                // often than that — if it's already been sent and the player
                // just hasn't consumed it yet, hammering PMS only makes
                // things worse.
                if due(last_try.get(&w)) {
                    wanted.push(w);
                    fetched.remove(&w);
                }
            }
            if cur + 1 < tracks.len() && due(last_try.get(&(cur + 1))) {
                wanted.push(cur + 1);
            }
            for idx in wanted {
                if fetched.contains(&idx) || idx >= tracks.len() {
                    continue;
                }
                let track = &tracks[idx];
                last_try.insert(idx, std::time::Instant::now());
                match fetch_track_bytes(&client, &server_url, &token, track).await {
                    Ok(bytes) => {
                        let _ = player_tx.send(PlayerCmd::SetBytes(idx, bytes));
                        fetched.insert(idx);
                    }
                    // {e:#} for anyhow's source chain: a truncated body and a
                    // JSON parse failure both print as "error decoding
                    // response body" on their own.
                    Err(e) => eprintln!("prefetch failed for \"{}\": {e:#}", track.title),
                }
            }
        }
    });
}

// ---------- Playback reporting ----------
//
// PMS records nothing for a client that doesn't tell it what it's doing —
// no play counts, no last-played, no "continue listening", and nothing for
// Last.fm to scrobble. So mirror the player's state to /:/timeline: a ping
// when a track starts, a keepalive while it runs, and a final "stopped"
// carrying the position the track actually reached. That last report is
// what makes the play count stick, so it has to be accurate.

// A snapshot of what the player is on, in the shape /:/timeline wants.
#[derive(Clone)]
struct TimelineReport {
    rating_key: String,
    key: String,
    play_queue_item_id: Option<u64>,
    play_queue_id: Option<u64>,
    play_queue_version: Option<u64>,
    container_key: Option<String>,
    duration_ms: u64,
    position_ms: u64,
    state: String,
}

impl TimelineReport {
    // Identifies the queue entry rather than the track: the same track can
    // sit in a queue twice, and each listen is reported separately.
    fn item(&self) -> (Option<u64>, String) {
        (self.play_queue_item_id, self.rating_key.clone())
    }
}

fn current_report(status: &SharedStatus) -> Option<TimelineReport> {
    let s = status.lock().unwrap();
    let track = s.queue.get(s.current_index?)?;
    Some(TimelineReport {
        rating_key: track.rating_key.clone(),
        key: track.key.clone(),
        play_queue_item_id: track.play_queue_item_id,
        play_queue_id: s.info.play_queue_id,
        play_queue_version: s.info.play_queue_version,
        container_key: s.info.container_key.clone(),
        duration_ms: track.duration_ms,
        position_ms: s.position_ms,
        state: s.state.clone(),
    })
}

// Plex's own clients send this as a GET with everything in the query
// string; the response body is empty.
async fn post_timeline(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    r: &TimelineReport,
) -> anyhow::Result<()> {
    // Position counts frames handed to the device, so at the end of a track
    // it can land a little past the duration PMS has on file. Clamp it —
    // reporting a time beyond the item's own duration is nonsense whatever
    // the server chooses to do with it.
    let time = if r.duration_ms > 0 {
        r.position_ms.min(r.duration_ms)
    } else {
        r.position_ms
    };
    let mut url = format!(
        "{server_url}/:/timeline?identifier=com.plexapp.plugins.library&ratingKey={}&key={}&state={}&time={time}&duration={}",
        urlencoding::encode(&r.rating_key),
        urlencoding::encode(&r.key),
        r.state,
        r.duration_ms,
    );
    // Tie the report to the play queue, not just the track: without this
    // the server sees an isolated item and loses the queue context that
    // "continue listening" is built from. Deliberately not sending
    // hasMDE=1, which Plexamp sets — we direct-play and never ask the
    // server for a playback decision, so claiming it would be a lie.
    if let Some(id) = r.play_queue_item_id {
        url.push_str(&format!("&playQueueItemID={id}"));
    }
    if let Some(id) = r.play_queue_id {
        url.push_str(&format!("&playQueueID={id}"));
    }
    if let Some(v) = r.play_queue_version {
        url.push_str(&format!("&playQueueVersion={v}"));
    }
    if let Some(ck) = &r.container_key {
        url.push_str(&format!("&containerKey={}", urlencoding::encode(ck)));
    }
    let req = client.get(&url).header("X-Plex-Token", token);
    let resp = plex_headers(req).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("{} for {}", resp.status(), redact(&url));
    }
    Ok(())
}

async fn send_report(
    client: &reqwest::Client,
    queue_source: &Arc<Mutex<(String, String)>>,
    r: &TimelineReport,
    failing: &mut bool,
) {
    // Report to the server the queue came from: one started by the phone
    // may live on a different address, under a different token, and that's
    // where the item being played lives too.
    let (server_url, token) = queue_source.lock().unwrap().clone();
    match post_timeline(client, &server_url, &token, r).await {
        Ok(()) => *failing = false,
        Err(e) => {
            // Once per outage — otherwise a server that's down produces a
            // line every keepalive. Reporting never affects playback.
            if !*failing {
                *failing = true;
                eprintln!("playback reporting to {server_url} failed: {e}");
            }
        }
    }
}

// Watches SharedStatus and reports transitions to PMS. Runs as its own task
// so a slow or unreachable server can't stall the player or a web handler.
fn spawn_timeline_reporter(state: &AppState) {
    let client = state.client.clone();
    let status = state.status.clone();
    let queue_source = state.queue_source.clone();

    tokio::spawn(async move {
        // PMS expires a session it hasn't heard from and drops it out of
        // "continue listening" with it; Plex's clients ping every 10s.
        const KEEPALIVE: Duration = Duration::from_secs(10);

        // The most recent snapshot, kept so a track that ends can be closed
        // out at the position it reached rather than the one carried by the
        // last keepalive, which may be up to KEEPALIVE stale.
        let mut seen: Option<TimelineReport> = None;
        let mut sent_item: Option<(Option<u64>, String)> = None;
        let mut sent_state = String::new();
        let mut sent_at = std::time::Instant::now();
        let mut failing = false;

        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let current = current_report(&status);

            // The track advanced, was replaced, or playback ended: close
            // out the one we were reporting before announcing anything new.
            if let Some(prev) = &seen {
                let prev_item = prev.item();
                let gone = current.as_ref().map(|c| c.item()) != Some(prev_item.clone());
                if gone && sent_item.as_ref() == Some(&prev_item) {
                    let mut end = prev.clone();
                    end.state = "stopped".to_string();
                    send_report(&client, &queue_source, &end, &mut failing).await;
                    sent_item = None;
                    sent_state.clear();
                }
            }

            if let Some(c) = &current {
                let due = sent_item.as_ref() != Some(&c.item())
                    || sent_state != c.state
                    || sent_at.elapsed() >= KEEPALIVE;
                if due {
                    send_report(&client, &queue_source, c, &mut failing).await;
                    sent_item = Some(c.item());
                    sent_state = c.state.clone();
                    sent_at = std::time::Instant::now();
                }
            }

            seen = current;
        }
    });
}

// Pushes an edited play queue (as returned by the PMS queue API) into the
// player and restarts prefetching for the new track order.
fn apply_queue_edit(state: &AppState, mc: MetadataContainer) {
    let serial = state.next_serial();
    // Keep the queue where it already is. Rebuilding this from our configured
    // server repoints the timeline at the wrong machine for any queue the
    // phone created, and a controller that cannot reconcile its queue stops
    // the player (decision 2).
    let base = {
        let s = state.status.lock().unwrap();
        if s.info.machine_identifier.is_empty() {
            state.local_queue_info()
        } else {
            s.info.clone()
        }
    };
    let info = queue_info_from(base, &mc);
    let (src_url, src_token) = state.queue_source.lock().unwrap().clone();
    let queued: Vec<QueuedTrack> = mc.metadata.iter().map(to_queued).collect();
    let _ = state.player_tx.send(PlayerCmd::ReplaceQueue {
        serial,
        tracks: queued,
        info,
    });
    spawn_prefetcher(state, serial, src_url, src_token, mc.metadata, Default::default());
}

async fn queue_request(
    state: &AppState,
    method: reqwest::Method,
    url: &str,
) -> anyhow::Result<MetadataContainer> {
    let token = state.queue_source.lock().unwrap().1.clone();
    let req = state.client.request(method, url).header("X-Plex-Token", token);
    let resp = plex_headers(req).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("queue edit failed: {status} — {body}");
    }
    let r: MetadataResponse = serde_json::from_str(&body)?;
    Ok(r.media_container)
}


// POST /api/queue/add?rating_key=X[&next=1] — add an album/track to the
// current queue (or start a new one if nothing is queued).
async fn queue_add_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> axum::response::Response {
    let Some(rk) = params.get("rating_key") else {
        return (StatusCode::BAD_REQUEST, "missing rating_key").into_response();
    };
    let next = params.get("next").map(|v| v == "1").unwrap_or(false);
    let pq_id = state.status.lock().unwrap().info.play_queue_id;

    let Some(pq_id) = pq_id else {
        // Nothing queued yet: create and play.
        return match create_play_queue(&state, rk, None, false).await {
            Ok((tracks, start, info)) => match start_queue(
                &state,
                state.server_url.clone(),
                state.token.clone(),
                tracks,
                start,
                0,
                info,
                false,
            )
            .await
            {
                Ok(()) => StatusCode::OK.into_response(),
                Err(e) => err_response(e),
            },
            Err(e) => err_response(e),
        };
    };

    let queue_server = state.queue_source.lock().unwrap().0.clone();
    let url = format!(
        "{}/playQueues/{pq_id}?uri={}&includeChapters=1&own=1{}",
        queue_server,
        urlencoding::encode(&format!(
            "server://{}/com.plexapp.plugins.library/library/metadata/{}",
            state.server_machine_id, rk
        )),
        if next { "&next=1" } else { "" }
    );
    match queue_request(&state, reqwest::Method::PUT, &url).await {
        Ok(mc) => apply_edit_response(&state, mc).await,
        Err(e) => err_response(e),
    }
}

// POST /api/queue/remove?item=<playQueueItemID>
async fn queue_remove_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> axum::response::Response {
    let Some(item) = params.get("item") else {
        return (StatusCode::BAD_REQUEST, "missing item").into_response();
    };
    let pq_id = state.status.lock().unwrap().info.play_queue_id;
    let Some(pq_id) = pq_id else {
        return (StatusCode::CONFLICT, "no active queue").into_response();
    };
    let queue_server = state.queue_source.lock().unwrap().0.clone();
    let url = format!("{queue_server}/playQueues/{pq_id}/items/{item}?includeChapters=1&own=1");
    match queue_request(&state, reqwest::Method::DELETE, &url).await {
        Ok(mc) => apply_edit_response(&state, mc).await,
        Err(e) => err_response(e),
    }
}

// A queue edit answers with a window rather than the queue, so read the whole
// thing back before handing it to the player.
async fn apply_edit_response(
    state: &AppState,
    mc: MetadataContainer,
) -> axum::response::Response {
    let mc = match mc.play_queue_id.filter(|_| is_windowed(&mc)) {
        Some(id) => {
            let (server_url, token) = state.queue_source.lock().unwrap().clone();
            match fetch_full_queue(&state.client, &server_url, &token, id).await {
                Ok(full) => full,
                Err(e) => return err_response(e),
            }
        }
        None => mc,
    };
    apply_queue_edit(state, mc);
    StatusCode::OK.into_response()
}

fn err_response(e: anyhow::Error) -> axum::response::Response {
    (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
}

// ---- Browser UI API ----

async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn sections_handler(State(state): State<AppState>) -> impl IntoResponse {
    match fetch_sections(&state.client, &state.server_url, &state.token).await {
        Ok(sections) => {
            let music: Vec<Directory> =
                sections.into_iter().filter(|d| d.kind == "artist").collect();
            Json(music).into_response()
        }
        Err(e) => err_response(e),
    }
}

// These return PMS's own MediaContainer JSON (the browser reads
// .MediaContainer.Metadata), cached for a few minutes.
// excludeFields=summary is worth ~10x here: artist biographies are the bulk of
// this payload (3.2MB of 3.6MB on a 589-artist library) and nothing displays
// them. Measured against PMS 1.43.4.
async fn section_items_handler(
    State(state): State<AppState>,
    Path(section_key): Path<String>,
) -> axum::response::Response {
    cached_listing(
        &state,
        &format!("/library/sections/{section_key}/all?excludeFields=summary"),
    )
    .await
}

// Flat listings across the whole section, sorted by title, for browsing by
// album name or song title rather than drilling through artists.
//
// sort=title, not titleSort: the latter is what Plex's own UIs use but it comes
// back in an order that is not alphabetical by anything we display, which looks
// broken in a flat A-Z list. excludeFields=summary halves the album payload;
// tracks carry no summaries so it changes nothing there.
async fn section_albums_handler(
    State(state): State<AppState>,
    Path(section_key): Path<String>,
) -> axum::response::Response {
    cached_listing(
        &state,
        &format!("/library/sections/{section_key}/all?type=9&sort=title&excludeFields=summary"),
    )
    .await
}

async fn section_tracks_handler(
    State(state): State<AppState>,
    Path(section_key): Path<String>,
) -> axum::response::Response {
    cached_listing(
        &state,
        // excludeElements=Media drops about a fifth of this payload. The song
        // list shows title, artist and duration, and plays through
        // /api/play/{ratingKey}, which builds the queue server-side - the
        // part keys are never read here.
        &format!("/library/sections/{section_key}/all?type=10&sort=title&excludeElements=Media"),
    )
    .await
}

async fn browse_handler(
    State(state): State<AppState>,
    Path(rating_key): Path<String>,
) -> axum::response::Response {
    cached_listing(&state, &format!("/library/metadata/{rating_key}/children")).await
}

// Grouped the way the UI renders it, so the browser doesn't have to walk
// the Hub structure itself.
#[derive(Serialize, Default)]
struct SearchResults {
    artists: Vec<Metadata>,
    albums: Vec<Metadata>,
    tracks: Vec<Metadata>,
}

// GET /api/search?q= — deliberately not run through ListingCache: every
// keystroke is a distinct query, and 200 of them would evict the browse
// listings that cache exists to keep off the Pi's network path.
async fn search_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> axum::response::Response {
    let q = params.get("q").map(|s| s.trim()).unwrap_or("");
    if q.is_empty() {
        return Json(SearchResults::default()).into_response();
    }
    // excludeFields=summary drops the long description blobs we never show.
    // Collections are left out because the UI has nowhere to put them.
    let url = format!(
        "{}/hubs/search?query={}&excludeFields=summary&limit=24&includeCollections=0",
        state.server_url,
        urlencoding::encode(q)
    );
    let r: HubsResponse = match plex_get_json(&state.client, &url, &state.token).await {
        Ok(r) => r,
        Err(e) => return err_response(e),
    };
    let mut out = SearchResults::default();
    for hub in r.media_container.hub {
        // An unscoped search covers every library on the server, so hubs for
        // movies, shows and the rest come back too; keep the three the
        // player can actually do something with.
        let bucket = match hub.kind.as_str() {
            "artist" => &mut out.artists,
            "album" => &mut out.albums,
            "track" => &mut out.tracks,
            _ => continue,
        };
        bucket.extend(hub.metadata);
    }
    Json(out).into_response()
}

async fn play_handler(
    State(state): State<AppState>,
    Path(rating_key): Path<String>,
) -> impl IntoResponse {
    let (tracks, start, info) = match create_play_queue(&state, &rating_key, None, false).await {
        Ok(v) => v,
        Err(e) => return err_response(e),
    };
    match start_queue(
        &state,
        state.server_url.clone(),
        state.token.clone(),
        tracks,
        start,
        0,
        info,
        false,
    )
    .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => err_response(e),
    }
}

async fn play_album_handler(
    State(state): State<AppState>,
    Path(rating_key): Path<String>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> impl IntoResponse {
    // Shuffle play: a one-off shuffled queue built by the server, distinct
    // from shuffle mode. The order is baked into the queue, so the phone and
    // the web UI both see exactly what will play, and it starts from the top
    // rather than from any particular track.
    let shuffle = params.get("shuffle").map(|v| v == "1").unwrap_or(false);
    if shuffle {
        // Leaving shuffle mode on as well would re-randomise a queue that is
        // already in the order the listener asked for.
        let _ = state.player_tx.send(PlayerCmd::SetShuffle(false));
    }

    // `start` is an index into the album's track list; the play queue API
    // wants the starting track's key instead, so resolve it first. A shuffled
    // queue has no meaningful starting track.
    let start_key = match params.get("start").and_then(|v| v.parse::<usize>().ok()) {
        Some(i) if i > 0 && !shuffle => {
            match fetch_children(&state.client, &state.server_url, &state.token, &rating_key).await
            {
                Ok(t) => t.get(i).and_then(|m| m.key.clone()),
                Err(e) => return err_response(e),
            }
        }
        _ => None,
    };
    let (tracks, start, info) =
        match create_play_queue(&state, &rating_key, start_key.as_deref(), shuffle).await {
            Ok(v) => v,
            Err(e) => return err_response(e),
        };
    match start_queue(
        &state,
        state.server_url.clone(),
        state.token.clone(),
        tracks,
        start,
        0,
        info,
        false,
    )
    .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => err_response(e),
    }
}

fn send_cmd(state: &AppState, cmd: PlayerCmd) -> StatusCode {
    let _ = state.player_tx.send(cmd);
    StatusCode::OK
}

async fn pause_handler(State(state): State<AppState>) -> StatusCode {
    send_cmd(&state, PlayerCmd::Pause)
}
async fn resume_handler(State(state): State<AppState>) -> StatusCode {
    send_cmd(&state, PlayerCmd::Resume)
}
async fn skip_handler(State(state): State<AppState>) -> StatusCode {
    send_cmd(&state, PlayerCmd::SkipOne)
}
async fn stop_handler(State(state): State<AppState>) -> StatusCode {
    send_cmd(&state, PlayerCmd::Stop)
}

async fn playpause_handler(State(state): State<AppState>) -> StatusCode {
    send_cmd(&state, PlayerCmd::PlayPause)
}
async fn prev_handler(State(state): State<AppState>) -> StatusCode {
    send_cmd(&state, PlayerCmd::SkipPrevious)
}
async fn seek_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> StatusCode {
    let ms = params.get("ms").and_then(|v| v.parse().ok()).unwrap_or(0);
    send_cmd(&state, PlayerCmd::Seek(ms))
}
async fn volume_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> StatusCode {
    let v = params.get("v").and_then(|v| v.parse().ok()).unwrap_or(100);
    send_cmd(&state, PlayerCmd::SetVolume(v))
}
async fn skipto_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> StatusCode {
    match params.get("i").and_then(|v| v.parse().ok()) {
        Some(i) => send_cmd(&state, PlayerCmd::SkipTo(i)),
        None => StatusCode::BAD_REQUEST,
    }
}

// POST /api/shuffle-library/:section_key — a shuffled queue of everything in
// the library, the same one-off shuffled queue an album's Shuffle button makes,
// just seeded from the whole section.
async fn shuffle_library_handler(
    State(state): State<AppState>,
    Path(section_key): Path<String>,
) -> axum::response::Response {
    // The queue is already in the order the listener asked for; shuffle mode on
    // top of it would only re-randomise it.
    let _ = state.player_tx.send(PlayerCmd::SetShuffle(false));
    let uri = section_uri(&state, &section_key);
    let (tracks, start, info) = match create_play_queue_uri(&state, &uri, None, true).await {
        Ok(v) => v,
        Err(e) => return err_response(e),
    };
    match start_queue(
        &state,
        state.server_url.clone(),
        state.token.clone(),
        tracks,
        start,
        0,
        info,
        false,
    )
    .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => err_response(e),
    }
}

// POST /api/rate?rating_key=X&rating=N — Plex's 0-10 scale, shown as five
// stars. -1 removes the rating: sending 0 instead stores a zero rating, which
// reads back as 0.0 rather than absent. Verified against PMS 1.43.4.
async fn rate_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> axum::response::Response {
    let Some(rk) = params.get("rating_key") else {
        return (StatusCode::BAD_REQUEST, "missing rating_key").into_response();
    };
    let rating: f32 = params
        .get("rating")
        .and_then(|v| v.parse().ok())
        .unwrap_or(-1.0f32)
        .clamp(-1.0, 10.0);
    let url = format!(
        "{}/:/rate?key={}&identifier=com.plexapp.plugins.library&rating={rating}",
        state.server_url,
        urlencoding::encode(rk),
    );
    let req = state.client.put(&url).header("X-Plex-Token", &state.token);
    match plex_headers(req).send().await {
        Ok(r) if r.status().is_success() => {
            // The player's copy of the track still holds the old rating, and
            // nothing else will correct it until the queue changes.
            let _ = state.player_tx.send(PlayerCmd::SetRating(rk.clone(), rating));
            StatusCode::OK.into_response()
        }
        Ok(r) => (
            StatusCode::BAD_GATEWAY,
            format!("rating failed: {}", r.status()),
        )
            .into_response(),
        Err(e) => err_response(e.into()),
    }
}

#[derive(Serialize, Default)]
struct LyricLine {
    start_ms: Option<u64>,
    text: String,
}

#[derive(Serialize, Default)]
struct Lyrics {
    provider: Option<String>,
    // Whether lines carry timings; untimed lyrics are shown as a plain block.
    timed: bool,
    lines: Vec<LyricLine>,
}

#[derive(Deserialize, Debug, Default)]
struct PlexSpan {
    text: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct PlexLine {
    #[serde(rename = "startOffset")]
    start_offset: Option<u64>,
    #[serde(rename = "Span", default)]
    span: Vec<PlexSpan>,
}

#[derive(Deserialize, Debug, Default)]
struct PlexLyrics {
    provider: Option<String>,
    timed: Option<serde_json::Value>,
    #[serde(rename = "Line", default)]
    line: Vec<PlexLine>,
}

#[derive(Deserialize, Debug, Default)]
struct LyricsContainer {
    // PMS returns an array here; accept a lone object too rather than fail.
    #[serde(rename = "Lyrics", default)]
    lyrics: serde_json::Value,
}

#[derive(Deserialize, Debug)]
struct LyricsResponse {
    #[serde(rename = "MediaContainer")]
    media_container: LyricsContainer,
}

// GET /api/lyrics/:rating_key — empty when the track has none, which is the
// common case; the UI hides the tab rather than showing an error.
async fn lyrics_handler(
    State(state): State<AppState>,
    Path(rating_key): Path<String>,
) -> axum::response::Response {
    let track = match fetch_item(&state.client, &state.server_url, &state.token, &rating_key).await
    {
        Ok(t) => t,
        Err(e) => return err_response(e),
    };
    // A track can carry several lyric streams and some of them 404 — PMS lists
    // stream ids that no longer resolve. So try each in turn, timed ("lrc")
    // first, and take the first that actually returns lines.
    let mut keys: Vec<(bool, String)> = track
        .media
        .iter()
        .flat_map(|m| m.part.iter())
        .flat_map(|p| p.stream.iter())
        .filter(|s| s.stream_type == Some(STREAM_TYPE_LYRIC))
        .filter_map(|s| {
            s.key
                .clone()
                .map(|k| (s.format.as_deref() == Some("lrc"), k))
        })
        .collect();
    keys.sort_by_key(|(is_lrc, _)| !is_lrc);

    for (_, stream_key) in keys {
        let url = format!("{}{stream_key}?includeInlineAttribution=1", state.server_url);
        let Ok(r) = plex_get_json::<LyricsResponse>(&state.client, &url, &state.token).await
        else {
            continue; // dead stream id; try the next one
        };
        let lyrics = to_lyrics(r.media_container.lyrics);
        if !lyrics.lines.is_empty() {
            return Json(lyrics).into_response();
        }
    }
    Json(Lyrics::default()).into_response()
}

fn to_lyrics(value: serde_json::Value) -> Lyrics {
    // PMS returns an array here; tolerate a bare object too.
    let first = match value {
        serde_json::Value::Array(mut a) if !a.is_empty() => a.remove(0),
        v @ serde_json::Value::Object(_) => v,
        _ => return Lyrics::default(),
    };
    let parsed: PlexLyrics = serde_json::from_value(first).unwrap_or_default();
    Lyrics {
        provider: parsed.provider,
        timed: parsed.timed.as_ref().map(truthy).unwrap_or(false),
        lines: parsed
            .line
            .into_iter()
            .filter_map(|l| {
                // Timed lyrics include zero-length marker lines carrying no
                // Span at all; they would render as blank rows and could take
                // the highlight away from a real line.
                let text = l
                    .span
                    .into_iter()
                    .filter_map(|s| s.text)
                    .collect::<Vec<_>>()
                    .join("");
                (!text.trim().is_empty()).then_some(LyricLine {
                    start_ms: l.start_offset,
                    text,
                })
            })
            .collect(),
    }
}

// PMS is inconsistent about whether flags come back as 1, "1" or true.
fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => s == "1" || s.eq_ignore_ascii_case("true"),
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        _ => false,
    }
}

// POST /api/repeat?v=0|1|2 — off, this track, the queue.
async fn repeat_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> StatusCode {
    let v = params.get("v").and_then(|v| v.parse().ok()).unwrap_or(0);
    send_cmd(&state, PlayerCmd::SetRepeat(v))
}

// POST /api/shuffle?v=0|1 — a playback mode, not a queue edit: the queue
// keeps its order and the player picks from it at random.
async fn shuffle_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> StatusCode {
    let on = params.get("v").map(|v| v == "1").unwrap_or(false);
    send_cmd(&state, PlayerCmd::SetShuffle(on))
}

#[derive(Serialize)]
struct LightStatus {
    generation: u64,
    state: String,
    position_ms: u64,
    volume: u8,
    unchanged: bool,
}

// GET /api/status[?gen=N] — full state, or a tiny "nothing changed since
// generation N" payload so the 1s poll stays cheap on the Pi.
async fn status_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> axum::response::Response {
    let s = state.status.lock().unwrap();
    let seen: Option<u64> = params.get("gen").and_then(|g| g.parse().ok());
    if seen == Some(s.generation) {
        return Json(LightStatus {
            generation: s.generation,
            state: s.state.clone(),
            position_ms: s.position_ms,
            volume: s.volume,
            unchanged: true,
        })
        .into_response();
    }
    Json(s.clone()).into_response()
}

// Album art proxy: the browser has no Plex token, so images are fetched
// server-side through PMS's photo transcoder (which also resizes them).
async fn thumb_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> axum::response::Response {
    let Some(path) = params.get("path") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let size: u32 = params.get("size").and_then(|v| v.parse().ok()).unwrap_or(300);
    let cache_key = format!("{size}:{path}");
    if let Some((ct, bytes)) = state.thumb_cache.lock().unwrap().get(&cache_key) {
        return (
            [
                ("Content-Type", ct),
                ("Cache-Control", "public, max-age=86400".to_string()),
            ],
            bytes,
        )
            .into_response();
    }
    let url = format!(
        "{}/photo/:/transcode?width={size}&height={size}&minSize=1&upscale=1&url={}&X-Plex-Token={}",
        state.server_url,
        urlencoding::encode(path),
        state.token
    );
    match state.client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("image/jpeg")
                .to_string();
            match resp.bytes().await {
                Ok(b) => {
                    let bytes = b.to_vec();
                    state
                        .thumb_cache
                        .lock()
                        .unwrap()
                        .put(cache_key, ct.clone(), bytes.clone());
                    (
                        [
                            ("Content-Type", ct),
                            ("Cache-Control", "public, max-age=86400".to_string()),
                        ],
                        bytes,
                    )
                        .into_response()
                }
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---- Plex Companion (remote-control) protocol ----

type Params = std::collections::HashMap<String, String>;

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// Every companion response must carry our identifier, or the controller
// ignores it.
fn plex_xml(body: String) -> axum::response::Response {
    (
        [
            ("Content-Type", "text/xml;charset=utf-8".to_string()),
            ("X-Plex-Client-Identifier", client_identifier().to_string()),
            ("X-Plex-Protocol", "1.0".to_string()),
            ("Access-Control-Allow-Origin", "*".to_string()),
        ],
        body,
    )
        .into_response()
}

// Companion replies are XML. Answering a controller with a bare status and a
// text body makes it report a generic failure, and tells us nothing about why.
fn plex_err(context: &str, e: anyhow::Error) -> axum::response::Response {
    eprintln!("companion {context} failed: {e:#}");
    plex_xml(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Response code="500" status="{}"/>"#,
        xml_escape(&e.to_string())
    ))
}

// What the phone actually sent. Off unless RUSTAMP_DEBUG_COMPANION is set,
// because this is per-request logging; it is the only way to see a controller's
// side of a conversation that otherwise fails silently.
fn log_companion(path: &str, params: &Params) {
    if std::env::var_os("RUSTAMP_DEBUG_COMPANION").is_none() {
        return;
    }
    let mut kv: Vec<String> = params
        .iter()
        .map(|(k, v)| {
            let shown = if k.eq_ignore_ascii_case("token") || k.contains("Token") {
                "…"
            } else {
                v.as_str()
            };
            format!("{k}={shown}")
        })
        .collect();
    kv.sort();
    eprintln!("companion {path} {}", kv.join(" "));
}

fn plex_ok() -> axum::response::Response {
    plex_xml(r#"<?xml version="1.0" encoding="UTF-8"?><Response code="200" status="OK"/>"#.into())
}

async fn resources_handler(State(state): State<AppState>) -> impl IntoResponse {
    let xml = format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<MediaContainer size="1">"#,
            r#"<Player machineIdentifier="{id}" deviceClass="{class}" platform="{platform}" "#,
            r#"platformVersion="1" product="{product}" protocol="plex" "#,
            r#"protocolVersion="{pv}" protocolCapabilities="{caps}" "#,
            r#"title="{title}" version="{version}"/>"#,
            r#"</MediaContainer>"#,
        ),
        id = client_identifier(),
        class = DEVICE_CLASS,
        platform = std::env::consts::OS,
        product = PRODUCT_NAME,
        pv = PROTOCOL_VERSION,
        caps = PROTOCOL_CAPABILITIES,
        title = xml_escape(&state.player_name),
        version = VERSION,
    );
    plex_xml(xml)
}

fn build_timeline_xml(status: &PlayerStatus, command_id: &str) -> String {
    let mut attrs = vec![
        ("type", "music".to_string()),
        ("itemType", "music".to_string()),
        ("state", status.state.clone()),
        ("volume", status.volume.to_string()),
        (
            "shuffle",
            if status.shuffle { "1" } else { "0" }.to_string(),
        ),
        ("repeat", status.repeat.to_string()),
    ];

    let mut controllable = vec![
        "volume",
        "repeat",
        "shuffle",
        "skipPrevious",
        "seekTo",
        "stepBack",
        "stepForward",
        "stop",
        "playPause",
    ];

    if let Some(idx) = status.current_index {
        if let Some(track) = status.queue.get(idx) {
            attrs.push(("time", status.position_ms.to_string()));
            attrs.push(("duration", track.duration_ms.to_string()));
            attrs.push(("key", track.key.clone()));
            attrs.push(("ratingKey", track.rating_key.clone()));
            if let Some(id) = track.play_queue_item_id {
                attrs.push(("playQueueItemID", id.to_string()));
            }
        }
        if idx + 1 < status.queue.len() {
            controllable.push("skipNext");
        }
        let info = &status.info;
        if !info.machine_identifier.is_empty() {
            attrs.push(("machineIdentifier", info.machine_identifier.clone()));
            attrs.push(("protocol", info.protocol.clone()));
            attrs.push(("address", info.address.clone()));
            attrs.push(("port", info.port.to_string()));
        }
        if let Some(id) = info.play_queue_id {
            attrs.push(("playQueueID", id.to_string()));
        }
        if let Some(v) = info.play_queue_version {
            attrs.push(("playQueueVersion", v.to_string()));
        }
        if let Some(ck) = &info.container_key {
            attrs.push(("containerKey", ck.clone()));
        }
    }
    attrs.push(("controllable", controllable.join(",")));

    let music: String = attrs
        .iter()
        .map(|(k, v)| format!(r#" {k}="{}""#, xml_escape(v)))
        .collect();

    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<MediaContainer commandID="{cid}" location="navigation">"#,
            r#"<Timeline{music}/>"#,
            r#"<Timeline type="video" state="stopped"/>"#,
            r#"<Timeline type="photo" state="stopped"/>"#,
            r#"</MediaContainer>"#,
        ),
        cid = xml_escape(command_id),
        music = music,
    )
}

async fn timeline_poll_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> impl IntoResponse {
    let command_id = params.get("commandID").cloned().unwrap_or_default();
    let wait = params.get("wait").map(|w| w == "1").unwrap_or(false);

    if wait {
        // Long-poll: hold until something changes, but never longer than the
        // controller's ~10s request timeout.
        let start_gen = state.status.lock().unwrap().generation;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if state.status.lock().unwrap().generation != start_gen {
                break;
            }
        }
    }

    let snapshot = state.status.lock().unwrap().clone();
    plex_xml(build_timeline_xml(&snapshot, &command_id))
}

async fn timeline_subscribe_handler() -> impl IntoResponse {
    // We don't push timelines to subscribers; controllers that subscribe
    // also poll, which is what Plexamp does.
    plex_ok()
}

// playMedia / createPlayQueue: the controller has already built a play
// queue on the server; we fetch it and play from the selected item.
async fn play_media_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> impl IntoResponse {
    let protocol = params.get("protocol").cloned().unwrap_or_else(|| "http".into());
    let address = params.get("address").cloned();
    let port = params.get("port").cloned();
    let token = params
        .get("token")
        .cloned()
        .unwrap_or_else(|| state.token.clone());
    let machine_id = params
        .get("machineIdentifier")
        .cloned()
        .unwrap_or_else(|| state.server_machine_id.clone());
    let offset_ms: u64 = params
        .get("offset")
        .and_then(|o| o.parse().ok())
        .unwrap_or(0);
    let paused = params.get("paused").map(|p| p == "1").unwrap_or(false);
    log_companion("playMedia", &params);

    let server_url = match (&address, &port) {
        (Some(a), Some(p)) => format!("{protocol}://{a}:{p}"),
        _ => state.server_url.clone(),
    };
    let (proto, addr, port_num) = split_url(&server_url);

    let container_key = params.get("containerKey").cloned();
    let key = params.get("key").cloned();
    // The phone sends a literal "undefined" for absent values; treat an empty
    // or "undefined" uri as no uri at all.
    let uri = params
        .get("uri")
        .filter(|u| !u.is_empty() && u.as_str() != "undefined")
        .cloned();

    // The controller names its server by machineIdentifier and hands us one of
    // its addresses, usually a plex.direct HTTPS name. When that is the server
    // we are already configured for, build the queue over our own connection:
    // it is known to work from here, and does not depend on the hashed
    // hostname resolving or on TLS to a LAN address.
    let (queue_server, queue_token) = if machine_id == state.server_machine_id {
        (state.server_url.clone(), state.token.clone())
    } else {
        (server_url.clone(), token.clone())
    };

    let (tracks, start_index, info) = match &container_key {
        Some(ck) if ck.contains("/playQueues/") => {
            // Strip any existing query, then ask for the whole queue.
            let base = ck.split('?').next().unwrap_or(ck);
            let url = format!(
                "{server_url}{base}?own=1&includeChapters=1&includeRelated=0&window={QUEUE_WINDOW}"
            );
            let r: MetadataResponse =
                match plex_get_json(&state.client, &url, &token).await {
                    Ok(r) => r,
                    Err(e) => return plex_err("playMedia (reading the queue)", e),
                };
            let mc = r.media_container;
            let selected = mc.play_queue_selected_item_id;
            let start = mc
                .metadata
                .iter()
                .position(|m| m.play_queue_item_id.is_some() && m.play_queue_item_id == selected)
                .or_else(|| {
                    key.as_ref()
                        .and_then(|k| mc.metadata.iter().position(|m| m.key.as_deref() == Some(k)))
                })
                .unwrap_or(0);
            let info = QueueInfo {
                machine_identifier: machine_id,
                address: addr,
                port: port_num,
                protocol: proto,
                container_key: Some(base.to_string()),
                play_queue_id: mc.play_queue_id,
                play_queue_version: mc.play_queue_version,
            };
            (mc.metadata, start, info)
        }
        // No existing queue, but the controller named a source to build one
        // from. This is how the phone starts playback when we are idle: it
        // sends `uri` with no containerKey, and without this we answered
        // "missing key" and it reported "Can't start playback".
        _ if uri.is_some() => {
            let uri = uri.as_deref().unwrap_or_default();
            let shuffle = params.get("shuffle").map(|v| v == "1").unwrap_or(false);
            match create_play_queue_on(
                &state,
                &queue_server,
                &queue_token,
                uri,
                key.as_deref(),
                shuffle,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => return plex_err("playMedia (building the queue)", e),
            }
        }
        _ => {
            // No play queue and no source: play the single item named by `key`.
            let Some(k) = key else {
                return plex_err(
                    "playMedia",
                    anyhow::anyhow!("no containerKey, uri or key in the request"),
                );
            };
            let rating_key = k.rsplit('/').next().unwrap_or(&k).to_string();
            let track = match fetch_item(&state.client, &server_url, &token, &rating_key).await {
                Ok(t) => t,
                Err(e) => return plex_err("playMedia (reading the item)", e),
            };
            let info = QueueInfo {
                machine_identifier: machine_id,
                address: addr,
                port: port_num,
                protocol: proto,
                container_key: None,
                play_queue_id: None,
                play_queue_version: None,
            };
            (vec![track], 0, info)
        }
    };

    match start_queue(
        &state,
        server_url,
        token,
        tracks,
        start_index,
        offset_ms,
        info,
        paused,
    )
    .await
    {
        Ok(()) => plex_ok(),
        Err(e) => plex_err("playMedia (starting playback)", e),
    }
}

async fn playback_cmd_handler(
    State(state): State<AppState>,
    Path(cmd): Path<String>,
    axum::extract::Query(params): axum::extract::Query<Params>,
) -> impl IntoResponse {
    log_companion(&cmd, &params);
    match cmd.as_str() {
        "play" => send_cmd(&state, PlayerCmd::Resume),
        "pause" => send_cmd(&state, PlayerCmd::Pause),
        "playPause" => send_cmd(&state, PlayerCmd::PlayPause),
        "stop" => send_cmd(&state, PlayerCmd::Stop),
        "skipNext" => send_cmd(&state, PlayerCmd::SkipOne),
        "skipPrevious" => send_cmd(&state, PlayerCmd::SkipPrevious),
        "seekTo" => {
            let ms: u64 = params
                .get("offset")
                .and_then(|o| o.parse().ok())
                .unwrap_or(0);
            send_cmd(&state, PlayerCmd::Seek(ms))
        }
        "skipTo" => {
            let target = params.get("playQueueItemID").and_then(|s| s.parse::<u64>().ok());
            let key = params.get("key").cloned();
            let idx = {
                let s = state.status.lock().unwrap();
                s.queue.iter().position(|t| {
                    (target.is_some() && t.play_queue_item_id == target)
                        || (key.is_some() && Some(&t.key) == key.as_ref())
                })
            };
            match idx {
                Some(i) => send_cmd(&state, PlayerCmd::SkipTo(i)),
                None => StatusCode::NOT_FOUND,
            }
        }
        "setParameters" => {
            if let Some(v) = params.get("volume").and_then(|v| v.parse::<u8>().ok()) {
                let _ = state.player_tx.send(PlayerCmd::SetVolume(v));
            }
            if let Some(r) = params.get("repeat").and_then(|v| v.parse::<u8>().ok()) {
                let _ = state.player_tx.send(PlayerCmd::SetRepeat(r));
            }
            if let Some(s) = params.get("shuffle") {
                let _ = state.player_tx.send(PlayerCmd::SetShuffle(s == "1"));
            }
            StatusCode::OK
        }
        "refreshPlayQueue" => {
            let pq_id = params
                .get("playQueueID")
                .and_then(|v| v.parse::<u64>().ok())
                .or_else(|| state.status.lock().unwrap().info.play_queue_id);
            if let Some(id) = pq_id {
                // Plain read — no own=1, which would claim the queue for
                // this client and can retire the one we're holding.
                let queue_server = state.queue_source.lock().unwrap().0.clone();
                let url = format!(
                    "{queue_server}/playQueues/{id}?includeChapters=1&window={QUEUE_WINDOW}"
                );
                match queue_request(&state, reqwest::Method::GET, &url).await {
                    Ok(mc) => apply_queue_edit(&state, mc),
                    Err(e) => eprintln!("queue refresh failed: {e}"),
                }
            }
            StatusCode::OK
        }
        other => {
            // Acknowledged anyway, because a controller that gets a non-Plex
            // reply reports a generic failure; but say so, since otherwise an
            // unimplemented command is indistinguishable from one that worked.
            eprintln!("companion command not implemented: {other}");
            StatusCode::OK
        }
    };
    plex_ok()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = reqwest::Client::new();

    let cfg = match load_config() {
        Some(cfg) => {
            CLIENT_IDENTIFIER.set(cfg.client_identifier.clone()).ok();
            println!("Loaded config for player '{}'.", cfg.player_name);
            cfg
        }
        None => {
            CLIENT_IDENTIFIER.set(Uuid::new_v4().to_string()).ok();
            let cfg = first_run_setup(&client).await?;
            save_config(&cfg)?;
            println!("Saved config to {:?}", config_path()?);
            cfg
        }
    };

    let server_machine_id = match fetch_server_machine_id(&client, &cfg.server_url, &cfg.token).await
    {
        Ok(id) => id,
        Err(e) => {
            eprintln!("could not read server identity ({e}); timelines will lack machineIdentifier");
            String::new()
        }
    };

    spawn_registration_task(client.clone(), cfg.clone());

    let status: SharedStatus = Arc::new(Mutex::new(PlayerStatus {
        volume: 100,
        state: "stopped".into(),
        ..Default::default()
    }));
    let player_tx = spawn_player_thread(status.clone());

    let state = AppState {
        client,
        server_url: cfg.server_url.clone(),
        server_machine_id,
        token: cfg.token.clone(),
        player_name: cfg.player_name.clone(),
        player_tx,
        status,
        queue_serial: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        thumb_cache: Arc::new(Mutex::new(ThumbCache::default())),
        listing_cache: Arc::new(Mutex::new(ListingCache::default())),
        queue_source: Arc::new(Mutex::new((cfg.server_url.clone(), cfg.token.clone()))),
    };

    spawn_timeline_reporter(&state);

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/resources", get(resources_handler))
        .route("/player/timeline/poll", get(timeline_poll_handler))
        .route("/player/timeline/subscribe", get(timeline_subscribe_handler))
        .route("/player/timeline/unsubscribe", get(timeline_subscribe_handler))
        .route("/player/playback/playMedia", get(play_media_handler))
        .route("/player/playback/createPlayQueue", get(play_media_handler))
        .route("/player/playback/:cmd", get(playback_cmd_handler))
        .route("/api/sections", get(sections_handler))
        .route("/api/sections/:key", get(section_items_handler))
        .route("/api/browse/:rating_key", get(browse_handler))
        .route("/api/sections/:key/albums", get(section_albums_handler))
        .route("/api/sections/:key/tracks", get(section_tracks_handler))
        .route("/api/shuffle-library/:key", post(shuffle_library_handler))
        .route("/api/rate", post(rate_handler))
        .route("/api/lyrics/:rating_key", get(lyrics_handler))
        .route("/api/search", get(search_handler))
        .route("/api/play/:rating_key", post(play_handler))
        .route("/api/play-album/:rating_key", post(play_album_handler))
        .route("/api/pause", post(pause_handler))
        .route("/api/resume", post(resume_handler))
        .route("/api/skip", post(skip_handler))
        .route("/api/stop", post(stop_handler))
        .route("/api/playpause", post(playpause_handler))
        .route("/api/prev", post(prev_handler))
        .route("/api/seek", post(seek_handler))
        .route("/api/volume", post(volume_handler))
        .route("/api/skipto", post(skipto_handler))
        .route("/api/repeat", post(repeat_handler))
        .route("/api/shuffle", post(shuffle_handler))
        .route("/api/queue/add", post(queue_add_handler))
        .route("/api/queue/remove", post(queue_remove_handler))
        .route("/api/status", get(status_handler))
        .route("/api/thumb", get(thumb_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", LISTEN_PORT)).await?;
    println!("RustAmp listening on http://0.0.0.0:{LISTEN_PORT}");
    axum::serve(listener, app).await?;

    Ok(())
}
