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
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::api::ApiError;
use crate::api::models::{
    Album, Artist, ArtistRef, CursorPage, Cursors, Image, Owner, Page, PlayHistory, PlayableItem,
    Playlist, PlaylistItem, Queue, SavedAlbum, SavedTrack, SearchResults, Track, TrackCount, User,
};
use crate::backend::{ApiRequest, ApiResponse, ApiResult, AuthStatus, Event, LocalPlayback, Waker};
use crate::player::{LoadSpec, LocalState, LocalTrack, Playback, PlayerCommand, RepeatMode};

pub const DEVICE_ID: &str = "apple-music";

type Reply = extern "C" fn(*mut c_void, i32, *const u8, usize);
type Audio = extern "C" fn(*mut c_void, *const f32, usize, u32);

unsafe extern "C" {
    fn am_authorize(context: *mut c_void, reply: Reply);
    fn am_request(path: *const c_char, context: *mut c_void, reply: Reply);
    fn am_load(json: *const c_char, context: *mut c_void, reply: Reply);
    fn am_control(operation: i32, value: f64);
    fn am_state(context: *mut c_void, reply: Reply);
    fn am_tap_start(context: *mut c_void, callback: Audio);
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

/// A library album or artist has its own `l.`/`r.` ID, and lives under /me.
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
    let found = get(&format!(
        "/v1/catalog/{{storefront}}/artists/{id}/view/top-songs?limit=20"
    ))
    .await?;
    Ok(songs_in(&found["data"]))
}

/// The answer to a request Apple Music can serve, or `None` to let it fall through.
pub async fn dispatch(request: &ApiRequest) -> Option<ApiResponse> {
    Some(match request {
        ApiRequest::Me => ApiResponse::Me(Ok(User {
            id: DEVICE_ID.to_owned(),
            display_name: Some("Apple Music".to_owned()),
            product: Some("premium".to_owned()),
            ..Default::default()
        })),
        ApiRequest::Devices => ApiResponse::Devices(Ok(Vec::new())),
        // Nothing plays elsewhere: the player here is the only one.
        ApiRequest::PlaybackState { seq } => ApiResponse::PlaybackState {
            seq: *seq,
            result: Ok(None),
        },
        ApiRequest::Queue { seq } => ApiResponse::Queue {
            seq: *seq,
            result: Ok(Queue::default()),
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
            let found = get(&format!("/v1/catalog/{{storefront}}/artists/{id}")).await;
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
            let found = get(&format!(
                "/v1/catalog/{{storefront}}/artists/{id}/albums?limit=25&offset={offset}"
            ))
            .await;
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
            let found = get(&format!(
                "/v1/catalog/{{storefront}}/artists/{id}/view/similar-artists?limit=20"
            ))
            .await;
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

async fn load(spec: LoadSpec) -> anyhow::Result<()> {
    let began = Instant::now();
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
        let mut offset = 0;
        loop {
            let page = playlist_tracks(playlist, offset)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let more = page.next.is_some();
            offset += page.items.len() as u32;
            ids.extend(page.items.into_iter().filter_map(|track| track.id));
            if !more || offset >= 1000 {
                break;
            }
        }
    }
    if ids.is_empty() {
        anyhow::bail!(
            "Playing {} isn't supported yet",
            spec.context_uri.as_deref().unwrap_or("this")
        );
    }
    let request = c_string(
        &json!({
            "start": start_index(&spec, &ids),
            "positionMs": spec.position_ms,
            "play": spec.play,
            "ids": ids,
        })
        .to_string(),
    );
    let (status, body) = call(|context, reply| unsafe { am_load(request.as_ptr(), context, reply) }).await;
    let body = String::from_utf8_lossy(&body);
    if status != 200 {
        anyhow::bail!("{body}");
    }
    log::info!(
        "apple: load answered in {} ms ({body})",
        began.elapsed().as_millis()
    );
    Ok(())
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
    ) -> Self {
        let client = Self {
            events,
            waker,
            asked_at: Arc::default(),
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
            emit(
                &starting.events,
                &starting.waker,
                Event::Playback(LocalPlayback::Ready {
                    device_id: DEVICE_ID.to_owned(),
                }),
            );
            start_tap(tap);
            starting.report_state().await;
        });
        client
    }

    pub fn player(&self, command: PlayerCommand) {
        // SAFETY: am_control only reads its two plain arguments.
        let control = |operation: i32, value: f64| unsafe { am_control(operation, value) };
        match command {
            PlayerCommand::Toggle => control(1, 0.0),
            PlayerCommand::Next => control(2, 0.0),
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
                let client = self.clone();
                tokio::spawn(async move {
                    let result = load(spec).await;
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
            // MusicKit has no per-app volume, and these have no meaning here yet.
            PlayerCommand::Volume(_)
            | PlayerCommand::VolumePreview(_)
            | PlayerCommand::AddToQueue(_)
            | PlayerCommand::ClearQueue
            | PlayerCommand::Transfer => {}
        }
    }

    async fn report_state(self) {
        let mut track_sequence = 0;
        let mut seek_sequence = 0;
        let mut last_id = String::new();
        let mut last_position: Option<(u32, Instant)> = None;
        let mut last_sent: Option<(Playback, String, bool, RepeatMode)> = None;
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
                _ => Playback::Stopped,
            };
            let position_ms = state["positionMs"].as_u64().unwrap_or_default() as u32;
            let id = text(&state, "id");
            if id != last_id {
                last_id.clone_from(&id);
                track_sequence += 1;
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
            let summary = (playback, id.clone(), shuffle, repeat);
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
                volume: u16::MAX,
                ..Default::default()
            };
            emit(&self.events, &self.waker, Event::Local(Box::new(local)));
        }
    }
}

// ---- The audio tap ----

struct TapSink {
    tap: Arc<crate::vis::AudioTap>,
    resampler: std::sync::Mutex<Option<(u32, Option<crate::resample::Resampler>)>>,
}

/// The rate the visualizers read; see `soloist::audio`.
const VIS_RATE: u32 = 44_100;

extern "C" fn on_audio(context: *mut c_void, samples: *const f32, count: usize, rate: u32) {
    if samples.is_null() || count == 0 {
        return;
    }
    // SAFETY: `start_tap` leaked the sink for the life of the app.
    let sink = unsafe { &*context.cast::<TapSink>() };
    // SAFETY: Swift keeps the buffer alive for the duration of this call.
    let samples = unsafe { std::slice::from_raw_parts(samples, count) };
    let mut guard = sink
        .resampler
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.as_ref().is_none_or(|(known, _)| *known != rate) {
        *guard = Some((rate, crate::resample::Resampler::new(rate, VIS_RATE, 2)));
    }
    let wide: Vec<f64> = match guard.as_mut().and_then(|(_, resampler)| resampler.as_mut()) {
        Some(resampler) => {
            let mut out = Vec::with_capacity(count * 2);
            resampler.process_into(samples, &mut out);
            out.into_iter().map(f64::from).collect()
        }
        None => samples.iter().copied().map(f64::from).collect(),
    };
    sink.tap.push(&wide, 1.0);
}

fn start_tap(tap: Arc<crate::vis::AudioTap>) {
    let sink = Box::leak(Box::new(TapSink {
        tap,
        resampler: std::sync::Mutex::new(None),
    }));
    // SAFETY: the sink lives for the life of the app, as the bridge requires.
    unsafe { am_tap_start(std::ptr::from_mut(sink).cast(), on_audio) };
}
