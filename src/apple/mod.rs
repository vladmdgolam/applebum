//! Apple Music in place of Spotify: the catalog behind `ApiRequest`, and
//! MusicKit's player behind `PlayerCommand`.
//!
//! The interface keeps speaking the app's existing models. Songs, albums and
//! artists keep `spotify:<kind>:<id>` URIs internally, with Apple's ID in the
//! last part, because the views and the queue route on those prefixes. They
//! never reach Spotify: a request this module doesn't answer falls through to
//! the Spotify path, which has no session and reports `NotSignedIn`.

use std::ffi::{CString, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::api::ApiError;
use crate::api::models::{
    Album, Artist, ArtistRef, CursorPage, Cursors, Device, Image, Owner, Page, PlayHistory, PlayableItem,
    Playlist, PlaylistItem, Queue, SavedAlbum, SavedTrack, SearchResults, Track, TrackCount, User,
};
use crate::backend::{ApiRequest, ApiResponse, ApiResult, AuthStatus, Event, LocalPlayback, Waker};
use crate::player::{LoadSpec, LocalState, LocalTrack, Playback, PlayerCommand, RepeatMode};

pub const DEVICE_ID: &str = "apple-music";


type Reply = extern "C" fn(*mut c_void, i32, *const u8, usize);

unsafe extern "C" {
    fn am_authorize(context: *mut c_void, reply: Reply);
    fn am_request(path: *const c_char, context: *mut c_void, reply: Reply);
    fn am_load(json: *const c_char, context: *mut c_void, reply: Reply);
    fn am_control(operation: i32, value: f64);
    fn am_stop();
    fn am_skip(count: i32);
    fn am_state(context: *mut c_void, reply: Reply);
    fn am_send(
        method: *const c_char,
        path: *const c_char,
        body: *const c_char,
        context: *mut c_void,
        reply: Reply,
    );
    fn am_speakers_start();
    fn am_speakers(context: *mut c_void, reply: Reply);
    fn am_speaker_resolve(name: *const c_char, context: *mut c_void, reply: Reply);
    fn am_airplay_start(
        host: *const c_char,
        port: u16,
        name: *const c_char,
        context: *mut c_void,
        event: extern "C" fn(*mut c_void, i32, *const c_char),
    ) -> *mut c_void;
    fn am_airplay_push(session: *mut c_void, samples: *const f32, count: usize);
    fn am_airplay_volume(session: *mut c_void, percent: f64);
    fn am_airplay_stop(session: *mut c_void);
    fn am_requeue(json: *const c_char, context: *mut c_void, reply: Reply);
    fn am_enqueue(id: *const c_char, context: *mut c_void, reply: Reply);
    fn am_queue(context: *mut c_void, reply: Reply);
}

extern "C" fn on_reply(context: *mut c_void, status: i32, data: *const u8, length: usize) {
    // SAFETY: `call` boxed this sender for exactly one reply.
    let sender = unsafe { Box::from_raw(context.cast::<oneshot::Sender<(i32, Vec<u8>)>>()) };
    let bytes = if data.is_null() || length == 0 {
        Vec::new()
    } else {
        // SAFETY: Swift keeps the buffer alive for the duration of this call.
        unsafe { std::slice::from_raw_parts(data, length) }.to_vec()
    };
    let _ = sender.send((status, bytes));
}

async fn call(start: impl FnOnce(*mut c_void, Reply)) -> (i32, Vec<u8>) {
    let (sender, receiver) = oneshot::channel();
    start(Box::into_raw(Box::new(sender)).cast(), on_reply);
    receiver
        .await
        .unwrap_or((-1, b"the bridge dropped the reply".to_vec()))
}

fn c_string(text: &str) -> CString {
    CString::new(text.replace('\0', "")).unwrap_or_default()
}

/// A GET against the Apple Music API, `{storefront}` standing for the user's.
async fn get(path: &str) -> ApiResult<Value> {
    let path = c_string(path);
    let (status, body) = call(|context, reply| unsafe { am_request(path.as_ptr(), context, reply) }).await;
    if (200..300).contains(&status) {
        serde_json::from_slice(&body).map_err(|error| ApiError::Decode(error.to_string()))
    } else {
        Err(ApiError::Status {
            status: status.max(0) as u16,
            message: format!(
                "Apple Music returned {status}: {}",
                String::from_utf8_lossy(&body)
            ),
        })
    }
}

/// A change to the user's account: POST, PUT or DELETE.
async fn send(method: &str, path: &str, body: &str) -> ApiResult<()> {
    let (method, path, body) = (c_string(method), c_string(path), c_string(body));
    let (status, reply) = call(|context, reply| unsafe {
        am_send(method.as_ptr(), path.as_ptr(), body.as_ptr(), context, reply)
    })
    .await;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(ApiError::Status {
            status: status.max(0) as u16,
            message: format!(
                "Apple Music returned {status}: {}",
                String::from_utf8_lossy(&reply)
            ),
        })
    }
}

/// A rateable song or album as (Apple's resource type, catalog ID).
fn rateable(uri: &str) -> Option<(&'static str, &str)> {
    let (kind, id) = if let Some(id) = uri.strip_prefix("spotify:track:") {
        ("songs", id)
    } else {
        ("albums", uri.strip_prefix("spotify:album:")?)
    };
    // Only catalog items have ratings here; library IDs start with a letter.
    id.chars().all(|c| c.is_ascii_digit()).then_some((kind, id))
}

/// The heart is a love rating, which can be taken back, and a loved song joins
/// the library. Apple has no call to remove it from the library again.
async fn set_loved(uris: &[String], loved: bool) -> ApiResult<()> {
    for uri in uris {
        let Some((kind, id)) = rateable(uri) else {
            return Err(ApiError::Status {
                status: 400,
                message: "This item can't be favorited yet".to_owned(),
            });
        };
        let path = format!("/v1/me/ratings/{kind}/{id}");
        if loved {
            let rating = json!({"type": "rating", "attributes": {"value": 1}}).to_string();
            send("PUT", &path, &rating).await?;
            send("POST", &format!("/v1/me/library?ids[{kind}]={id}"), "").await?;
        } else {
            send("DELETE", &path, "").await?;
        }
    }
    Ok(())
}

async fn are_loved(uris: &[String]) -> ApiResult<Vec<bool>> {
    let mut loved = std::collections::HashSet::new();
    for kind in ["songs", "albums"] {
        let ids: Vec<&str> = uris
            .iter()
            .filter_map(|uri| rateable(uri))
            .filter(|(found, _)| *found == kind)
            .map(|(_, id)| id)
            .collect();
        for chunk in ids.chunks(100) {
            let found = or_empty(get(&format!("/v1/me/ratings/{kind}?ids={}", chunk.join(","))).await)?;
            for rating in found["data"].as_array().into_iter().flatten() {
                if rating["attributes"]["value"].as_i64() == Some(1) {
                    loved.insert(text(rating, "id"));
                }
            }
        }
    }
    Ok(uris
        .iter()
        .map(|uri| rateable(uri).is_some_and(|(_, id)| loved.contains(id)))
        .collect())
}

