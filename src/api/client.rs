//! An authenticated transport for one Jellyfin server.
//!
//! Typed calls share one `send` helper that adds the session's token, limits
//! concurrency, and turns the server's refusals into errors the interface
//! can show. Answers come back as the app's own models.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Semaphore;

use super::jellyfin::{self, Item, Items, SessionInfo};
use super::models::*;
use crate::auth::Session;
use crate::http::Http;

const MAX_IN_FLIGHT: usize = 6;
/// Ids sent in one query string. A hundred 32-character ids stay well inside
/// what servers and proxies accept for a URL.
const ID_BATCH: usize = 100;
/// Songs read per request when a whole album, playlist or mix is needed.
const BULK_PAGE: u32 = 500;
/// The most songs one context loads into the player.
pub const CONTEXT_LIMIT: usize = 5_000;
/// What a song, album, artist or playlist answer carries beyond the basics.
const FIELDS: &str = "Genres,DateCreated,ChildCount,Overview,Etag";
/// The containers and codecs the player decodes itself. Anything else, the
/// server converts.
const DIRECT_PLAY: &str = "mp3,flac,m4a|aac,m4a|alac,mp4|aac,ogg|vorbis,oga|vorbis,wav,aiff";

#[derive(Clone, Debug, Error)]
pub enum ApiError {
    #[error("not signed in")]
    NotSignedIn,
    #[error("{message}")]
    Status { status: u16, message: String },
    #[error("the server is busy; try again in a moment")]
    RateLimited,
    #[error("your sign-in expired; please sign in again")]
    SignInExpired,
    #[error("network error: {0}")]
    Network(String),
    #[error("unexpected response from the server: {0}")]
    Decode(String),
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    fn unsupported(what: &str) -> Self {
        Self::Status {
            status: 0,
            message: what.to_string(),
        }
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(error: reqwest::Error) -> Self {
        let error = error.without_url();
        if error.is_decode() {
            Self::Decode(error.to_string())
        } else {
            Self::Network(error.to_string())
        }
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

/// What to start playing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayRequest {
    pub context_uri: Option<String>,
    pub uris: Vec<String>,
    pub offset_uri: Option<String>,
    pub offset_position: Option<u32>,
    pub position_ms: u32,
}

impl PlayRequest {
    pub fn context(uri: impl Into<String>) -> Self {
        Self {
            context_uri: Some(uri.into()),
            ..Self::default()
        }
    }

    pub fn tracks(uris: Vec<String>) -> Self {
        Self {
            uris,
            ..Self::default()
        }
    }

    pub fn starting_at_uri(mut self, uri: impl Into<String>) -> Self {
        self.offset_uri = Some(uri.into());
        self
    }

    pub fn starting_at_index(mut self, index: u32) -> Self {
        self.offset_position = Some(index);
        self
    }
}

/// Live view of the client's traffic, shared with the interface so it can
/// show that the app is talking to the server rather than being slow itself.
pub struct NetActivity {
    started_at: Instant,
    in_flight: AtomicUsize,
    /// Milliseconds since `started_at` when the oldest current burst began.
    busy_since_ms: AtomicU64,
}

impl Default for NetActivity {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            in_flight: AtomicUsize::new(0),
            busy_since_ms: AtomicU64::new(0),
        }
    }
}

impl NetActivity {
    fn now_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    fn begin(&self) {
        if self.in_flight.fetch_add(1, Ordering::SeqCst) == 0 {
            self.busy_since_ms.store(self.now_ms(), Ordering::SeqCst);
        }
    }

    fn end(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    /// Requests have been in flight continuously for at least `for_at_least`.
    pub fn busy(&self, for_at_least: Duration) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0
            && self
                .now_ms()
                .saturating_sub(self.busy_since_ms.load(Ordering::SeqCst))
                >= for_at_least.as_millis() as u64
    }
}

/// Decrements the in-flight count even if the request future is dropped.
struct ActivityGuard<'a>(&'a NetActivity);

impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        self.0.end();
    }
}

type Query = Vec<(&'static str, String)>;

/// A song as the player needs it: what to show, and how loud it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Playable {
    pub track: Track,
    /// The server's loudness correction for this song, in dB.
    pub gain_db: Option<f32>,
}

pub struct ApiClient {
    http: Http,
    activity: Arc<NetActivity>,
    session: Session,
    device_name: String,
    owner: Owner,
    limiter: Semaphore,
    /// Loudness corrections seen so far, by song id.
    gains: Mutex<HashMap<String, f32>>,
}