fn track_from_player(song: &Value) -> Track {
    let id = text(song, "id");
    let art = text(song, "art");
    let artist = text(song, "artist");
    Track {
        uri: format!("spotify:track:{id}"),
        id: Some(id),
        name: text(song, "title"),
        duration_ms: song["durationMs"].as_u64().unwrap_or_default() as u32,
        artists: vec![ArtistRef {
            name: artist,
            ..Default::default()
        }],
        album: Some(Album {
            name: text(song, "album"),
            images: if art.is_empty() {
                Vec::new()
            } else {
                vec![Image {
                    url: art,
                    width: Some(640),
                    height: Some(640),
                }]
            },
            ..Default::default()
        }),
        is_playable: Some(true),
        ..Default::default()
    }
}

/// A song earlier in the queue than the one on.
#[derive(Clone)]
pub struct PlayedSong {
    pub title: String,
    pub artist: String,
    pub uri: String,
    pub duration_s: f64,
}

/// The queue's model has no place for songs already passed, but a skin's list
/// shows the whole queue, with the song on in its place.
static PLAYED: std::sync::Mutex<Vec<PlayedSong>> = std::sync::Mutex::new(Vec::new());

pub fn played_songs() -> Vec<PlayedSong> {
    PLAYED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// Moves through the queue by this many songs, back if negative.
pub fn jump(songs: i32) {
    // SAFETY: am_skip only reads its plain argument.
    unsafe { am_skip(songs) };
}

async fn player_queue() -> ApiResult<Queue> {
    let (status, body) = call(|context, reply| unsafe { am_queue(context, reply) }).await;
    if status != 200 {
        return Err(ApiError::Decode(String::from_utf8_lossy(&body).into_owned()));
    }
    let state: Value =
        serde_json::from_slice(&body).map_err(|error| ApiError::Decode(error.to_string()))?;
    let before: Vec<PlayedSong> = state["before"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|song| PlayedSong {
            title: text(song, "title"),
            artist: text(song, "artist"),
            uri: format!("spotify:track:{}", text(song, "id")),
            duration_s: song["durationMs"].as_f64().unwrap_or_default() / 1000.0,
        })
        .collect();
    log::info!("apple: queue has {} songs before the current one", before.len());
    *PLAYED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = before;
    Ok(Queue {
        currently_playing: state
            .get("current")
            .map(|song| PlayableItem::Track(track_from_player(song))),
        queue: state["next"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|song| PlayableItem::Track(track_from_player(song)))
            .collect(),
    })
}

async fn enqueue(uri: &str) -> ApiResult<()> {
    let Some(id) = song_id(uri) else {
        return Err(ApiError::Decode("Only songs can be queued".into()));
    };
    let id = c_string(id);
    let (status, body) = call(|context, reply| unsafe { am_enqueue(id.as_ptr(), context, reply) }).await;
    if status == 200 {
        Ok(())
    } else {
        Err(ApiError::Decode(String::from_utf8_lossy(&body).into_owned()))
    }
}

// ---- AirPlay ----

const SPEAKER_PREFIX: &str = "airplay-";

/// Device IDs are limited to letters, digits, `-` and `_`, so a speaker's name
/// travels in hex: "Living Room" has a space.
fn speaker_id(name: &str) -> String {
    let hex: String = name.bytes().map(|byte| format!("{byte:02x}")).collect();
    format!("{SPEAKER_PREFIX}{hex}")
}

fn speaker_name(id: &str) -> Option<String> {
    let hex = id.strip_prefix(SPEAKER_PREFIX)?;
    let bytes: Option<Vec<u8>> = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

struct Speaker {
    id: usize,
    name: String,
    session: *mut c_void,
}
// SAFETY: the session is only used under `SPEAKER`'s lock, and its C++ side is
// built for one thread pushing audio while another runs the sender.
unsafe impl Send for Speaker {}

/// The speaker this app's audio goes to, if any.
static SPEAKER: std::sync::Mutex<Option<Speaker>> = std::sync::Mutex::new(None);
static SPEAKER_STARTED: std::sync::Mutex<Option<(usize, oneshot::Sender<Result<(), String>>)>> =
    std::sync::Mutex::new(None);
static NEXT_SESSION: AtomicUsize = AtomicUsize::new(1);
static ROUTE_TURN: AtomicU64 = AtomicU64::new(0);
static ROUTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// A speaker's level on arrival, in percent, at most.
const SPEAKER_START_VOLUME: u8 = 30;
static EVENTS: std::sync::OnceLock<(std::sync::mpsc::Sender<Event>, Waker)> =
    std::sync::OnceLock::new();

fn speaker_lock() -> std::sync::MutexGuard<'static, Option<Speaker>> {
    SPEAKER.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

extern "C" fn on_speaker_event(context: *mut c_void, kind: i32, message: *const c_char) {
    // The context is the session's number, not a pointer.
    let id = context as usize;
    let message = if message.is_null() {
        String::new()
    } else {
        // SAFETY: the sender passes a C string, valid for this call.
        unsafe { std::ffi::CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    };
    let starting = {
        let mut started = SPEAKER_STARTED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if started.as_ref().is_some_and(|(waiting, _)| *waiting == id) {
            started.take().map(|(_, sender)| sender)
        } else {
            None
        }
    };
    if let Some(starting) = starting {
        let _ = starting.send(if kind == 1 { Ok(()) } else { Err(message) });
    } else if kind != 1 {
        // The speaker went away mid-song: the music comes back on this Mac. The
        // session is torn down elsewhere, since this runs on its own thread.
        std::thread::spawn(move || {
            if leave_speaker(Some(id))
                && let Some((events, waker)) = EVENTS.get()
            {
                emit(
                    events,
                    waker,
                    Event::Error("Lost the speaker; playing on this Mac".to_owned()),
                );
            }
        });
    }
}

/// Back to this Mac, from this session if one is named, or from any. Says whether
/// it left one. The sound moves first, so it's never silent in between.
fn leave_speaker(only: Option<usize>) -> bool {
    let leaving = {
        let mut speaker = speaker_lock();
        if only.is_some() && speaker.as_ref().map(|speaker| speaker.id) != only {
            return false;
        }
        SPEAKER_ON.store(false, Ordering::Release);
        speaker.take()
    };
    let Some(speaker) = leaving else {
        return false;
    };
    // SAFETY: taking it out of `SPEAKER` makes this the session's last use.
    unsafe { am_airplay_stop(speaker.session) };
    log::info!("apple: stopped streaming to {}", speaker.name);
    true
}

async fn route_to(device_id: &str) -> ApiResult<()> {
    // One change of output at a time; of several waiting, only the newest matters.
    let turn = ROUTE_TURN.fetch_add(1, Ordering::SeqCst) + 1;
    let _one_at_a_time = ROUTING.lock().await;
    if ROUTE_TURN.load(Ordering::SeqCst) != turn {
        return Ok(());
    }
    let failed = |message: String| ApiError::Status { status: 0, message };
    let Some(name) = speaker_name(device_id) else {
        // Stopping joins the sender's thread, which flushes its goodbye.
        let _ = tokio::task::spawn_blocking(|| leave_speaker(None)).await;
        return Ok(());
    };
    let name = name.as_str();
    let began = Instant::now();
    let _ = tokio::task::spawn_blocking(|| leave_speaker(None)).await;
    let wanted = c_string(name);
    let (status, body) =
        call(|context, reply| unsafe { am_speaker_resolve(wanted.as_ptr(), context, reply) }).await;
    if status != 200 {
        return Err(failed(String::from_utf8_lossy(&body).into_owned()));
    }
    let address: Value =
        serde_json::from_slice(&body).map_err(|error| failed(error.to_string()))?;
    let host = c_string(address["host"].as_str().unwrap_or_default());
    let port = address["port"].as_u64().unwrap_or(7000) as u16;
    log::info!("apple: connecting to {name} at {host:?}:{port}");
    let id = NEXT_SESSION.fetch_add(1, Ordering::SeqCst);
    let (started, starting) = oneshot::channel();
    *SPEAKER_STARTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((id, started));
    // SAFETY: the sender copies its strings before returning. The context only
    // carries the session's number back and is never dereferenced.
    let session = unsafe {
        am_airplay_start(host.as_ptr(), port, wanted.as_ptr(), id as *mut c_void, on_speaker_event)
    };
    *speaker_lock() = Some(Speaker {
        id,
        name: name.to_owned(),
        session,
    });
    let outcome = tokio::time::timeout(Duration::from_secs(15), starting).await;
    let reason = match outcome {
        Ok(Ok(Ok(()))) => {
            let mut speaker = speaker_lock();
            // It can already have closed again.
            if let Some(speaker) = speaker.as_mut().filter(|speaker| speaker.id == id) {
                // A speaker's own level can't be read, so it's set: quiet enough
                // that going to a speaker is never a surprise.
                let level = MAC_VOLUME.load(Ordering::Relaxed).min(SPEAKER_START_VOLUME);
                SPEAKER_VOLUME.store(level, Ordering::Relaxed);
                // SAFETY: the lock keeps the session alive for this call.
                unsafe { am_airplay_volume(speaker.session, f64::from(level)) };
                // Only now: had the speaker failed, the music would have kept playing here.
                SPEAKER_ON.store(true, Ordering::Release);
                log::info!("apple: streaming to {name}, {} ms to connect", began.elapsed().as_millis());
                return Ok(());
            }
            "it closed the session".to_owned()
        }
        Ok(Ok(Err(reason))) => reason,
        _ => "it didn't answer in time".to_owned(),
    };
    SPEAKER_STARTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let _ = tokio::task::spawn_blocking(move || leave_speaker(Some(id))).await;
    log::warn!("apple: couldn't stream to {name}: {reason}");
    Err(failed(format!("Couldn't connect to {name}: {reason}")))
}

async fn speakers() -> ApiResult<Vec<Device>> {
    let (_, body) = call(|context, reply| unsafe { am_speakers(context, reply) }).await;
    let found: Value = serde_json::from_slice(&body).unwrap_or_default();
    log::info!("apple: speakers on the network: {}", found["names"]);
    let current = speaker_lock().as_ref().map(|speaker| speaker.name.clone());
    Ok(found["names"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str())
        .map(|name| Device {
            id: Some(speaker_id(name)),
            name: name.to_owned(),
            is_active: current.as_deref() == Some(name),
            supports_volume: Some(true),
            kind: "Speaker".to_owned(),
            ..Default::default()
        })
        .collect())
}

fn encode(term: &str) -> String {
    urlencoding::encode(term).into_owned()
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

fn images(attributes: &Value) -> Vec<Image> {
    let Some(template) = attributes["artwork"]["url"].as_str() else {
        return Vec::new();
    };
    [640u32, 300, 64]
        .into_iter()
        .map(|size| Image {
            url: template
                .replace("{w}", &size.to_string())
                .replace("{h}", &size.to_string()),
            width: Some(size),
            height: Some(size),
        })
        .collect()
}

/// `.../album/name/123?i=456` names the album a song belongs to.
fn album_id_in(url: &str) -> Option<String> {
    let path = url.split('?').next()?;
    let (_, rest) = path.split_once("/album/")?;
    rest.rsplit('/')
        .next()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn artist_refs(resource: &Value, name: &str) -> Vec<ArtistRef> {
    let linked = resource["relationships"]["artists"]["data"].as_array();
    match linked.and_then(|artists| artists.first()) {
        // Apple joins every credit into `artistName`; the first link is the lead.
        Some(first) => {
            let id = text(first, "id");
            vec![ArtistRef {
                uri: Some(format!("spotify:artist:{id}")),
                id: Some(id),
                name: name.to_owned(),
                ..Default::default()
            }]
        }
        None => vec![ArtistRef {
            name: name.to_owned(),
            ..Default::default()
        }],
    }
}

fn track(resource: &Value) -> Track {
    let attributes = &resource["attributes"];
    // A library song plays as its catalog song when it has one; an upload
    // only exists under its library ID.
    let id = attributes["playParams"]["catalogId"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| text(resource, "id"));
    let artist = text(attributes, "artistName");
    let url = text(attributes, "url");
    let linked_album = resource["relationships"]["albums"]["data"][0]["id"]
        .as_str()
        .map(str::to_owned);
    let album_id = linked_album.or_else(|| album_id_in(&url)).unwrap_or_default();
    Track {
        uri: format!("spotify:track:{id}"),
        id: Some(id),
        name: text(attributes, "name"),
        duration_ms: attributes["durationInMillis"].as_u64().unwrap_or_default() as u32,
        explicit: attributes["contentRating"].as_str() == Some("explicit"),
        artists: artist_refs(resource, &artist),
        album: Some(Album {
            uri: format!("spotify:album:{album_id}"),
            id: album_id,
            name: text(attributes, "albumName"),
            images: images(attributes),
            artists: vec![ArtistRef {
                name: artist,
                ..Default::default()
            }],
            release_date: attributes["releaseDate"].as_str().map(str::to_owned),
            ..Default::default()
        }),
        track_number: attributes["trackNumber"].as_u64().map(|number| number as u32),
        disc_number: attributes["discNumber"].as_u64().map(|number| number as u32),
        is_playable: Some(true),
        ..Default::default()
    }
}

fn album(resource: &Value) -> Album {
    let id = text(resource, "id");
    let attributes = &resource["attributes"];
    let artist = text(attributes, "artistName");
    let tracks = songs_in(&resource["relationships"]["tracks"]["data"]);
    Album {
        uri: format!("spotify:album:{id}"),
        id,
        name: text(attributes, "name"),
        album_type: Some(
            if attributes["isSingle"].as_bool() == Some(true) {
                "single"
            } else if attributes["isCompilation"].as_bool() == Some(true) {
                "compilation"
            } else {
                "album"
            }
            .to_owned(),
        ),
        total_tracks: attributes["trackCount"].as_u64().map(|count| count as u32),
        images: images(attributes),
        artists: artist_refs(resource, &artist),
        release_date: attributes["releaseDate"].as_str().map(str::to_owned),
        label: attributes["recordLabel"].as_str().map(str::to_owned),
        genres: attributes["genreNames"]
            .as_array()
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        tracks: (!tracks.is_empty()).then(|| page(tracks, 0)),
        ..Default::default()
    }
}

fn artist(resource: &Value) -> Artist {
    let id = text(resource, "id");
    let attributes = &resource["attributes"];
    Artist {
        uri: format!("spotify:artist:{id}"),
        id,
        name: text(attributes, "name"),
        images: images(attributes),
        genres: attributes["genreNames"]
            .as_array()
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        ..Default::default()
    }
}

fn songs_in(data: &Value) -> Vec<Track> {
    data.as_array()
        .map(|items| items.iter().map(track).collect())
        .unwrap_or_default()
}

fn page<T>(items: Vec<T>, offset: u32) -> Page<T> {
    Page {
        total: offset + items.len() as u32,
        limit: items.len() as u32,
        offset,
        next: None,
        items,
    }
}

/// A page of a list, with the total Apple reports and whether more follows.
fn listed<T>(found: &Value, offset: u32, map: impl Fn(&Value) -> T) -> Page<T> {
    let items: Vec<T> = found["data"]
        .as_array()
        .map(|items| items.iter().map(&map).collect())
        .unwrap_or_default();
    let count = items.len() as u32;
    Page {
        total: found["meta"]["total"].as_u64().map_or(offset + count, |total| total as u32),
        limit: count,
        offset,
        next: found["next"].as_str().map(str::to_owned),
        items,
    }
}

fn playlist(resource: &Value) -> Playlist {
    let id = text(resource, "id");
    let attributes = &resource["attributes"];
    Playlist {
        uri: format!("spotify:playlist:{id}"),
        id,
        name: text(attributes, "name"),
        description: attributes["description"]["standard"].as_str().map(str::to_owned),
        images: images(attributes),
        owner: Owner {
            id: Some(DEVICE_ID.to_owned()),
            display_name: Some("You".to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn is_missing(error: &ApiError) -> bool {
    matches!(error, ApiError::Status { status: 404, .. })
}

/// Apple answers 404 for an empty list; the interface expects an empty page.
fn or_empty(found: ApiResult<Value>) -> ApiResult<Value> {
    match found {
        Err(error) if is_missing(&error) => Ok(json!({"data": []})),
        other => other,
    }
}

async fn playlist_tracks(id: &str, offset: u32) -> ApiResult<Page<Track>> {
    let found = or_empty(
        get(&format!(
            "/v1/me/library/playlists/{id}/tracks?limit=100&offset={offset}"
        ))
        .await,
    )?;
    Ok(listed(&found, offset, track))
}

/// A library artist has its own `r.` ID, and lives under /me.
fn is_library_id(id: &str) -> bool {
    id.starts_with("r.")
}

fn artist_path(id: &str, rest: &str) -> String {
    if is_library_id(id) {
        format!("/v1/me/library/artists/{id}{rest}")
    } else {
        format!("/v1/catalog/{{storefront}}/artists/{id}{rest}")
    }
}

/// A library album has its own `l.` ID, and lives under /me.
fn album_path(id: &str) -> String {
    if id.starts_with("l.") {
        format!("/v1/me/library/albums/{id}?include=tracks,artists")
    } else {
        format!("/v1/catalog/{{storefront}}/albums/{id}?include=tracks,artists")
    }
}

async fn search(query: &str) -> ApiResult<SearchResults> {
    let found = get(&format!(
        "/v1/catalog/{{storefront}}/search?types=songs,albums,artists&limit=25&include=artists&term={}",
        encode(query)
    ))
    .await?;
    let results = &found["results"];
    let list = |key: &str| results[key]["data"].as_array().cloned().unwrap_or_default();
    Ok(SearchResults {
        tracks: Some(page(list("songs").iter().map(track).collect(), 0)),
        albums: Some(page(list("albums").iter().map(album).collect(), 0)),
        artists: Some(page(list("artists").iter().map(artist).collect(), 0)),
        ..Default::default()
    })
}

async fn album_by_id(id: &str) -> ApiResult<Album> {
    let found = get(&album_path(id)).await?;
    found["data"]
        .get(0)
        .map(album)
        .ok_or_else(|| ApiError::Decode("no such album".into()))
}

async fn album_tracks(id: &str) -> ApiResult<Page<Track>> {
    Ok(album_by_id(id).await?.tracks.unwrap_or_default())
}

async fn artist_top_songs(id: &str) -> ApiResult<Vec<Track>> {
    if is_library_id(id) {
        return Ok(Vec::new());
    }
    let found = get(&format!(
        "/v1/catalog/{{storefront}}/artists/{id}/view/top-songs?limit=20"
    ))
    .await?;
    Ok(songs_in(&found["data"]))
}

/// Who the interface signs in as. MusicKit doesn't name the listener, and
/// "premium" says playback is allowed, which authorization just established.
fn account() -> User {
    User {
        id: DEVICE_ID.to_owned(),
        display_name: Some("Apple Music".to_owned()),
        product: Some("premium".to_owned()),
        ..Default::default()
    }
}

/// The answer to a request Apple Music can serve, or `None` to let it fall through.
pub async fn dispatch(request: &ApiRequest) -> Option<ApiResponse> {
    Some(match request {
        ApiRequest::Me => ApiResponse::Me(Ok(account())),
        ApiRequest::Devices => ApiResponse::Devices(speakers().await),
        ApiRequest::Transfer { device_id, .. } => ApiResponse::Transferred {
            device_id: device_id.clone(),
            result: route_to(device_id).await,
        },
        // Nothing plays elsewhere: the player here is the only one.
        ApiRequest::PlaybackState { seq } => ApiResponse::PlaybackState {
            seq: *seq,
            result: Ok(None),
        },
        ApiRequest::Queue { seq } => ApiResponse::Queue {
            seq: *seq,
            result: player_queue().await.inspect(|queue| {
                log::info!("apple: queue has {} songs after the current one", queue.queue.len());
            }),
        },
        ApiRequest::Search { query, serial } | ApiRequest::SearchCatalogue { query, serial } => {
            ApiResponse::Search {
                query: query.clone(),
                serial: *serial,
                result: search(query).await,
            }
        }
        ApiRequest::PlaySearch {
            kind,
            query,
            serial,
        } => ApiResponse::PlaySearch {
            query: query.clone(),
            serial: *serial,
            result: search(query).await.map(|found| match kind.as_str() {
                "album" => found.albums.and_then(|page| page.items.into_iter().next()).map(|album| album.uri),
                _ => found.tracks.and_then(|page| page.items.into_iter().next()).map(|track| track.uri),
            }),
        },
        ApiRequest::MyPlaylists { offset, generation } => ApiResponse::MyPlaylists {
            offset: *offset,
            generation: *generation,
            result: or_empty(
                get(&format!("/v1/me/library/playlists?limit=100&offset={offset}")).await,
            )
            .map(|found| {
                let page = listed(&found, *offset, playlist);
                log::info!("apple: {} playlists at {offset}, {} in all", page.items.len(), page.total);
                page
            }),
        },
        ApiRequest::Playlist { id, generation } => ApiResponse::Playlist {
            id: id.clone(),
            generation: *generation,
            result: get(&format!("/v1/me/library/playlists/{id}")).await.and_then(|found| {
                found["data"]
                    .get(0)
                    .map(playlist)
                    .ok_or_else(|| ApiError::Decode("no such playlist".into()))
            }),
        },
        ApiRequest::PlaylistItems {
            id,
            offset,
            generation,
        } => ApiResponse::PlaylistItems {
            id: id.clone(),
            offset: *offset,
            generation: *generation,
            result: playlist_tracks(id, *offset).await.map(|tracks| Page {
                total: tracks.total,
                limit: tracks.limit,
                offset: tracks.offset,
                next: tracks.next,
                items: tracks
                    .items
                    .into_iter()
                    .map(|track| PlaylistItem {
                        item: Some(PlayableItem::Track(track)),
                        ..Default::default()
                    })
                    .collect(),
            }),
        },
        ApiRequest::SavedTracks { offset, generation } => ApiResponse::SavedTracks {
            offset: *offset,
            generation: *generation,
            account_id: Some(DEVICE_ID.to_owned()),
            result: or_empty(get(&format!("/v1/me/library/songs?limit=100&offset={offset}")).await)
                .map(|found| {
                    let page = listed(&found, *offset, |resource| SavedTrack {
                        track: track(resource),
                        ..Default::default()
                    });
                    log::info!("apple: {} library songs at {offset}, {} in all", page.items.len(), page.total);
                    page
                }),
        },
        ApiRequest::SavedAlbums { offset } => ApiResponse::SavedAlbums {
            offset: *offset,
            result: or_empty(get(&format!("/v1/me/library/albums?limit=100&offset={offset}")).await)
                .map(|found| {
                    listed(&found, *offset, |resource| SavedAlbum {
                        album: album(resource),
                        ..Default::default()
                    })
                }),
        },
        ApiRequest::FollowedArtists { after } => {
            let offset: u32 = after.as_deref().and_then(|after| after.parse().ok()).unwrap_or(0);
            ApiResponse::FollowedArtists {
                after: after.clone(),
                result: or_empty(
                    get(&format!("/v1/me/library/artists?limit=100&offset={offset}")).await,
                )
                .map(|found| {
                    let page = listed(&found, offset, artist);
                    let next = page.next.is_some().then(|| (offset + page.items.len() as u32).to_string());
                    CursorPage {
                        total: Some(page.total),
                        next: next.clone(),
                        cursors: Some(Cursors { after: next, before: None }),
                        items: page.items,
                    }
                }),
            }
        }
        ApiRequest::RecentlyPlayed {
            who,
            generation,
            limit,
            ..
        } => ApiResponse::RecentlyPlayed {
            who: *who,
            generation: *generation,
            limit: *limit,
            result: or_empty(get("/v1/me/recent/played/tracks?limit=30").await).map(|found| CursorPage {
                items: songs_in(&found["data"])
                    .into_iter()
                    .map(|track| PlayHistory {
                        track,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
        },
        ApiRequest::SetSaved { uris, saved } => ApiResponse::SavedChanged {
            uris: uris.clone(),
            saved: *saved,
            result: set_loved(uris, *saved).await,
        },
        ApiRequest::Contains { uris } => ApiResponse::Contains {
            uris: uris.clone(),
            result: are_loved(uris).await,
        },
        ApiRequest::AddToQueue { uri, label, .. } => ApiResponse::QueueAdded {
            label: label.clone(),
            result: enqueue(uri).await,
        },
        ApiRequest::AddManyToQueue { request, uris, .. } => {
            let mut added = 0;
            let mut result = Ok(());
            for uri in uris {
                result = enqueue(uri).await;
                if result.is_err() {
                    break;
                }
                added += 1;
            }
            ApiResponse::QueueBatchAdded {
                request: *request,
                added,
                result,
            }
        }
        ApiRequest::Album { id } => ApiResponse::Album {
            id: id.clone(),
            result: album_by_id(id).await,
        },
        ApiRequest::AlbumTracks {
            id,
            offset,
            generation,
        } => ApiResponse::AlbumTracks {
            id: id.clone(),
            offset: *offset,
            generation: *generation,
            result: album_tracks(id).await,
        },
        ApiRequest::Artist { id } => {
            let found = get(&artist_path(id, "")).await;
            ApiResponse::Artist {
                id: id.clone(),
                result: found.and_then(|found| {
                    found["data"]
                        .get(0)
                        .map(artist)
                        .ok_or_else(|| ApiError::Decode("no such artist".into()))
                }),
            }
        }
        ApiRequest::ArtistTopTracks { id, generation } => ApiResponse::ArtistTopTracks {
            id: id.clone(),
            generation: *generation,
            result: artist_top_songs(id).await,
        },
        ApiRequest::ArtistAlbums { id, groups, offset } => {
            let found = get(&artist_path(id, &format!("/albums?limit=25&offset={offset}"))).await;
            ApiResponse::ArtistAlbums {
                id: id.clone(),
                groups: groups.clone(),
                offset: *offset,
                result: found.map(|found| {
                    let items: Vec<Album> = found["data"]
                        .as_array()
                        .map(|albums| albums.iter().map(album).collect())
                        .unwrap_or_default();
                    page(items, *offset)
                }),
            }
        }
        ApiRequest::RelatedArtists { id } => {
            // Only the catalog knows who's similar, and a library artist isn't in it.
            let found = if is_library_id(id) {
                Ok(json!({"data": []}))
            } else {
                get(&artist_path(id, "/view/similar-artists?limit=20")).await
            };
            ApiResponse::RelatedArtists {
                id: id.clone(),
                result: found.map(|found| {
                    found["data"]
                        .as_array()
                        .map(|artists| artists.iter().map(artist).collect())
                        .unwrap_or_default()
                }),
            }
        }
        _ => return None,
    })
}

// ---- Playback ----

fn song_id(uri: &str) -> Option<&str> {
    uri.strip_prefix("spotify:track:")
}

fn start_index(spec: &LoadSpec, ids: &[String]) -> usize {
    if let Some(wanted) = spec.offset_uri.as_deref().and_then(song_id)
        && let Some(index) = ids.iter().position(|id| id == wanted)
    {
        return index;
    }
    spec.offset_index.unwrap_or(0) as usize
}

/// The playlist the queue came from, and the order it was in.
struct Following {
    playlist: String,
    ids: Vec<String>,
}

static FOLLOWING: std::sync::Mutex<Option<Following>> = std::sync::Mutex::new(None);

fn following_lock() -> std::sync::MutexGuard<'static, Option<Following>> {
    FOLLOWING.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A playlist's songs in order, by catalog ID where they have one, with the
/// library ID of each for songs the catalog won't return, as uploads may be.
async fn playlist_songs(
    playlist: &str,
) -> ApiResult<(Vec<String>, serde_json::Map<String, Value>)> {
    let mut ids = Vec::new();
    let mut library_ids = serde_json::Map::new();
    let mut offset = 0;
    loop {
        let found = or_empty(
            get(&format!(
                "/v1/me/library/playlists/{playlist}/tracks?limit=100&offset={offset}"
            ))
            .await,
        )?;
        let songs = found["data"].as_array().cloned().unwrap_or_default();
        let more = found["next"].is_string();
        offset += songs.len() as u32;
        for song in &songs {
            let library = text(song, "id");
            match song["attributes"]["playParams"]["catalogId"].as_str() {
                Some(catalog) => {
                    library_ids.insert(catalog.to_owned(), json!(library));
                    ids.push(catalog.to_owned());
                }
                None => ids.push(library),
            }
        }
        // Apple's own limit on a playlist is far above what anyone queues.
        if !more || offset >= 5000 {
            return Ok((ids, library_ids));
        }
    }
}

/// Keeps the queue in step with changes made to its playlist elsewhere, as in
/// Music.app, once Apple has synced them. The song playing isn't interrupted.
async fn follow_playlist(client: Client) {
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let Some(playlist) = following_lock().as_ref().map(|following| following.playlist.clone())
        else {
            continue;
        };
        let Ok((ids, library_ids)) = playlist_songs(&playlist).await else {
            continue;
        };
        // Skipped unless this is still the playlist and its order really changed.
        let changed = following_lock().as_ref().is_some_and(|following| {
            following.playlist == playlist && following.ids != ids
        });
        if !changed || ids.is_empty() {
            continue;
        }
        let request = c_string(&json!({"ids": ids, "libraryIDs": library_ids}).to_string());
        let (status, body) =
            call(|context, reply| unsafe { am_requeue(request.as_ptr(), context, reply) }).await;
        let reply = String::from_utf8_lossy(&body);
        if status == 200 && reply.contains("\"applied\":true") {
            if let Some(following) = following_lock().as_mut()
                && following.playlist == playlist
            {
                following.ids = ids;
            }
            log::info!("apple: the playlist changed, queue updated ({reply})");
            emit(&client.events, &client.waker, Event::RemoteChanged);
        }
    }
}

async fn load(spec: LoadSpec, warm: bool) -> anyhow::Result<()> {
    // A warm-up isn't a choice, and yields to any that is made.
    let turn = if warm { 0 } else { LOAD_TURN.fetch_add(1, Ordering::SeqCst) + 1 };
    let began = Instant::now();
    let mut library_ids = serde_json::Map::new();
    let mut next_following = None;
    let mut ids: Vec<String> = spec
        .uris
        .iter()
        .filter_map(|uri| song_id(uri).map(str::to_owned))
        .collect();
    if ids.is_empty()
        && let Some(song) = spec.context_uri.as_deref().and_then(song_id)
    {
        ids.push(song.to_owned());
    }
    if ids.is_empty()
        && let Some(album) = spec
            .context_uri
            .as_deref()
            .and_then(|context| context.strip_prefix("spotify:album:"))
    {
        ids = album_tracks(album)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .items
            .into_iter()
            .filter_map(|track| track.id)
            .collect();
    }
    if ids.is_empty()
        && let Some(playlist) = spec
            .context_uri
            .as_deref()
            .and_then(|context| context.strip_prefix("spotify:playlist:"))
    {
        // Every song, so the queue is the whole playlist from the chosen one on.
        (ids, library_ids) = playlist_songs(playlist)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        next_following = Some(Following {
            playlist: playlist.to_owned(),
            ids: ids.clone(),
        });
    }
    if ids.is_empty() {
        anyhow::bail!(
            "Playing {} isn't supported yet",
            spec.context_uri.as_deref().unwrap_or("this")
        );
    }
    // Fetching a long playlist takes a while: something newer may have been chosen.
    if turn > 0 && LOAD_TURN.load(Ordering::SeqCst) != turn {
        return Ok(());
    }
    let request = c_string(
        &json!({
            "generation": turn,
            "start": start_index(&spec, &ids),
            "positionMs": spec.position_ms,
            "play": spec.play,
            "warm": warm,
            "libraryIDs": library_ids,
            "ids": ids,
        })
        .to_string(),
    );
    let (status, body) = call(|context, reply| unsafe { am_load(request.as_ptr(), context, reply) }).await;
    let body = String::from_utf8_lossy(&body);
    if status != 200 {
        anyhow::bail!("{body}");
    }
    // A warm-up that found a song already loaded changed nothing.
    if !body.contains("skipped") {
        *following_lock() = next_following;
    }
    log::info!(
        "apple: load answered in {} ms ({body})",
        began.elapsed().as_millis()
    );
    Ok(())
}

/// The library playlist with this name, as a URI.
async fn playlist_named(name: &str) -> Option<String> {
    let mut offset = 0;
    loop {
        let found = get(&format!("/v1/me/library/playlists?limit=100&offset={offset}"))
            .await
            .ok()?;
        let page = listed(&found, offset, playlist);
        if let Some(playlist) = page.items.iter().find(|playlist| playlist.name == name) {
            return Some(playlist.uri.clone());
        }
        page.next.as_ref()?;
        offset += page.items.len() as u32;
    }
}

async fn warm_up(client: Client, startup_playlist: Option<String>) {
    use std::sync::atomic::Ordering;
    let began = Instant::now();
    let wanted = match &startup_playlist {
        Some(name) => playlist_named(name).await,
        None => None,
    };
    let spec = if let Some(playlist) = wanted {
        LoadSpec {
            context_uri: Some(playlist),
            play: false,
            ..Default::default()
        }
    } else {
        let Ok(recent) = get("/v1/me/recent/played/tracks?limit=1").await else {
            return;
        };
        let Some(track) = recent["data"].get(0).map(track) else {
            return;
        };
        LoadSpec {
            uris: vec![track.uri],
            play: false,
            ..Default::default()
        }
    };
    let chosen = &client.chosen;
    // Whatever was chosen while this waited on the network comes first.
    if chosen.load(Ordering::SeqCst) {
        return;
    }
    match load(spec, true).await {
        Ok(()) => {
            log::info!("apple: warmed up in {} ms", began.elapsed().as_millis());
            // Let the panel fetch the rows that just appeared.
            emit(&client.events, &client.waker, Event::RemoteChanged);
        }
        Err(error) => log::warn!("apple: warm-up failed: {error}"),
    }
}

static STARTED: AtomicBool = AtomicBool::new(false);
static LOAD_TURN: AtomicU64 = AtomicU64::new(0);

/// The account the app's per-account caches are kept under, once signed in.
pub fn cache_account() -> Option<crate::api::AccountId> {
    STARTED
        .load(Ordering::Acquire)
        .then(|| crate::api::AccountId::new(DEVICE_ID))
}

/// Quitting stops the song: the player's process would keep it playing. On the
/// main thread, before it blocks waiting for the backend to finish.
pub fn stop_player() {
    if STARTED.load(Ordering::Acquire) {
        // SAFETY: takes no arguments.
        unsafe { am_stop() };
    }
}

/// The running side of Apple Music: the player's commands and its reported state.
#[derive(Clone)]
pub struct Client {
    events: std::sync::mpsc::Sender<Event>,
    waker: Waker,
    /// When the last song was asked for, and whether the player has taken it,
    /// until its first sound is reported. The old song's clock keeps running
    /// until then, so it can't count as the new song's sound.
    asked_at: Arc<std::sync::Mutex<Option<(Instant, bool)>>>,
    /// Something was chosen to play, so the warm-up must leave the player alone.
    chosen: Arc<std::sync::atomic::AtomicBool>,
    /// Skips asked for together, as a click on a row far down the queue does.
    skips: Arc<std::sync::atomic::AtomicI32>,
}

fn emit(events: &std::sync::mpsc::Sender<Event>, waker: &Waker, event: Event) {
    let _ = events.send(event);
    waker.wake();
}

impl Client {
    /// Signs in through MusicKit and starts reporting the player and its audio.
    pub fn start(
        events: std::sync::mpsc::Sender<Event>,
        waker: Waker,
        tap: Arc<crate::vis::AudioTap>,
        eq: crate::eq::SharedEq,
        initial_volume: u16,
        startup_playlist: Option<String>,
    ) -> Self {
        MAC_VOLUME.store(crate::app::volume_to_percent(initial_volume), Ordering::Relaxed);
        let _ = EVENTS.set((events.clone(), waker.clone()));
        STARTED.store(true, Ordering::Release);
        let client = Self {
            events,
            waker,
            asked_at: Arc::default(),
            chosen: Arc::default(),
            skips: Arc::default(),
        };
        let starting = client.clone();
        tokio::spawn(async move {
            let (status, body) = call(|context, reply| unsafe { am_authorize(context, reply) }).await;
            if status != 200 {
                let body = String::from_utf8_lossy(&body).into_owned();
                log::warn!("apple: authorization refused: {body}");
                emit(
                    &starting.events,
                    &starting.waker,
                    Event::Auth(AuthStatus::Failed(format!(
                        "Apple Music access wasn't granted ({body})."
                    ))),
                );
                return;
            }
            emit(
                &starting.events,
                &starting.waker,
                Event::Auth(AuthStatus::Connected {
                    username: "Apple Music".to_owned(),
                }),
            );
            // The main window waits for an account, which a Spotify sign-in sends itself.
            emit(
                &starting.events,
                &starting.waker,
                Event::Api(Box::new(ApiResponse::Me(Ok(account())))),
            );
            emit(
                &starting.events,
                &starting.waker,
                Event::Playback(LocalPlayback::Ready {
                    device_id: DEVICE_ID.to_owned(),
                }),
            );
            start_route(eq, tap);
            // SAFETY: takes no arguments and keeps its own state.
            unsafe { am_speakers_start() };
            // The last song, paused: somewhere to resume, and the player's
            // process is already up when the first song is chosen.
            tokio::spawn(warm_up(starting.clone(), startup_playlist));
            tokio::spawn(follow_playlist(starting.clone()));
            starting.report_state().await;
        });
        client
    }

    /// Quitting says goodbye to the speaker, or it stays held for minutes.
    pub async fn stop(&self) {
        let leaving = tokio::task::spawn_blocking(|| leave_speaker(None));
        let _ = tokio::time::timeout(Duration::from_secs(2), leaving).await;
    }

    pub fn player(&self, command: PlayerCommand) {
        // SAFETY: am_control only reads its two plain arguments.
        let control = |operation: i32, value: f64| unsafe { am_control(operation, value) };
        match command {
            PlayerCommand::Toggle => control(1, 0.0),
            PlayerCommand::Next => {
                use std::sync::atomic::Ordering;
                if self.skips.fetch_add(1, Ordering::SeqCst) == 0 {
                    let skips = Arc::clone(&self.skips);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        let count = skips.swap(0, Ordering::SeqCst);
                        // SAFETY: am_skip only reads its plain argument.
                        unsafe { am_skip(count) };
                    });
                }
            }
            PlayerCommand::Previous => control(3, 0.0),
            PlayerCommand::Seek(position) => control(4, f64::from(position)),
            PlayerCommand::Shuffle(on) => control(5, if on { 1.0 } else { 0.0 }),
            PlayerCommand::Repeat(mode) => control(
                6,
                match mode {
                    RepeatMode::Off => 0.0,
                    RepeatMode::Context => 1.0,
                    RepeatMode::Track => 2.0,
                },
            ),
            PlayerCommand::Load(spec) => {
                *self.asked_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some((Instant::now(), false));
                self.chosen.store(true, std::sync::atomic::Ordering::SeqCst);
                let client = self.clone();
                tokio::spawn(async move {
                    let result = load(spec, false).await;
                    {
                        let mut asked = client
                            .asked_at
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        match (&result, asked.as_mut()) {
                            (Ok(()), Some(asked)) => asked.1 = true,
                            (Err(_), _) => *asked = None,
                            _ => {}
                        }
                    }
                    if let Err(error) = result {
                        emit(
                            &client.events,
                            &client.waker,
                            Event::Error(format!("Couldn't play that: {error}")),
                        );
                    }
                });
            }
            // MusicKit has no volume of its own: the route or the speaker applies it.
            PlayerCommand::Volume(volume) | PlayerCommand::VolumePreview(volume) => {
                let percent = crate::app::volume_to_percent(volume);
                if SPEAKER_ON.load(Ordering::Acquire)
                    && let Some(speaker) = speaker_lock().as_ref()
                {
                    SPEAKER_VOLUME.store(percent, Ordering::Relaxed);
                    // SAFETY: the lock keeps the session alive for this call.
                    unsafe { am_airplay_volume(speaker.session, f64::from(percent)) };
                } else {
                    MAC_VOLUME.store(percent, Ordering::Relaxed);
                }
            }
            PlayerCommand::ClearQueue | PlayerCommand::Transfer => {}
            PlayerCommand::AddToQueue(uri) => {
                let client = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = enqueue(&uri).await {
                        emit(
                            &client.events,
                            &client.waker,
                            Event::Error(format!("Couldn't queue that: {error}")),
                        );
                    }
                });
            }
        }
    }

    async fn report_state(self) {
        let mut track_sequence = 0;
        let mut seek_sequence = 0;
        let mut last_id = String::new();
        let mut last_position: Option<(u32, Instant)> = None;
        let mut last_sent: Option<(Playback, String, bool, RepeatMode, u8)> = None;
        let mut sent_at = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let (status, body) = call(|context, reply| unsafe { am_state(context, reply) }).await;
            if status != 200 {
                continue;
            }
            let Ok(state) = serde_json::from_slice::<Value>(&body) else {
                continue;
            };
            let playback = match state["status"].as_str().unwrap_or_default() {
                "playing" => Playback::Playing,
                "paused" | "interrupted" => Playback::Paused,
                "seekingForward" | "seekingBackward" => Playback::Loading,
                // A song waiting in the player is paused, ready to start.
                _ if state["id"].is_string() => Playback::Paused,
                _ => Playback::Stopped,
            };
            let position_ms = state["positionMs"].as_u64().unwrap_or_default() as u32;
            let id = text(&state, "id");
            if id != last_id {
                last_id.clone_from(&id);
                track_sequence += 1;
                // The rows around the song on have moved: fetch them.
                emit(&self.events, &self.waker, Event::RemoteChanged);
            }
            if playback == Playback::Playing && position_ms > 0 {
                let mut asked = self
                    .asked_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some((at, true)) = *asked {
                    log::info!(
                        "apple: first audio {} ms after the song was chosen",
                        at.elapsed().as_millis()
                    );
                    *asked = None;
                }
            }
            // A jump the clock can't explain is a seek, for lyrics and media panels.
            if let Some((previous, at)) = last_position {
                let expected = previous as i64 + at.elapsed().as_millis() as i64;
                if playback == Playback::Playing && (position_ms as i64 - expected).abs() > 1500 {
                    seek_sequence += 1;
                }
            }
            last_position = Some((position_ms, Instant::now()));
            let shuffle = state["shuffle"].as_bool().unwrap_or_default();
            let repeat = match state["repeat"].as_i64().unwrap_or_default() {
                2 => RepeatMode::Track,
                1 => RepeatMode::Context,
                _ => RepeatMode::Off,
            };
            let level = if SPEAKER_ON.load(Ordering::Acquire) {
                SPEAKER_VOLUME.load(Ordering::Relaxed)
            } else {
                MAC_VOLUME.load(Ordering::Relaxed)
            };
            let summary = (playback, id.clone(), shuffle, repeat, level);
            // Between changes the interface runs its own clock; correct it now and then.
            if last_sent.as_ref() == Some(&summary) && sent_at.elapsed() < Duration::from_secs(2) {
                continue;
            }
            last_sent = Some(summary);
            sent_at = Instant::now();
            let art = text(&state, "art");
            let track = (!id.is_empty()).then(|| LocalTrack {
                uri: format!("spotify:track:{id}"),
                title: text(&state, "title"),
                artists: vec![ArtistRef {
                    name: text(&state, "artist"),
                    ..Default::default()
                }],
                album: text(&state, "album"),
                art_url: (!art.is_empty()).then(|| art.clone()),
                art_small_url: (!art.is_empty()).then_some(art),
                duration_ms: state["durationMs"].as_u64().unwrap_or_default() as u32,
                is_episode: false,
            });
            let local = LocalState {
                playback,
                track,
                position_ms,
                position_at: (playback == Playback::Playing).then(Instant::now),
                shuffle,
                repeat,
                connected: true,
                username: "Apple Music".to_owned(),
                seek_sequence,
                track_sequence,
                volume: crate::app::percent_to_volume(level),
                ..Default::default()
            };
            emit(&self.events, &self.waker, Event::Local(Box::new(local)));
        }
    }
}

// ---- The sound ----

// Apple's player has no volume or equalizer of its own. So its process is
// tapped and muted, as Spotifast does for Soloist, and its sound goes on from
// here: equalizer, visualizers, then this Mac at the app's volume, or a speaker.
// Nothing is stored: samples only pass through in memory.

unsafe extern "C" {
    fn am_player_pid() -> i32;
    fn spotifast_soloist_audio(
        pid: u32,
        context: *mut c_void,
        control: extern "C" fn(*mut c_void) -> u32,
        process: extern "C" fn(*mut c_void, *mut f32, usize, u32),
        event: extern "C" fn(*mut c_void, u32),
    ) -> i32;
}

/// The rate the visualizers and the speaker read.
const SHARED_RATE: u32 = 44_100;

static SPEAKER_ON: AtomicBool = AtomicBool::new(false);
/// The Mac's volume, 0 to 100.
static MAC_VOLUME: AtomicU8 = AtomicU8::new(100);
/// The speaker's volume as last set here, 0 to 100.
static SPEAKER_VOLUME: AtomicU8 = AtomicU8::new(100);
static ROUTE_STOP: AtomicBool = AtomicBool::new(false);

struct Route {
    shared: crate::eq::SharedEq,
    eq: crate::eq::Processor,
    limiter: crate::limiter::Limiter,
    resampler: Option<crate::resample::Resampler>,
    rate: u32,
    tap: Arc<crate::vis::AudioTap>,
}

impl Route {
    fn new(shared: crate::eq::SharedEq, tap: Arc<crate::vis::AudioTap>) -> Self {
        let mut route = Self {
            eq: crate::eq::Processor::new(shared.clone()),
            shared,
            limiter: crate::limiter::Limiter::new(f64::from(SHARED_RATE)),
            resampler: None,
            rate: SHARED_RATE,
            tap,
        };
        route.reset(SHARED_RATE);
        route
    }

    fn reset(&mut self, rate: u32) {
        self.rate = rate;
        self.eq = crate::eq::Processor::at_rate(self.shared.clone(), rate);
        self.limiter = crate::limiter::Limiter::new(f64::from(rate));
        self.resampler = crate::resample::Resampler::new(rate, SHARED_RATE, 2);
        self.tap.clear();
    }

    fn process(&mut self, samples: &mut [f32], rate: u32) {
        if !(8_000..=192_000).contains(&rate) || !samples.len().is_multiple_of(2) {
            samples.fill(0.0);
            return;
        }
        if self.rate != rate {
            self.reset(rate);
        }
        let mut wide: Vec<f64> = samples
            .iter()
            .map(|sample| if sample.is_finite() { f64::from(*sample) } else { 0.0 })
            .collect();
        self.eq.process(&mut wide);
        self.limiter.process(&mut wide, 1.0);
        let narrow: Vec<f32> = wide.iter().map(|sample| *sample as f32).collect();
        let shared = match &mut self.resampler {
            Some(resampler) => resampler.process(&narrow),
            None => narrow.clone(),
        };
        let on_speaker = SPEAKER_ON.load(Ordering::Acquire);
        if on_speaker
            && let Ok(speaker) = SPEAKER.try_lock()
            && let Some(speaker) = speaker.as_ref()
        {
            // SAFETY: the lock keeps the session alive; the slice outlives the call.
            unsafe { am_airplay_push(speaker.session, shared.as_ptr(), shared.len()) };
        }
        let for_visuals: Vec<f64> = shared.iter().copied().map(f64::from).collect();
        self.tap.push(&for_visuals, 1.0);
        if on_speaker {
            samples.fill(0.0);
        } else {
            let gain = (f32::from(MAC_VOLUME.load(Ordering::Relaxed).min(100)) / 100.0).powi(3);
            for (sample, processed) in samples.iter_mut().zip(narrow) {
                *sample = processed * gain;
            }
        }
    }
}

extern "C" fn route_control(_context: *mut c_void) -> u32 {
    // Bit 0 stops the route; bit 1 says sound is wanted.
    u32::from(ROUTE_STOP.load(Ordering::Acquire)) | 2
}

extern "C" fn route_process(context: *mut c_void, data: *mut f32, count: usize, rate: u32) {
    // SAFETY: the native worker owns this exclusive buffer and calls serially,
    // and `run_route` keeps the route alive until the native call returns.
    let (route, samples) = unsafe {
        (
            &mut *context.cast::<Route>(),
            std::slice::from_raw_parts_mut(data, count),
        )
    };
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        route.process(samples, rate);
    }))
    .is_err();
    if failed {
        samples.fill(0.0);
        ROUTE_STOP.store(true, Ordering::Release);
    }
}

extern "C" fn route_event(context: *mut c_void, event: u32) {
    match event {
        1 => log::info!("apple: the player's sound now goes through the app"),
        2 => {
            // SAFETY: events and processing run serially on the native worker.
            let route = unsafe { &mut *context.cast::<Route>() };
            let rate = route.rate;
            route.reset(rate);
        }
        // Whatever failed, starting over finds the process and device afresh.
        3 => ROUTE_STOP.store(true, Ordering::Release),
        _ => {}
    }
}

/// Keeps the route on the player's process for the life of the app. The process
/// comes and goes, and the route restarts when the output device changes.
fn start_route(shared: crate::eq::SharedEq, tap: Arc<crate::vis::AudioTap>) {
    std::thread::Builder::new()
        .name("applebum-route".to_owned())
        .spawn(move || {
            loop {
                // SAFETY: only reads Core Audio's process list.
                let pid = unsafe { am_player_pid() };
                if pid <= 0 {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                ROUTE_STOP.store(false, Ordering::Release);
                let watcher = std::thread::spawn(move || {
                    while !ROUTE_STOP.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(500));
                        // SAFETY: as above.
                        if unsafe { am_player_pid() } != pid {
                            ROUTE_STOP.store(true, Ordering::Release);
                        }
                    }
                });
                let mut route = Route::new(shared.clone(), Arc::clone(&tap));
                // SAFETY: `route` outlives this call, which joins its callbacks.
                let status = unsafe {
                    spotifast_soloist_audio(
                        pid as u32,
                        std::ptr::from_mut(&mut route).cast(),
                        route_control,
                        route_process,
                        route_event,
                    )
                };
                ROUTE_STOP.store(true, Ordering::Release);
                let _ = watcher.join();
                log::info!("apple: the sound route ended ({status}), restarting");
                std::thread::sleep(Duration::from_millis(300));
            }
        })
        .expect("the route thread starts");
}