impl ApiClient {
    pub fn new(
        http: Http,
        activity: Arc<NetActivity>,
        session: Session,
        device_name: String,
    ) -> Self {
        let owner = Owner {
            id: Some(session.user_id.clone()),
            display_name: Some(session.username.clone()),
            uri: Some(jellyfin::uri("user", &session.user_id)),
        };
        Self {
            http,
            activity,
            session,
            device_name,
            owner,
            limiter: Semaphore::new(MAX_IN_FLIGHT),
            gains: Mutex::new(HashMap::new()),
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn user_id(&self) -> &str {
        &self.session.user_id
    }

    fn server(&self) -> &str {
        &self.session.server
    }

    /// The `Authorization` header every request to this server carries.
    pub fn authorization(&self) -> String {
        self.session.authorization(&self.device_name)
    }

    // ---- transport ----------------------------------------------------------

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&'static str, String)],
        body: Option<&Value>,
    ) -> Result<reqwest::Response> {
        let client = self.http.client().map_err(ApiError::Network)?;
        let _permit = self
            .limiter
            .acquire()
            .await
            .map_err(|_| ApiError::NotSignedIn)?;
        self.activity.begin();
        let _guard = ActivityGuard(&self.activity);
        let mut request = client
            .request(method, format!("{}{path}", self.server()))
            .header(reqwest::header::AUTHORIZATION, self.authorization())
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        Self::check(response).await
    }

    async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        Err(match status {
            StatusCode::UNAUTHORIZED => ApiError::SignInExpired,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                ApiError::RateLimited
            }
            StatusCode::FORBIDDEN => ApiError::Status {
                status: 403,
                message: "The server doesn't allow that for this user.".into(),
            },
            StatusCode::NOT_FOUND => ApiError::Status {
                status: 404,
                message: "The server no longer has that.".into(),
            },
            _ => {
                let body = response.text().await.unwrap_or_default();
                let detail = body.trim();
                // A reverse proxy answers with a page; only a short plain
                // message is worth showing.
                let message = if detail.is_empty() || detail.len() > 200 || detail.starts_with('<')
                {
                    format!("The server answered {status}.")
                } else {
                    detail.trim_matches('"').to_string()
                };
                ApiError::Status {
                    status: status.as_u16(),
                    message,
                }
            }
        })
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&'static str, String)],
    ) -> Result<T> {
        let response = self.send(Method::GET, path, query, None).await?;
        let bytes = response.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|error| ApiError::Decode(error.to_string()))
    }

    async fn write(
        &self,
        method: Method,
        path: &str,
        query: &[(&'static str, String)],
        body: Option<&Value>,
    ) -> Result<()> {
        self.send(method, path, query, body).await.map(drop)
    }

    fn user_query(&self) -> Query {
        vec![("UserId", self.session.user_id.clone())]
    }

    /// The query every library listing starts from.
    fn listing(&self, kinds: &str) -> Query {
        let mut query = self.user_query();
        query.extend([
            ("IncludeItemTypes", kinds.to_string()),
            ("Recursive", "true".into()),
            ("Fields", FIELDS.into()),
            ("EnableUserData", "true".into()),
            ("EnableImageTypes", "Primary".into()),
            ("ImageTypeLimit", "1".into()),
        ]);
        query
    }

    fn paged(mut query: Query, offset: u32, limit: u32) -> Query {
        query.push(("StartIndex", offset.to_string()));
        query.push(("Limit", limit.to_string()));
        query
    }

    async fn items(&self, query: Query) -> Result<Items> {
        self.get("/Items", &query).await
    }

    async fn item(&self, id: &str) -> Result<Item> {
        let mut query = self.user_query();
        query.push(("Fields", FIELDS.into()));
        self.get(&format!("/Items/{id}"), &query).await
    }

    fn track_of(&self, item: &Item) -> Track {
        if let Some(gain) = item.normalization_gain {
            self.gains
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .insert(item.id.clone(), gain);
        }
        jellyfin::track(self.server(), item)
    }

    fn tracks_of(&self, items: &[Item]) -> Vec<Track> {
        items.iter().map(|item| self.track_of(item)).collect()
    }

    /// The song with the loudness correction the server reported for it.
    pub fn playable(&self, track: Track) -> Playable {
        let gain_db = track.id.as_deref().and_then(|id| {
            self.gains
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .get(id)
                .copied()
        });
        Playable { track, gain_db }
    }

    // ---- account ------------------------------------------------------------

    pub async fn me(&self) -> Result<User> {
        let dto: jellyfin::UserDto = self.get("/Users/Me", &[]).await?;
        Ok(jellyfin::user(self.server(), &dto))
    }

    // ---- other players on the server -----------------------------------------

    /// The other sessions of this server this user may control that play music.
    async fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let sessions: Vec<SessionInfo> = self
            .get(
                "/Sessions",
                &[("ControllableByUserId", self.session.user_id.clone())],
            )
            .await?;
        Ok(sessions
            .into_iter()
            .filter(|session| {
                session.device_id.as_deref() != Some(self.session.device_id.as_str())
                    && session.supports_remote_control
                    && session
                        .playable_media_types
                        .iter()
                        .any(|kind| kind == "Audio")
            })
            .collect())
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        Ok(self
            .sessions()
            .await?
            .iter()
            .map(jellyfin::device)
            .collect())
    }

    /// What another player on this server is playing, if one plays music.
    pub async fn playback_state(&self) -> Result<Option<PlaybackState>> {
        Ok(self
            .sessions()
            .await?
            .iter()
            .find_map(|session| jellyfin::playback_state(self.server(), session)))
    }

    /// The player to address when none was named: the one playing music.
    async fn target_session(&self, device_id: Option<&str>) -> Result<String> {
        if let Some(id) = device_id {
            return Ok(id.to_string());
        }
        self.sessions()
            .await?
            .into_iter()
            .find(|session| session.now_playing_item.is_some())
            .map(|session| session.id)
            .ok_or_else(|| ApiError::unsupported("No other player is active."))
    }

    async fn playstate(&self, device_id: Option<&str>, command: &str, query: Query) -> Result<()> {
        let session = self.target_session(device_id).await?;
        self.write(
            Method::POST,
            &format!("/Sessions/{session}/Playing/{command}"),
            &query,
            None,
        )
        .await
    }

    async fn general_command(
        &self,
        device_id: Option<&str>,
        name: &str,
        arguments: Value,
    ) -> Result<()> {
        let session = self.target_session(device_id).await?;
        self.write(
            Method::POST,
            &format!("/Sessions/{session}/Command"),
            &[],
            Some(&json!({ "Name": name, "Arguments": arguments })),
        )
        .await
    }

    /// Starts `request` on another player, or resumes it when there is none.
    pub async fn play(&self, device_id: Option<&str>, request: Option<&PlayRequest>) -> Result<()> {
        let Some(request) = request else {
            return self.playstate(device_id, "Unpause", Vec::new()).await;
        };
        let (tracks, start) = self.resolve(request).await?;
        if tracks.is_empty() {
            return Err(ApiError::unsupported("Nothing to play there."));
        }
        self.send_play(device_id, &tracks, start, request.position_ms, "PlayNow")
            .await
    }

    async fn send_play(
        &self,
        device_id: Option<&str>,
        tracks: &[Playable],
        start: usize,
        position_ms: u32,
        command: &str,
    ) -> Result<()> {
        let session = self.target_session(device_id).await?;
        // The whole list travels in the query string, so it is cut to what
        // surrounds the chosen song.
        let from = start.min(tracks.len().saturating_sub(1));
        let window = &tracks[from..(from + ID_BATCH).min(tracks.len())];
        let ids: Vec<&str> = window
            .iter()
            .filter_map(|playable| playable.track.id.as_deref())
            .collect();
        let mut query: Query = vec![
            ("playCommand", command.to_string()),
            ("itemIds", ids.join(",")),
        ];
        if position_ms > 0 {
            query.push((
                "startPositionTicks",
                jellyfin::ms_to_ticks(position_ms).to_string(),
            ));
        }
        self.write(
            Method::POST,
            &format!("/Sessions/{session}/Playing"),
            &query,
            None,
        )
        .await
    }

    pub async fn pause(&self, device_id: Option<&str>) -> Result<()> {
        self.playstate(device_id, "Pause", Vec::new()).await
    }

    pub async fn next(&self, device_id: Option<&str>) -> Result<()> {
        self.playstate(device_id, "NextTrack", Vec::new()).await
    }

    pub async fn previous(&self, device_id: Option<&str>) -> Result<()> {
        self.playstate(device_id, "PreviousTrack", Vec::new()).await
    }

    pub async fn seek(&self, position_ms: u32, device_id: Option<&str>) -> Result<()> {
        self.playstate(
            device_id,
            "Seek",
            vec![(
                "seekPositionTicks",
                jellyfin::ms_to_ticks(position_ms).to_string(),
            )],
        )
        .await
    }

    pub async fn set_volume(&self, percent: u8, device_id: Option<&str>) -> Result<()> {
        self.general_command(
            device_id,
            "SetVolume",
            json!({ "Volume": percent.min(100).to_string() }),
        )
        .await
    }

    pub async fn set_shuffle(&self, state: bool, device_id: Option<&str>) -> Result<()> {
        self.general_command(
            device_id,
            "SetShuffleQueue",
            json!({ "ShuffleMode": if state { "Shuffle" } else { "Sorted" } }),
        )
        .await
    }

    pub async fn set_repeat(&self, state: &str, device_id: Option<&str>) -> Result<()> {
        let mode = match state {
            "context" => "RepeatAll",
            "track" => "RepeatOne",
            _ => "RepeatNone",
        };
        self.general_command(device_id, "SetRepeatMode", json!({ "RepeatMode": mode }))
            .await
    }

    /// Adds songs to the end of another player's queue, in order.
    pub async fn add_to_queue(&self, uris: &[String], device_id: Option<&str>) -> Result<()> {
        let tracks = self.tracks_by_uri(uris).await?;
        if tracks.is_empty() {
            return Err(ApiError::unsupported("Only songs can be queued."));
        }
        for batch in tracks.chunks(ID_BATCH) {
            self.send_play(device_id, batch, 0, 0, "PlayLast").await?;
        }
        Ok(())
    }

    // ---- playlists ----------------------------------------------------------

    pub async fn my_playlists(&self, offset: u32, limit: u32) -> Result<Page<Playlist>> {
        let mut query = Self::paged(self.listing("Playlist"), offset, limit);
        query.push(("SortBy", "SortName".into()));
        let found = self.items(query).await?;
        let playlists = found
            .items
            .iter()
            // A playlist of films has nothing for a music player.
            .filter(|item| item.media_type.as_deref() != Some("Video"))
            .map(|item| jellyfin::playlist(self.server(), item, &self.owner))
            .collect();
        Ok(Page {
            // Skipped playlists still count towards the offset of the next page.
            next: (offset + (found.items.len() as u32) < found.total_record_count
                && !found.items.is_empty())
            .then(|| "more".into()),
            items: playlists,
            total: found.total_record_count,
            limit: found.items.len() as u32,
            offset,
        })
    }

    pub async fn playlist(&self, id: &str) -> Result<Playlist> {
        let item = self.item(id).await?;
        Ok(jellyfin::playlist(self.server(), &item, &self.owner))
    }

    async fn playlist_entries(
        &self,
        id: &str,
        offset: u32,
        limit: u32,
        full: bool,
    ) -> Result<Items> {
        let mut query = Self::paged(self.user_query(), offset, limit);
        if full {
            query.extend([
                ("Fields", FIELDS.into()),
                ("EnableUserData", "true".into()),
                ("EnableImageTypes", "Primary".to_string()),
            ]);
        } else {
            query.push(("EnableUserData", "false".into()));
            query.push(("EnableImages", "false".into()));
        }
        self.get(&format!("/Playlists/{id}/Items"), &query).await
    }

    pub async fn playlist_items(
        &self,
        id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<PlaylistItem>> {
        let found = self.playlist_entries(id, offset, limit, true).await?;
        let items = found
            .items
            .iter()
            .map(|item| {
                let mut entry = jellyfin::playlist_item(self.server(), item, self.user_id());
                if let Some(PlayableItem::Track(track)) = &mut entry.item {
                    *track = self.track_of(item);
                }
                entry
            })
            .collect();
        Ok(jellyfin::page(
            items,
            found.total_record_count,
            offset,
            limit,
        ))
    }

    /// Every entry of a playlist as (entry id, song id), in order.
    async fn playlist_entry_ids(&self, id: &str) -> Result<Vec<(String, String)>> {
        let mut entries = Vec::new();
        let mut offset = 0;
        loop {
            let found = self.playlist_entries(id, offset, BULK_PAGE, false).await?;
            let count = found.items.len() as u32;
            entries.extend(found.items.into_iter().map(|item| {
                (
                    item.playlist_item_id.unwrap_or_else(|| item.id.clone()),
                    item.id,
                )
            }));
            offset += count;
            if count == 0 || offset >= found.total_record_count {
                break;
            }
        }
        Ok(entries)
    }

    /// The URIs among `uris` the playlist already holds.
    pub async fn playlist_duplicates(&self, id: &str, uris: &[String]) -> Result<Vec<String>> {
        let present: HashSet<String> = self
            .playlist_entry_ids(id)
            .await?
            .into_iter()
            .map(|(_, song)| song)
            .collect();
        Ok(uris
            .iter()
            .filter(|uri| crate::util::uri_id(uri).is_some_and(|song| present.contains(song)))
            .cloned()
            .collect())
    }

    pub async fn create_playlist(
        &self,
        name: &str,
        public: bool,
        description: &str,
    ) -> Result<Playlist> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Created {
            id: String,
        }
        let response = self
            .send(
                Method::POST,
                "/Playlists",
                &[],
                Some(&json!({
                    "Name": name,
                    "UserId": self.session.user_id,
                    "MediaType": "Audio",
                    "IsPublic": public,
                    "Ids": [],
                })),
            )
            .await?;
        let created: Created = response.json().await?;
        if !description.trim().is_empty() {
            self.update_playlist(&created.id, None, Some(description), None)
                .await?;
        }
        self.playlist(&created.id).await
    }

    /// The server takes pictures as Base64 text with the picture's type.
    pub async fn upload_playlist_cover(&self, id: &str, encoded: &str) -> Result<()> {
        let client = self.http.client().map_err(ApiError::Network)?;
        self.activity.begin();
        let _guard = ActivityGuard(&self.activity);
        let response = client
            .post(format!("{}/Items/{id}/Images/Primary", self.server()))
            .header(reqwest::header::AUTHORIZATION, self.authorization())
            .header(reqwest::header::CONTENT_TYPE, "image/jpeg")
            .body(encoded.to_string())
            .send()
            .await?;
        Self::check(response).await.map(drop)
    }

    pub async fn update_playlist(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
        public: Option<bool>,
    ) -> Result<()> {
        if name.is_some() || description.is_some() {
            // The item editor takes the whole item back, so it is read
            // first and only the edited fields are changed.
            let mut item: Value = self
                .get(&format!("/Items/{id}"), &self.user_query())
                .await?;
            if let Some(name) = name {
                item["Name"] = json!(name);
            }
            if let Some(description) = description {
                item["Overview"] = json!(description);
            }
            self.write(Method::POST, &format!("/Items/{id}"), &[], Some(&item))
                .await?;
        }
        if let Some(public) = public {
            self.write(
                Method::POST,
                &format!("/Playlists/{id}"),
                &[],
                Some(&json!({ "IsPublic": public })),
            )
            .await?;
        }
        Ok(())
    }

    async fn playlist_snapshot(&self, id: &str) -> Option<String> {
        self.item(id)
            .await
            .ok()
            .and_then(|item| jellyfin::snapshot(&item))
    }

    pub async fn add_playlist_items(
        &self,
        id: &str,
        uris: &[String],
        position: Option<u32>,
    ) -> Result<Option<String>> {
        let ids: Vec<&str> = uris
            .iter()
            .filter(|uri| crate::util::uri_kind(uri) == Some("track"))
            .filter_map(|uri| crate::util::uri_id(uri))
            .collect();
        if ids.is_empty() {
            return Err(ApiError::unsupported("Only songs can go in a playlist."));
        }
        // New entries land at the end; a chosen position is reached by
        // moving them there afterwards.
        let before = match position {
            Some(_) => Some(
                self.playlist_entries(id, 0, 0, false)
                    .await?
                    .total_record_count,
            ),
            None => None,
        };
        for batch in ids.chunks(ID_BATCH) {
            let mut query = self.user_query();
            query.push(("Ids", batch.join(",")));
            self.write(
                Method::POST,
                &format!("/Playlists/{id}/Items"),
                &query,
                None,
            )
            .await?;
        }
        if let (Some(position), Some(before)) = (position, before)
            && position < before
        {
            let added = self
                .playlist_entries(id, before, ids.len() as u32, false)
                .await?;
            for (index, item) in added.items.iter().enumerate() {
                let Some(entry) = item.playlist_item_id.as_deref() else {
                    continue;
                };
                self.write(
                    Method::POST,
                    &format!(
                        "/Playlists/{id}/Items/{entry}/Move/{}",
                        position + index as u32
                    ),
                    &[],
                    None,
                )
                .await?;
            }
        }
        Ok(self.playlist_snapshot(id).await)
    }

    /// Removes every entry of each named song.
    pub async fn remove_playlist_items(&self, id: &str, uris: &[String]) -> Result<Option<String>> {
        let songs: HashSet<&str> = uris
            .iter()
            .filter_map(|uri| crate::util::uri_id(uri))
            .collect();
        let entries: Vec<String> = self
            .playlist_entry_ids(id)
            .await?
            .into_iter()
            .filter(|(_, song)| songs.contains(song.as_str()))
            .map(|(entry, _)| entry)
            .collect();
        for batch in entries.chunks(ID_BATCH) {
            self.write(
                Method::DELETE,
                &format!("/Playlists/{id}/Items"),
                &[("EntryIds", batch.join(","))],
                None,
            )
            .await?;
        }
        Ok(self.playlist_snapshot(id).await)
    }

    /// Moves the entry at `range_start` so it sits before what was at
    /// `insert_before`.
    pub async fn reorder_playlist(
        &self,
        id: &str,
        range_start: u32,
        insert_before: u32,
    ) -> Result<Option<String>> {
        let found = self.playlist_entries(id, range_start, 1, false).await?;
        let entry = found
            .items
            .first()
            .and_then(|item| item.playlist_item_id.clone())
            .ok_or_else(|| ApiError::unsupported("That song is no longer in the playlist."))?;
        let target = if insert_before > range_start {
            insert_before - 1
        } else {
            insert_before
        };
        self.write(
            Method::POST,
            &format!("/Playlists/{id}/Items/{entry}/Move/{target}"),
            &[],
            None,
        )
        .await?;
        Ok(self.playlist_snapshot(id).await)
    }

    /// Deletes a playlist from the server. The songs in it stay.
    pub async fn delete_playlist(&self, id: &str) -> Result<()> {
        self.write(Method::DELETE, &format!("/Items/{id}"), &[], None)
            .await
    }

    // ---- library ------------------------------------------------------------

    /// Favourite songs, newest in the library first.
    pub async fn saved_tracks(&self, offset: u32, limit: u32) -> Result<Page<SavedTrack>> {
        let mut query = Self::paged(self.listing("Audio"), offset, limit);
        query.extend([
            ("Filters", "IsFavorite".into()),
            ("SortBy", "DateCreated,SortName".into()),
            ("SortOrder", "Descending".to_string()),
        ]);
        let found = self.items(query).await?;
        let items = found
            .items
            .iter()
            .map(|item| SavedTrack {
                added_at: item.date_created.clone(),
                track: self.track_of(item),
            })
            .collect();
        Ok(jellyfin::page(
            items,
            found.total_record_count,
            offset,
            limit,
        ))
    }

    /// Every album in the library, by name.
    pub async fn saved_albums(&self, offset: u32, limit: u32) -> Result<Page<SavedAlbum>> {
        let mut query = Self::paged(self.listing("MusicAlbum"), offset, limit);
        query.push(("SortBy", "SortName".into()));
        let found = self.items(query).await?;
        let items = found
            .items
            .iter()
            .map(|item| SavedAlbum {
                added_at: item.date_created.clone(),
                album: jellyfin::album(self.server(), item),
            })
            .collect();
        Ok(jellyfin::page(
            items,
            found.total_record_count,
            offset,
            limit,
        ))
    }

    /// The newest albums in the library.
    pub async fn latest_albums(&self, limit: u32) -> Result<Vec<Album>> {
        let mut query = Self::paged(self.listing("MusicAlbum"), 0, limit);
        query.extend([
            ("SortBy", "DateCreated,SortName".into()),
            ("SortOrder", "Descending".to_string()),
        ]);
        let found = self.items(query).await?;
        Ok(found
            .items
            .iter()
            .map(|item| jellyfin::album(self.server(), item))
            .collect())
    }

    /// Every album artist in the library, by name. The cursor is the offset
    /// of the next page.
    pub async fn followed_artists(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<CursorPage<Artist>> {
        let offset: u32 = after.and_then(|after| after.parse().ok()).unwrap_or(0);
        let mut query = Self::paged(self.user_query(), offset, limit);
        query.extend([
            ("Fields", FIELDS.into()),
            ("EnableUserData", "true".into()),
            ("EnableImageTypes", "Primary".into()),
            ("SortBy", "SortName".to_string()),
        ]);
        let found: Items = self.get("/Artists/AlbumArtists", &query).await?;
        let end = offset + found.items.len() as u32;
        let more = end < found.total_record_count && !found.items.is_empty();
        Ok(CursorPage {
            items: found
                .items
                .iter()
                .map(|item| jellyfin::artist(self.server(), item))
                .collect(),
            total: Some(found.total_record_count),
            next: more.then(|| "more".into()),
            cursors: Some(Cursors {
                after: more.then(|| end.to_string()),
                before: None,
            }),
        })
    }

    /// Marks songs, albums, artists or playlists as favourites, or not.
    pub async fn set_saved(&self, uris: &[String], saved: bool) -> Result<()> {
        let method = if saved { Method::POST } else { Method::DELETE };
        for uri in uris {
            let Some(id) = crate::util::uri_id(uri) else {
                continue;
            };
            let result = self
                .write(
                    method.clone(),
                    &format!("/UserFavoriteItems/{id}"),
                    &[("userId", self.session.user_id.clone())],
                    None,
                )
                .await;
            match result {
                // Servers before 10.9 only know the address with the user in it.
                Err(ApiError::Status {
                    status: 404 | 405, ..
                }) => {
                    self.write(
                        method.clone(),
                        &format!("/Users/{}/FavoriteItems/{id}", self.session.user_id),
                        &[],
                        None,
                    )
                    .await?
                }
                other => other?,
            }
        }
        Ok(())
    }

    /// Whether each of `uris` is a favourite, in the order asked.
    pub async fn contains(&self, uris: &[String]) -> Result<Vec<bool>> {
        let ids: Vec<&str> = uris
            .iter()
            .filter_map(|uri| crate::util::uri_id(uri))
            .collect();
        let mut favourites = HashSet::new();
        for batch in ids.chunks(ID_BATCH) {
            let mut query = self.user_query();
            query.extend([
                ("Ids", batch.join(",")),
                ("EnableUserData", "true".into()),
                ("EnableImages", "false".to_string()),
            ]);
            let found = self.items(query).await?;
            favourites.extend(
                found
                    .items
                    .into_iter()
                    .filter(Item::is_favorite)
                    .map(|item| item.id),
            );
        }
        Ok(uris
            .iter()
            .map(|uri| crate::util::uri_id(uri).is_some_and(|id| favourites.contains(id)))
            .collect())
    }

    // ---- listening ----------------------------------------------------------

    /// Songs by when they were last played, newest first. The cursor is the
    /// offset of the next page.
    pub async fn recently_played(
        &self,
        limit: u32,
        before: Option<&str>,
    ) -> Result<CursorPage<PlayHistory>> {
        let offset: u32 = before.and_then(|before| before.parse().ok()).unwrap_or(0);
        let mut query = Self::paged(self.listing("Audio"), offset, limit);
        query.extend([
            ("Filters", "IsPlayed".into()),
            ("SortBy", "DatePlayed".into()),
            ("SortOrder", "Descending".to_string()),
        ]);
        let found = self.items(query).await?;
        let end = offset + found.items.len() as u32;
        let more = end < found.total_record_count && !found.items.is_empty();
        Ok(CursorPage {
            items: found
                .items
                .iter()
                .map(|item| PlayHistory {
                    track: self.track_of(item),
                    played_at: item
                        .user_data
                        .as_ref()
                        .and_then(|data| data.last_played_date.clone()),
                    context: None,
                })
                .collect(),
            total: Some(found.total_record_count),
            next: more.then(|| "more".into()),
            cursors: Some(Cursors {
                after: None,
                before: more.then(|| end.to_string()),
            }),
        })
    }

    /// The songs played most.
    pub async fn top_tracks(&self, limit: u32, offset: u32) -> Result<Page<Track>> {
        let mut query = Self::paged(self.listing("Audio"), offset, limit);
        query.extend([
            ("Filters", "IsPlayed".into()),
            ("SortBy", "PlayCount,SortName".into()),
            ("SortOrder", "Descending".to_string()),
        ]);
        let found = self.items(query).await?;
        Ok(jellyfin::page(
            self.tracks_of(&found.items),
            found.total_record_count,
            offset,
            limit,
        ))
    }

    /// The artists of the songs played most, most played first. The server
    /// keeps no play count for an artist.
    pub async fn top_artists(&self, limit: u32) -> Result<Vec<Artist>> {
        let top = self.top_tracks(100, 0).await?;
        let mut ids: Vec<String> = Vec::new();
        for track in &top.items {
            for artist in &track.artists {
                if let Some(id) = &artist.id
                    && !ids.contains(id)
                {
                    ids.push(id.clone());
                }
            }
        }
        ids.truncate(limit as usize);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = self.user_query();
        query.extend([
            ("Ids", ids.join(",")),
            ("Fields", FIELDS.into()),
            ("EnableImageTypes", "Primary".to_string()),
        ]);
        let found = self.items(query).await?;
        let mut by_id: HashMap<&str, &Item> = found
            .items
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect();
        Ok(ids
            .iter()
            .filter_map(|id| by_id.remove(id.as_str()))
            .map(|item| jellyfin::artist(self.server(), item))
            .collect())
    }

    /// The server's mix seeded by a song, album, artist or playlist.
    pub async fn instant_mix(&self, id: &str, limit: u32) -> Result<Vec<Track>> {
        let mut query = self.user_query();
        query.extend([
            ("Limit", limit.to_string()),
            ("Fields", FIELDS.into()),
            ("EnableUserData", "true".into()),
            ("EnableImageTypes", "Primary".to_string()),
        ]);
        let found: Items = self.get(&format!("/Items/{id}/InstantMix"), &query).await?;
        Ok(self.tracks_of(&found.items))
    }

    // ---- search -------------------------------------------------------------

    pub async fn search(&self, text: &str, limit: u32) -> Result<SearchResults> {
        let listing = |kinds: &str| {
            let mut query = Self::paged(self.listing(kinds), 0, limit);
            query.push(("SearchTerm", text.to_string()));
            query
        };
        let mut artist_query = Self::paged(self.user_query(), 0, limit);
        artist_query.extend([
            ("SearchTerm", text.to_string()),
            ("Fields", FIELDS.into()),
            ("EnableImageTypes", "Primary".to_string()),
        ]);
        let (tracks, albums, playlists, artists) = tokio::join!(
            self.items(listing("Audio")),
            self.items(listing("MusicAlbum")),
            self.items(listing("Playlist")),
            self.get::<Items>("/Artists", &artist_query),
        );
        let tracks = tracks?;
        let albums = albums?;
        let playlists = playlists?;
        let artists = artists?;
        Ok(SearchResults {
            tracks: Some(jellyfin::page(
                self.tracks_of(&tracks.items),
                tracks.total_record_count,
                0,
                limit,
            )),
            artists: Some(jellyfin::page(
                artists
                    .items
                    .iter()
                    .map(|item| jellyfin::artist(self.server(), item))
                    .collect(),
                artists.total_record_count,
                0,
                limit,
            )),
            albums: Some(jellyfin::page(
                albums
                    .items
                    .iter()
                    .map(|item| jellyfin::album(self.server(), item))
                    .collect(),
                albums.total_record_count,
                0,
                limit,
            )),
            playlists: Some(jellyfin::page(
                playlists
                    .items
                    .iter()
                    .filter(|item| item.media_type.as_deref() != Some("Video"))
                    .map(|item| jellyfin::playlist(self.server(), item, &self.owner))
                    .collect(),
                playlists.total_record_count,
                0,
                limit,
            )),
            shows: None,
            episodes: None,
        })
    }

    // ---- catalogue ----------------------------------------------------------

    pub async fn artist(&self, id: &str) -> Result<Artist> {
        Ok(jellyfin::artist(self.server(), &self.item(id).await?))
    }

    /// An artist's most played songs, or the first few by name when none has
    /// been played.
    pub async fn artist_top_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let mut query = Self::paged(self.listing("Audio"), 0, 10);
        query.extend([
            ("ArtistIds", id.to_string()),
            ("SortBy", "PlayCount,SortName".into()),
            ("SortOrder", "Descending,Ascending".to_string()),
        ]);
        Ok(self.tracks_of(&self.items(query).await?.items))
    }

    /// An artist's own albums, or with `appears_on` the albums of others the
    /// artist plays on. Newest first.
    pub async fn artist_albums(
        &self,
        id: &str,
        groups: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Album>> {
        let appears_on = groups.split(',').all(|group| group == "appears_on");
        let mut query = Self::paged(self.listing("MusicAlbum"), offset, limit);
        query.extend([
            (
                if appears_on {
                    "ContributingArtistIds"
                } else {
                    "AlbumArtistIds"
                },
                id.to_string(),
            ),
            ("SortBy", "PremiereDate,ProductionYear,SortName".into()),
            ("SortOrder", "Descending".to_string()),
        ]);
        let found = self.items(query).await?;
        let group = if appears_on { "appears_on" } else { "album" };
        Ok(jellyfin::page(
            found
                .items
                .iter()
                .map(|item| Album {
                    album_group: Some(group.into()),
                    ..jellyfin::album(self.server(), item)
                })
                .collect(),
            found.total_record_count,
            offset,
            limit,
        ))
    }

    pub async fn related_artists(&self, id: &str) -> Result<Vec<Artist>> {
        let mut query = self.user_query();
        query.extend([
            ("Limit", "20".to_string()),
            ("Fields", FIELDS.into()),
            ("EnableImageTypes", "Primary".to_string()),
        ]);
        let found: Items = self.get(&format!("/Artists/{id}/Similar"), &query).await?;
        Ok(found
            .items
            .iter()
            .map(|item| jellyfin::artist(self.server(), item))
            .collect())
    }

    /// An album with its first page of songs.
    pub async fn album(&self, id: &str) -> Result<Album> {
        let (item, tracks) = tokio::join!(self.item(id), self.album_tracks(id, 0, 50));
        let mut album = jellyfin::album(self.server(), &item?);
        let tracks = tracks?;
        album.total_tracks = Some(tracks.total);
        album.tracks = Some(tracks);
        Ok(album)
    }

    pub async fn album_tracks(&self, id: &str, offset: u32, limit: u32) -> Result<Page<Track>> {
        let mut query = Self::paged(self.listing("Audio"), offset, limit);
        query.extend([
            ("AlbumIds", id.to_string()),
            (
                "SortBy",
                "ParentIndexNumber,IndexNumber,SortName".to_string(),
            ),
        ]);
        let found = self.items(query).await?;
        Ok(jellyfin::page(
            self.tracks_of(&found.items),
            found.total_record_count,
            offset,
            limit,
        ))
    }

    pub async fn track(&self, id: &str) -> Result<Track> {
        Ok(self.track_of(&self.item(id).await?))
    }

    /// The words of a song as the server holds them; `None` when it has none.
    pub async fn lyrics(&self, id: &str) -> Result<Option<crate::lyrics::Lyrics>> {
        match self
            .get::<jellyfin::LyricDto>(&format!("/Audio/{id}/Lyrics"), &[])
            .await
        {
            Ok(dto) => Ok(jellyfin::lyrics(&dto)),
            Err(ApiError::Status { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    // ---- what the player loads ------------------------------------------------

    /// Songs for `uris`, in the order given. URIs that are not songs, and
    /// songs the server no longer has, are left out.
    pub async fn tracks_by_uri(&self, uris: &[String]) -> Result<Vec<Playable>> {
        let ids: Vec<&str> = uris
            .iter()
            .filter(|uri| crate::util::uri_kind(uri) == Some("track"))
            .filter_map(|uri| crate::util::uri_id(uri))
            .collect();
        let mut found: HashMap<String, Track> = HashMap::new();
        let unique: Vec<&str> = {
            let mut seen = HashSet::new();
            ids.iter().copied().filter(|id| seen.insert(*id)).collect()
        };
        for batch in unique.chunks(ID_BATCH) {
            let mut query = self.user_query();
            query.extend([
                ("Ids", batch.join(",")),
                ("Fields", FIELDS.into()),
                ("EnableUserData", "true".into()),
                ("EnableImageTypes", "Primary".to_string()),
            ]);
            for item in &self.items(query).await?.items {
                found.insert(item.id.clone(), self.track_of(item));
            }
        }
        Ok(ids
            .iter()
            .filter_map(|id| found.get(*id).cloned())
            .map(|track| self.playable(track))
            .collect())
    }

    async fn all_tracks(&self, mut query: Query) -> Result<Vec<Track>> {
        let mut tracks = Vec::new();
        let mut offset = 0u32;
        loop {
            let mut page_query = query.clone();
            page_query.push(("StartIndex", offset.to_string()));
            page_query.push(("Limit", BULK_PAGE.to_string()));
            let found = self.items(page_query).await?;
            let count = found.items.len() as u32;
            tracks.extend(self.tracks_of(&found.items));
            offset += count;
            if count == 0 || offset >= found.total_record_count || tracks.len() >= CONTEXT_LIMIT {
                break;
            }
        }
        query.clear();
        tracks.truncate(CONTEXT_LIMIT);
        Ok(tracks)
    }

    /// Every song of an album, playlist, artist, mix or the favourites, in
    /// the order it plays.
    pub async fn context_tracks(&self, context: &str) -> Result<Vec<Playable>> {
        let kind = crate::util::uri_kind(context).unwrap_or_default();
        let id = crate::util::uri_id(context).unwrap_or_default();
        let tracks = match kind {
            "track" => vec![self.track(id).await?],
            "album" => {
                let mut query = self.listing("Audio");
                query.extend([
                    ("AlbumIds", id.to_string()),
                    (
                        "SortBy",
                        "ParentIndexNumber,IndexNumber,SortName".to_string(),
                    ),
                ]);
                self.all_tracks(query).await?
            }
            "artist" => {
                let mut query = self.listing("Audio");
                query.extend([
                    ("ArtistIds", id.to_string()),
                    (
                        "SortBy",
                        "Album,ParentIndexNumber,IndexNumber,SortName".to_string(),
                    ),
                ]);
                self.all_tracks(query).await?
            }
            "playlist" => {
                let mut tracks = Vec::new();
                let mut offset = 0u32;
                loop {
                    let found = self.playlist_entries(id, offset, BULK_PAGE, true).await?;
                    let count = found.items.len() as u32;
                    tracks.extend(
                        found
                            .items
                            .iter()
                            .filter(|item| item.kind == "Audio" || item.kind.is_empty())
                            .map(|item| self.track_of(item)),
                    );
                    offset += count;
                    if count == 0
                        || offset >= found.total_record_count
                        || tracks.len() >= CONTEXT_LIMIT
                    {
                        break;
                    }
                }
                tracks
            }
            "station" => {
                let seed = crate::util::station_seed(context)
                    .ok_or_else(|| ApiError::unsupported("That mix can't be played."))?;
                let seed_id = crate::util::uri_id(&seed).unwrap_or_default().to_string();
                self.instant_mix(&seed_id, 100).await?
            }
            "user" if context.ends_with(":collection") => {
                let mut query = self.listing("Audio");
                query.extend([
                    ("Filters", "IsFavorite".into()),
                    ("SortBy", "DateCreated,SortName".into()),
                    ("SortOrder", "Descending".to_string()),
                ]);
                self.all_tracks(query).await?
            }
            _ => return Err(ApiError::unsupported("That can't be played.")),
        };
        Ok(tracks
            .into_iter()
            .map(|track| self.playable(track))
            .collect())
    }

    /// The songs a play request names and the index to start at.
    pub async fn resolve(&self, request: &PlayRequest) -> Result<(Vec<Playable>, usize)> {
        let tracks = match &request.context_uri {
            Some(context) => self.context_tracks(context).await?,
            None => self.tracks_by_uri(&request.uris).await?,
        };
        let by_uri = request
            .offset_uri
            .as_deref()
            .and_then(|uri| tracks.iter().position(|playable| playable.track.uri == uri));
        let start = by_uri
            .or_else(|| {
                // A list names its rows by position; a row the server no
                // longer has shifts the ones after it, so the chosen song is
                // looked up by name first.
                let position = request.offset_position? as usize;
                request
                    .uris
                    .get(position)
                    .and_then(|uri| {
                        tracks
                            .iter()
                            .position(|playable| &playable.track.uri == uri)
                    })
                    .or(Some(position))
            })
            .unwrap_or(0)
            .min(tracks.len().saturating_sub(1));
        Ok((tracks, start))
    }

    /// Where a song streams from: the file itself when the player can decode
    /// it within `max_bitrate`, the server's MP3 conversion otherwise.
    pub fn stream_url(&self, id: &str, max_bitrate: Option<u32>, play_session: &str) -> String {
        let mut url = reqwest::Url::parse(&format!("{}/Audio/{id}/universal", self.server()))
            .expect("a session's server address was parsed at sign-in");
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("UserId", &self.session.user_id)
                .append_pair("DeviceId", &self.session.device_id)
                .append_pair("PlaySessionId", play_session)
                .append_pair("Container", DIRECT_PLAY)
                .append_pair("TranscodingContainer", "mp3")
                .append_pair("TranscodingProtocol", "http")
                .append_pair("AudioCodec", "mp3")
                .append_pair("MaxAudioChannels", "2");
            // Without a ceiling the server sends the file as it is.
            let ceiling = max_bitrate.unwrap_or(140_000_000);
            query.append_pair("MaxStreamingBitrate", &ceiling.to_string());
            // What a conversion is made at, when one is needed.
            let converted = max_bitrate.map_or(320_000, |bitrate| bitrate.min(320_000));
            query.append_pair("AudioBitRate", &converted.to_string());
        }
        url.into()
    }

    // ---- telling the server what plays ---------------------------------------

    /// Reports playback to the server, which keeps play counts, "last
    /// played" and the dashboard's activity from it.
    pub async fn report(&self, report: &PlaybackReport) -> Result<()> {
        let path = match report.event {
            ReportEvent::Start => "/Sessions/Playing",
            ReportEvent::Progress => "/Sessions/Playing/Progress",
            ReportEvent::Stop => "/Sessions/Playing/Stopped",
        };
        self.write(
            Method::POST,
            path,
            &[],
            Some(&json!({
                "ItemId": report.item_id,
                "PlaySessionId": report.play_session,
                "PositionTicks": jellyfin::ms_to_ticks(report.position_ms),
                "IsPaused": report.paused,
                "IsMuted": false,
                "CanSeek": true,
                "PlayMethod": "DirectPlay",
                "VolumeLevel": report.volume_percent,
                "RepeatMode": report.repeat,
                "PlaybackOrder": if report.shuffle { "Shuffle" } else { "Default" },
            })),
        )
        .await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportEvent {
    Start,
    Progress,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackReport {
    pub event: ReportEvent,
    pub item_id: String,
    pub play_session: String,
    pub position_ms: u32,
    pub paused: bool,
    pub volume_percent: u8,
    pub repeat: &'static str,
    pub shuffle: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> ApiClient {
        ApiClient::new(
            Http::default(),
            Arc::new(NetActivity::default()),
            Session {
                server: "http://nas.local:8096".into(),
                server_id: "server".into(),
                server_name: "NAS".into(),
                user_id: "user".into(),
                username: "jack".into(),
                token: "secret-token".into(),
                device_id: "device".into(),
            },
            "Desk".into(),
        )
    }

    #[test]
    fn the_stream_address_never_carries_the_token() {
        let client = client();
        let original = client.stream_url("song", None, "play");
        assert!(original.starts_with("http://nas.local:8096/Audio/song/universal?"));
        assert!(!original.contains("secret-token"));
        assert!(original.contains("MaxStreamingBitrate=140000000"));
        assert!(original.contains("AudioBitRate=320000"));
        let capped = client.stream_url("song", Some(160_000), "play");
        assert!(capped.contains("MaxStreamingBitrate=160000"));
        assert!(capped.contains("AudioBitRate=160000"));
        assert!(capped.contains("TranscodingContainer=mp3"));
    }

    #[test]
    fn the_token_travels_in_the_authorization_header() {
        let header = client().authorization();
        assert!(header.starts_with("MediaBrowser Client=\"Jellifast\""));
        assert!(header.ends_with("Token=\"secret-token\""));
        assert!(header.contains("DeviceId=\"device\""));
    }

    #[test]
    fn a_song_carries_the_gain_the_server_reported() {
        let client = client();
        let item: Item = serde_json::from_str(
            r#"{"Id": "a", "Name": "Loud", "Type": "Audio", "NormalizationGain": -6.5}"#,
        )
        .unwrap();
        let track = client.track_of(&item);
        assert_eq!(client.playable(track).gain_db, Some(-6.5));
        let quiet = Track {
            id: Some("unknown".into()),
            ..Track::default()
        };
        assert_eq!(client.playable(quiet).gain_db, None);
    }
}
