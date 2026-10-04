//! Bridge between the UI thread and asynchronous work.
//!
//! egui runs on the main thread and must never block. A dedicated tokio
//! runtime hosts the Jellyfin client, sign-in, the player's downloads, and
//! artwork fetches; the two sides talk through channels. Every event wakes
//! the interface with `request_repaint`, so the app stays event-driven and
//! idle when nothing is happening.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};

use crate::api::client::{PlaybackReport, ReportEvent};
use crate::api::models::*;
use crate::api::{ApiClient, ApiError, NetActivity, PlayRequest};
use crate::auth::{Login, Session};
use crate::credentials::{
    Grant as StoredGrant, Lease as CredentialLease, Slot as CredentialSlot,
    Store as CredentialStore,
};
use crate::http::Http;
use crate::images::{ArtLoader, accent_color};
use crate::model::PlaylistCache;
use crate::paths::AppDirs;
use crate::player::{
    Engine, EngineConfig, EngineEvent, Heard, Load, LoadSpec, LocalState, Playback, PlaybackResume,
    PlayerCommand,
};
use crate::settings::ProxyConfig;

pub type ApiResult<T> = Result<T, ApiError>;

pub const PLAYLIST_PAGE_SIZE: u32 = 50;
/// Songs in a mix the server makes from a seed.
const RADIO_SIZE: u32 = 100;
/// How often a playing song reports its position to the server.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq)]
pub enum AuthStatus {
    Starting,
    SignedOut,
    Connecting,
    Connected { username: String },
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteAction {
    Play,
    Pause,
    Next,
    Previous,
    Seek,
    Volume,
    Shuffle,
    Repeat,
}

/// Which of the two readers of the recently-played endpoint an answer
/// belongs to: the shelf on Home, or the Recents tab in the queue panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecentsFor {
    Home,
    Panel,
}

#[derive(Clone, Debug)]
pub enum ApiRequest {
    Me,
    Devices,
    PlaybackState {
        seq: u64,
    },
    Queue {
        seq: u64,
    },
    RecentlyPlayed {
        /// Request owner. Home and Recents use separate generation counters,
        /// so generation alone cannot route the response.
        who: RecentsFor,
        generation: u64,
        before: Option<String>,
        limit: u32,
    },
    TopTracks {
        offset: u32,
        full: bool,
        generation: u64,
    },
    TopArtists {
        generation: u64,
    },
    Recommendations {
        seed_tracks: Vec<String>,
        seed_artists: Vec<String>,
        generation: u64,
    },
    /// The albums added to the library most recently, for Home.
    LatestAlbums {
        generation: u64,
    },
    MyPlaylists {
        offset: u32,
        generation: u64,
    },
    Playlist {
        id: String,
        generation: u64,
    },
    PlaylistItems {
        id: String,
        offset: u32,
        generation: u64,
    },
    /// A slice of a playlist read only for who added its songs; the rows
    /// on screen stay untouched.
    PlaylistSample {
        id: String,
        offset: u32,
        generation: u64,
    },
    CreatePlaylist {
        name: String,
        public: bool,
        description: String,
    },
    UploadPlaylistCover {
        id: String,
        request: u64,
        previous_urls: Vec<String>,
        cover: crate::playlist_cover::Cover,
    },
    UpdatePlaylist {
        id: String,
        name: Option<String>,
        description: Option<String>,
        public: Option<bool>,
    },
    CheckPlaylistDuplicates {
        playlist_id: String,
        playlist_name: String,
        items: Vec<PlayableItem>,
        position: Option<u32>,
    },
    AddToPlaylist {
        playlist_id: String,
        playlist_name: String,
        uris: Vec<String>,
        position: Option<u32>,
    },
    RemoveFromPlaylist {
        playlist_id: String,
        uris: Vec<String>,
        snapshot_id: Option<String>,
    },
    ReorderPlaylist {
        playlist_id: String,
        range_start: u32,
        insert_before: u32,
        snapshot_id: Option<String>,
    },
    /// Delete a playlist from the server. Its songs stay in the library.
    DeletePlaylist {
        id: String,
    },
    SavedTracks {
        offset: u32,
        generation: u64,
    },
    SavedAlbums {
        offset: u32,
    },
    FollowedArtists {
        after: Option<String>,
    },
    SavedShows {
        offset: u32,
    },
    SavedEpisodes {
        offset: u32,
    },
    SetSaved {
        uris: Vec<String>,
        saved: bool,
    },
    Contains {
        uris: Vec<String>,
    },
    Search {
        query: String,
        serial: u64,
    },
    Artist {
        id: String,
    },
    ArtistTopTracks {
        id: String,
    },
    ArtistAlbums {
        id: String,
        groups: String,
        offset: u32,
    },
    RelatedArtists {
        id: String,
    },
    Album {
        id: String,
    },
    AlbumTracks {
        id: String,
        offset: u32,
        generation: u64,
    },
    AlbumQueueTracks {
        id: String,
        offset: u32,
        request: u64,
    },
    Show {
        id: String,
    },
    ShowEpisodes {
        id: String,
        offset: u32,
    },
    /// The newest episodes of a few saved podcasts, for Home. The shows
    /// are read one after another, not all at once.
    HomeEpisodes {
        shows: Vec<Show>,
        generation: u64,
    },
    Track {
        id: String,
    },
    /// One episode, asked for by a link to it: the podcast it belongs to
    /// is the page that opens.
    Episode {
        id: String,
    },
    Remote {
        action: RemoteAction,
        device_id: Option<String>,
        play: Option<PlayRequest>,
        position_ms: u32,
        percent: u8,
        flag: bool,
        repeat: String,
    },
    Transfer {
        device_id: String,
        play: bool,
    },
    /// Shuffle on, then start the context, one after the other: sent as two
    /// independent requests they race, and shuffle sometimes lost.
    ShufflePlay {
        device_id: Option<String>,
        play: PlayRequest,
    },
    AddToQueue {
        uri: String,
        device_id: Option<String>,
        label: String,
    },
    AddManyToQueue {
        request: u64,
        uris: Vec<String>,
        device_id: Option<String>,
    },
}

impl ApiRequest {
    fn background(&self) -> bool {
        matches!(
            self,
            Self::PlaybackState { .. }
                | Self::RecentlyPlayed { .. }
                | Self::TopTracks { .. }
                | Self::TopArtists { .. }
                | Self::Recommendations { .. }
                | Self::LatestAlbums { .. }
                | Self::MyPlaylists { .. }
                | Self::PlaylistSample { .. }
                | Self::Contains { .. }
        )
    }
}

#[derive(Debug)]
pub enum ApiResponse {
    Me(ApiResult<User>),
    Devices(ApiResult<Vec<Device>>),
    PlaybackState {
        seq: u64,
        result: ApiResult<Option<PlaybackState>>,
    },
    Queue {
        seq: u64,
        result: ApiResult<Queue>,
    },
    RecentlyPlayed {
        who: RecentsFor,
        generation: u64,
        limit: u32,
        result: ApiResult<CursorPage<PlayHistory>>,
    },
    TopTracks {
        offset: u32,
        full: bool,
        generation: u64,
        result: ApiResult<Page<Track>>,
    },
    TopArtists {
        generation: u64,
        result: ApiResult<Vec<Artist>>,
    },
    Recommendations {
        generation: u64,
        result: ApiResult<Vec<Track>>,
    },
    LatestAlbums {
        generation: u64,
        result: ApiResult<Vec<Album>>,
    },
    MyPlaylists {
        offset: u32,
        generation: u64,
        result: ApiResult<Page<Playlist>>,
    },
    Playlist {
        id: String,
        generation: u64,
        result: ApiResult<Playlist>,
    },
    PlaylistItems {
        id: String,
        offset: u32,
        generation: u64,
        result: ApiResult<Page<PlaylistItem>>,
    },
    PlaylistSample {
        id: String,
        generation: u64,
        result: ApiResult<Page<PlaylistItem>>,
    },
    PlaylistCreated(ApiResult<Playlist>),
    PlaylistCoverUploaded {
        id: String,
        request: u64,
        previous_urls: Vec<String>,
        cover: crate::playlist_cover::Cover,
        result: ApiResult<()>,
    },
    PlaylistUpdated {
        id: String,
        result: ApiResult<()>,
    },
    PlaylistDuplicatesChecked {
        playlist_id: String,
        playlist_name: String,
        items: Vec<PlayableItem>,
        position: Option<u32>,
        result: ApiResult<Vec<String>>,
    },
    PlaylistItemsChanged {
        id: String,
        message: String,
        result: ApiResult<Option<String>>,
    },
    PlaylistDeleted {
        id: String,
        result: ApiResult<()>,
    },
    SavedTracks {
        offset: u32,
        generation: u64,
        account_id: Option<String>,
        result: ApiResult<Page<SavedTrack>>,
    },
    SavedAlbums {
        offset: u32,
        result: ApiResult<Page<SavedAlbum>>,
    },
    FollowedArtists {
        after: Option<String>,
        result: ApiResult<CursorPage<Artist>>,
    },
    SavedShows {
        offset: u32,
        result: ApiResult<Page<SavedShow>>,
    },
    SavedEpisodes {
        offset: u32,
        result: ApiResult<Page<SavedEpisode>>,
    },
    SavedChanged {
        uris: Vec<String>,
        saved: bool,
        result: ApiResult<()>,
    },
    Contains {
        uris: Vec<String>,
        result: ApiResult<Vec<bool>>,
    },
    SearchStarted {
        query: String,
        serial: u64,
        split: bool,
    },
    Search {
        query: String,
        serial: u64,
        result: ApiResult<SearchResults>,
    },
    Artist {
        id: String,
        result: ApiResult<Artist>,
    },
    ArtistTopTracks {
        id: String,
        result: ApiResult<Vec<Track>>,
    },
    ArtistAlbums {
        id: String,
        groups: String,
        offset: u32,
        result: ApiResult<Page<Album>>,
    },
    RelatedArtists {
        id: String,
        result: ApiResult<Vec<Artist>>,
    },
    Album {
        id: String,
        result: ApiResult<Album>,
    },
    AlbumTracks {
        id: String,
        offset: u32,
        generation: u64,
        result: ApiResult<Page<Track>>,
    },
    AlbumQueueTracks {
        offset: u32,
        request: u64,
        result: ApiResult<Page<Track>>,
    },
    Show {
        id: String,
        result: ApiResult<Show>,
    },
    ShowEpisodes {
        id: String,
        offset: u32,
        result: ApiResult<Page<Episode>>,
    },
    /// Each show with its newest episodes, in the order asked for.
    HomeEpisodes {
        generation: u64,
        result: ApiResult<Vec<(Show, Vec<Episode>)>>,
    },
    Track {
        id: String,
        result: ApiResult<Track>,
    },
    Episode {
        id: String,
        result: ApiResult<Episode>,
    },
    Remote {
        action: RemoteAction,
        result: ApiResult<()>,
    },
    Transferred {
        device_id: String,
        result: ApiResult<()>,
    },
    QueueAdded {
        label: String,
        result: ApiResult<()>,
    },
    QueueBatchAdded {
        request: u64,
        added: usize,
        result: ApiResult<()>,
    },
}

pub enum PlaylistCacheRows {
    Replace(Vec<PlaylistItem>),
    Append {
        previous_rows: usize,
        previous_offset: u32,
        items: Vec<PlaylistItem>,
    },
}

struct PlaylistCacheWrite {
    path: std::path::PathBuf,
    account_id: String,
    id: String,
    generation: u64,
    snapshot: String,
    rows: PlaylistCacheRows,
    total: u32,
    next_offset: Option<u32>,
}

pub enum Command {
    OpenThemesFolder,
    ProxyRestored {
        lease: CredentialLease,
        result: Result<crate::credentials::Loaded, crate::credentials::Error>,
    },
    /// Internal: the stored sign-in was read back from the credential store.
    SessionRestored {
        lease: CredentialLease,
        result: Result<crate::credentials::Loaded, crate::credentials::Error>,
    },
    CheckPlaylistCover {
        id: String,
        request: u64,
        cover: crate::playlist_cover::Cover,
        images: Vec<crate::api::models::Image>,
    },
    ChoosePlaylistCover {
        id: String,
        request: u64,
        selected:
            std::pin::Pin<Box<dyn std::future::Future<Output = Option<rfd::FileHandle>> + Send>>,
    },
    /// Sign in to a server with what the form holds, through `config`.
    SignIn {
        request: u64,
        config: ProxyConfig,
        login: Box<Login>,
    },
    /// Give up on a sign-in that has not answered yet.
    CancelSignIn,
    SignOut,
    /// Internal: the server answered a sign-in.
    SignedIn {
        attempt: u64,
        result: ApiResult<Box<Session>>,
    },
    /// Reload the engine config (audio settings changed).
    RestartEngine(EngineConfig),
    /// Rebuild the HTTP client. The player fetches through it too.
    ApplyProxy {
        request: u64,
        config: ProxyConfig,
    },
    Player(PlayerCommand),
    Api(ApiRequest),
    ApiFinished {
        generation: u64,
        response: Box<ApiResponse>,
        /// The server rejected the session's token.
        expired: bool,
    },
    Accent {
        url: String,
    },
    Shutdown,
    /// Ask GitHub whether a newer release exists. Manual checks report every
    /// outcome; the daily check only announces a new release.
    CheckForUpdates {
        manual: bool,
        source: crate::updates::Source,
    },
    InspectUpdate,
    DownloadUpdate {
        release: crate::updates::Release,
        source: crate::updates::Source,
    },
    InstallUpdate {
        prepared: Box<crate::updates::Prepared>,
        arguments: Vec<String>,
    },
    /// The words of a track, from the server or LRCLIB.
    Lyrics(Box<LyricsRequest>),
    /// Read a playlist's cached items from disk.
    LoadPlaylistCache {
        id: String,
        generation: u64,
    },
    /// Remember a playlist prefix on disk under its snapshot.
    StorePlaylistCache {
        id: String,
        generation: u64,
        snapshot: String,
        rows: PlaylistCacheRows,
        total: u32,
        next_offset: Option<u32>,
    },
    LoadLikedSongsCache {
        generation: u64,
    },
    StoreLikedSongsCache(crate::liked::Cache),
    /// The server's mix seeded by `seed`: a song, album, artist or playlist.
    Radio {
        seed: String,
        generation: u64,
    },
}

pub struct LyricsRequest {
    /// The track the answer is for, so a stale one is ignored.
    pub uri: String,
    pub query: crate::lyrics::Query,
}

pub enum Event {
    ProxyRestored {
        config: ProxyConfig,
        password: Option<crate::credentials::ProxyPassword>,
    },
    ProxyPasswordStored,
    ProxyStorageFailed(crate::credentials::Error),
    ProxyApplied {
        request: u64,
        config: ProxyConfig,
        result: Result<bool, String>,
    },
    UpdateSupport(Result<crate::updates::Installation, String>),
    UpdateProgress {
        received: u64,
        total: u64,
    },
    UpdateDownloaded(Result<Box<crate::updates::Prepared>, String>),
    UpdateInstalling(Result<(), String>),
    PlaylistCoverChecked {
        id: String,
        request: u64,
        images: Vec<crate::api::models::Image>,
        result: Result<bool, String>,
    },
    PlaylistCoverChosen {
        id: String,
        request: u64,
        result: Result<Option<crate::playlist_cover::Cover>, String>,
    },
    Auth(AuthStatus),
    /// The server a session belongs to, and the account on it.
    Server {
        address: String,
        username: String,
    },
    Playback(LocalPlayback),
    Local(Box<LocalState>),
    Api(Box<ApiResponse>),
    Accent {
        url: String,
        color: [u8; 3],
    },
    Error(String),
    /// GitHub answered an update check, or the request failed.
    UpdateChecked {
        manual: bool,
        result: Result<Option<crate::updates::Release>, String>,
    },
    /// Track lyrics, or `None` when unavailable.
    Lyrics {
        uri: String,
        result: Result<Option<crate::lyrics::Lyrics>, String>,
    },
    /// The result of reading a playlist cache for this load generation.
    PlaylistCache {
        account_id: String,
        id: String,
        generation: u64,
        cache: Option<PlaylistCache>,
    },
    PlaylistCacheStored {
        account_id: String,
        id: String,
        generation: u64,
        snapshot: String,
        success: bool,
    },
    /// The songs of the radio seeded by `seed`, for the request `generation`.
    Radio {
        seed: String,
        generation: u64,
        result: Result<Vec<crate::api::models::Track>, String>,
    },
    LikedSongsCache {
        account_id: String,
        generation: u64,
        cache: Option<crate::liked::Cache>,
    },
}

/// The state of playback on this computer.
#[derive(Clone, Debug, PartialEq)]
pub enum LocalPlayback {
    /// Signed out: there is no server to play from.
    Unavailable,
    /// The player is up and takes songs.
    Ready {
        device_id: String,
    },
    Failed(String),
}

/// Background services (the runtime, MPRIS, the tray) outlive individual
/// windows: the window is destroyed when it closes to the tray and created
/// again on demand. They therefore hold this handle, which repaints
/// whichever window exists, instead of an `egui::Context`.
pub use fastframe_shell::Waker;

/// The interface's handle to the runtime.
pub struct Backend {
    commands: mpsc::UnboundedSender<Command>,
    events: std::sync::mpsc::Receiver<Event>,
    art: ArtLoader,
    activity: Arc<NetActivity>,
    thread: Option<std::thread::JoinHandle<()>>,
    offline: bool,
    #[cfg(test)]
    playlist_item_requests: std::sync::Mutex<Vec<(String, u32, u64)>>,
    #[cfg(test)]
    playlist_sample_requests: std::sync::Mutex<Vec<(String, u32, u64)>>,
    #[cfg(test)]
    playlist_add_requests: std::sync::Mutex<Vec<ApiRequest>>,
    #[cfg(test)]
    remote_play_requests: std::sync::Mutex<Vec<ApiRequest>>,
    #[cfg(test)]
    remote_shuffle_requests: std::sync::Mutex<Vec<ApiRequest>>,
    #[cfg(test)]
    queue_requests: std::sync::Mutex<Vec<ApiRequest>>,
    #[cfg(test)]
    queued_tracks: std::sync::Mutex<Vec<String>>,
    #[cfg(test)]
    player_commands: std::sync::Mutex<Vec<PlayerCommand>>,
}

impl Backend {
    pub fn spawn(
        dirs: AppDirs,
        engine_config: EngineConfig,
        waker: Waker,
        restore_sign_in: bool,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("jellifast-runtime")
            .enable_all()
            .build()
            .expect("unable to start the async runtime");
        let http = if restore_sign_in {
            Http::unavailable("Restoring proxy settings".into())
        } else {
            Http::from_proxy(&engine_config.proxy).unwrap_or_else(|error| {
                let _ = event_tx.send(Event::Error(format!(
                    "Network configuration failed: {error}"
                )));
                Http::unavailable(error)
            })
        };
        let art = ArtLoader::new(http.clone(), runtime.handle().clone(), dirs.art_cache_dir());
        let activity = Arc::new(NetActivity::default());

        let worker_activity = Arc::clone(&activity);
        let worker_art = art.clone();
        let worker_commands = command_tx.clone();
        let thread = std::thread::Builder::new()
            .name("jellifast-backend".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    let mut worker = Worker::new(
                        dirs,
                        engine_config,
                        http,
                        worker_art,
                        worker_activity,
                        event_tx,
                        worker_commands,
                        waker,
                    );
                    if restore_sign_in {
                        worker.restore_session();
                    }
                    worker.run(command_rx).await;
                });
                // Give the player's thread a moment to release the audio device.
                runtime.shutdown_timeout(Duration::from_secs(2));
            })
            .expect("unable to start the backend thread");

        Self {
            commands: command_tx,
            events: event_rx,
            art,
            activity,
            thread: Some(thread),
            offline: false,
            #[cfg(test)]
            playlist_item_requests: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            playlist_sample_requests: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            playlist_add_requests: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            remote_play_requests: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            remote_shuffle_requests: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            queue_requests: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            queued_tracks: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            player_commands: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Live network activity, for the interface's busy indicator.
    pub fn activity(&self) -> &NetActivity {
        &self.activity
    }

    /// Stops server-bound commands from leaving the process; artwork and
    /// shutdown still work. Used by the demo mode and by headless tests.
    #[cfg_attr(not(any(test, feature = "demo")), allow(dead_code))]
    pub fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
    }

    pub fn send(&self, command: Command) {
        if self.offline
            && !matches!(
                command,
                Command::Accent { .. }
                    | Command::Shutdown
                    | Command::CheckForUpdates { .. }
                    | Command::InspectUpdate
                    | Command::DownloadUpdate { .. }
                    | Command::InstallUpdate { .. }
            )
        {
            return;
        }
        let _ = self.commands.send(command);
    }

    /// Construct the native dialog on the UI thread, then await and read it
    /// on the runtime. AppKit requires its window lookup on the main thread.
    pub fn choose_playlist_cover(&self, id: String, request: u64) {
        if self.offline {
            return;
        }
        let selected = rfd::AsyncFileDialog::new()
            .set_title("Choose playlist cover")
            .add_filter("JPEG or PNG image", &["jpg", "jpeg", "png"])
            .pick_file();
        self.send(Command::ChoosePlaylistCover {
            id,
            request,
            selected: Box::pin(selected),
        });
    }

    pub fn api(&self, request: ApiRequest) {
        #[cfg(test)]
        if matches!(
            request,
            ApiRequest::AlbumQueueTracks { .. }
                | ApiRequest::AddToQueue { .. }
                | ApiRequest::AddManyToQueue { .. }
        ) {
            self.queue_requests.lock().unwrap().push(request.clone());
        }
        #[cfg(test)]
        if matches!(
            request,
            ApiRequest::Remote {
                action: RemoteAction::Play,
                ..
            } | ApiRequest::ShufflePlay { .. }
        ) {
            self.remote_play_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request.clone());
        }
        #[cfg(test)]
        if matches!(
            request,
            ApiRequest::Remote {
                action: RemoteAction::Shuffle,
                ..
            }
        ) {
            self.remote_shuffle_requests
                .lock()
                .unwrap()
                .push(request.clone());
        }
        #[cfg(test)]
        if matches!(
            request,
            ApiRequest::AddToPlaylist { .. }
                | ApiRequest::CheckPlaylistDuplicates { .. }
                | ApiRequest::UpdatePlaylist { .. }
        ) {
            self.playlist_add_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request.clone());
        }
        #[cfg(test)]
        if let ApiRequest::PlaylistItems {
            id,
            offset,
            generation,
        } = &request
        {
            self.playlist_item_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((id.clone(), *offset, *generation));
        }
        #[cfg(test)]
        if let ApiRequest::PlaylistSample {
            id,
            offset,
            generation,
        } = &request
        {
            self.playlist_sample_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((id.clone(), *offset, *generation));
        }
        self.send(Command::Api(request));
    }

    #[cfg(test)]
    pub fn take_playlist_item_requests(&self) -> Vec<(String, u32, u64)> {
        std::mem::take(
            &mut *self
                .playlist_item_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    #[cfg(test)]
    pub fn take_playlist_sample_requests(&self) -> Vec<(String, u32, u64)> {
        std::mem::take(
            &mut *self
                .playlist_sample_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    #[cfg(test)]
    pub fn take_playlist_add_requests(&self) -> Vec<ApiRequest> {
        std::mem::take(
            &mut *self
                .playlist_add_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    #[cfg(test)]
    pub fn take_remote_play_requests(&self) -> Vec<ApiRequest> {
        std::mem::take(
            &mut *self
                .remote_play_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    #[cfg(test)]
    pub fn take_remote_shuffle_requests(&self) -> Vec<ApiRequest> {
        std::mem::take(&mut *self.remote_shuffle_requests.lock().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn take_queue_requests(&self) -> Vec<ApiRequest> {
        std::mem::take(&mut *self.queue_requests.lock().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn take_queued_tracks(&self) -> Vec<String> {
        std::mem::take(&mut *self.queued_tracks.lock().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn take_player_commands(&self) -> Vec<PlayerCommand> {
        std::mem::take(&mut *self.player_commands.lock().unwrap())
    }

    pub fn player(&self, command: PlayerCommand) {
        #[cfg(test)]
        self.player_commands.lock().unwrap().push(command.clone());
        #[cfg(test)]
        if let PlayerCommand::AddToQueue(uri) = &command {
            self.queued_tracks.lock().unwrap().push(uri.clone());
        }
        self.send(Command::Player(command));
    }

    pub fn poll(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    pub fn art(&self) -> &ArtLoader {
        &self.art
    }

    pub fn shutdown(&mut self) {
        self.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One step for the player, taken in the order the interface asked. A load
/// names songs by URI and has to ask the server about them first; a command
/// sent after it must still reach the engine after it.
struct PlayerJob {
    command: PlayerCommand,
}

struct Worker {
    dirs: AppDirs,
    credentials: CredentialStore,
    restoring_proxy: bool,
    waiting_for_proxy: VecDeque<Command>,
    proxy_revision: u64,
    /// Counts sign-ins and sign-outs, so work started for one session cannot
    /// land in the next.
    session: watch::Sender<u64>,
    sign_in_attempt: u64,
    engine_config: EngineConfig,
    http: Http,
    client: Option<Arc<ApiClient>>,
    background_api: Arc<tokio::sync::Semaphore>,
    art: ArtLoader,
    activity: Arc<NetActivity>,
    events: std::sync::mpsc::Sender<Event>,
    commands: mpsc::UnboundedSender<Command>,
    waker: Waker,
    engine: Option<Arc<Engine>>,
    /// The ordered queue of player commands for the current engine.
    player: Option<mpsc::UnboundedSender<PlayerJob>>,
    /// What the engine is heard at, so the engine that replaces it starts
    /// there. `engine_config` alone knows only the level the app launched
    /// with.
    heard: Option<Heard>,
    search_tasks: Vec<tokio::task::AbortHandle>,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        dirs: AppDirs,
        engine_config: EngineConfig,
        http: Http,
        art: ArtLoader,
        activity: Arc<NetActivity>,
        events: std::sync::mpsc::Sender<Event>,
        commands: mpsc::UnboundedSender<Command>,
        waker: Waker,
    ) -> Self {
        Self {
            #[cfg(not(test))]
            credentials: CredentialStore::new(dirs.clone()),
            #[cfg(test)]
            credentials: CredentialStore::in_memory(dirs.clone()),
            restoring_proxy: false,
            waiting_for_proxy: VecDeque::new(),
            proxy_revision: 0,
            session: watch::channel(0).0,
            sign_in_attempt: 0,
            dirs,
            engine_config,
            http,
            client: None,
            background_api: Arc::new(tokio::sync::Semaphore::new(4)),
            art,
            activity,
            events,
            commands,
            waker,
            engine: None,
            player: None,
            heard: None,
            search_tasks: Vec::new(),
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.waker.wake();
    }

    fn account_id(&self) -> Option<String> {
        self.client
            .as_ref()
            .map(|client| client.user_id().to_string())
    }

    // ---- proxy ----------------------------------------------------------------

    /// Build before replacing the transport. A rejected change leaves the
    /// existing connection in place and is reported to the UI.
    fn apply_proxy(&mut self, proxy: ProxyConfig) -> Result<bool, String> {
        let client = crate::http::build_client(&proxy)?;
        self.http.replace(client);
        self.engine_config.proxy = proxy;
        // The player fetches through the same client: nothing restarts.
        Ok(false)
    }

    fn change_proxy(&mut self, request: u64, proxy: ProxyConfig) -> bool {
        let result = self.apply_proxy(proxy.clone());
        let applied = result.is_ok();
        if applied {
            self.proxy_revision = request;
        }
        self.emit(Event::ProxyApplied {
            request,
            config: proxy.clone(),
            result,
        });
        if applied {
            self.persist_proxy_password(&proxy);
            self.finish_proxy_restore();
        }
        applied
    }

    fn persist_proxy_password(&mut self, proxy: &ProxyConfig) {
        if !matches!(proxy, ProxyConfig::Http(_) | ProxyConfig::Socks(_)) {
            // Off/System retain the saved manual password for later use.
            return;
        }
        let events = self.events.clone();
        let waker = self.waker.clone();
        let pending = if let Some(password) = proxy.password_record() {
            self.credentials.invalidate(CredentialSlot::Proxy);
            let lease = self.credentials.lease(CredentialSlot::Proxy);
            let saving = lease.save(StoredGrant::Proxy(password));
            (lease, saving, true)
        } else {
            if let Err(error) = self.credentials.revoke(CredentialSlot::Proxy) {
                self.emit(Event::ProxyStorageFailed(error));
                return;
            }
            // The durable revocation marker already prevents a failed delete
            // from restoring an old password. The UI can now scrub legacy JSON.
            self.emit(Event::ProxyPasswordStored);
            let lease = self.credentials.lease(CredentialSlot::Proxy);
            let deleting = lease.delete();
            (lease, deleting, false)
        };
        tokio::spawn(async move {
            let (lease, operation, saving) = pending;
            let result = operation.await;
            if !lease.current() {
                return;
            }
            match result {
                Ok(()) if saving => {
                    let _ = events.send(Event::ProxyPasswordStored);
                }
                Ok(()) => {}
                Err(error) if error != crate::credentials::Error::Stale => {
                    let _ = events.send(Event::ProxyStorageFailed(error));
                }
                Err(_) => {}
            }
            waker.wake();
        });
    }

    fn on_proxy_restored(
        &mut self,
        lease: CredentialLease,
        result: Result<crate::credentials::Loaded, crate::credentials::Error>,
    ) {
        if !lease.current() {
            return;
        }
        let (password, protected) = match result {
            Ok(loaded) => {
                if let Some(error) = loaded.warning {
                    self.emit(Event::ProxyStorageFailed(error));
                }
                let password = match loaded.grant {
                    Some(StoredGrant::Proxy(password)) => Some(password),
                    _ => None,
                };
                (password, loaded.warning.is_none())
            }
            Err(error) => {
                self.emit(Event::ProxyStorageFailed(error));
                (None, false)
            }
        };
        if self.proxy_revision == 0 {
            let mut config = self.engine_config.proxy.clone();
            if let Some(password) = &password {
                config.restore_password(password);
            }
            if let Err(error) = self.apply_proxy(config.clone()) {
                self.http.block(error.clone());
                config = ProxyConfig::Invalid(error.clone());
                self.engine_config.proxy = config.clone();
                self.emit(Event::Error(format!(
                    "Network configuration failed: {error}"
                )));
            }
            self.emit(Event::ProxyRestored { config, password });
        }
        if protected {
            self.emit(Event::ProxyPasswordStored);
        }
        self.finish_proxy_restore();
    }

    fn finish_proxy_restore(&mut self) {
        if !std::mem::take(&mut self.restoring_proxy) {
            return;
        }
        self.restore_stored_session();
        for command in self.waiting_for_proxy.drain(..) {
            let _ = self.commands.send(command);
        }
    }

    // ---- the command loop ---------------------------------------------------------

    async fn run(&mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        let (cache_writes, cache_write_receiver) = mpsc::channel(1);
        let cache_writer = tokio::spawn(store_playlist_caches(
            cache_write_receiver,
            self.events.clone(),
            self.waker.clone(),
        ));
        while let Some(command) = commands.recv().await {
            if self.restoring_proxy
                && !matches!(
                    &command,
                    Command::ProxyRestored { .. }
                        | Command::ApplyProxy { .. }
                        | Command::SignIn { .. }
                        | Command::SignOut
                        | Command::CancelSignIn
                        | Command::Shutdown
                )
            {
                self.waiting_for_proxy.push_back(command);
                continue;
            }
            match command {
                Command::OpenThemesFolder => {
                    let directory = self.dirs.config.join("themes");
                    let events = self.events.clone();
                    let waker = self.waker.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(error) = std::fs::create_dir_all(&directory)
                            .and_then(|()| crate::opener::open(&directory))
                        {
                            let _ = events.send(Event::Error(format!(
                                "Couldn't open the themes folder: {error}"
                            )));
                            waker.wake();
                        }
                    });
                }
                Command::ProxyRestored { lease, result } => self.on_proxy_restored(lease, result),
                Command::SessionRestored { lease, result } => {
                    self.on_session_restored(lease, result)
                }
                Command::CheckPlaylistCover {
                    id,
                    request,
                    cover,
                    images,
                } => {
                    let art = self.art.clone();
                    let events = self.events.clone();
                    let waker = self.waker.clone();
                    let mut session = self.session.subscribe();
                    tokio::spawn(async move {
                        let result = tokio::select! {
                            _ = session.changed() => return,
                            result = async {
                                let url = crate::api::models::pick_image(&images, u32::MAX)
                                    .ok_or_else(|| "No playlist artwork yet.".to_string())?;
                                let bytes = art.fetch(url).await?;
                                tokio::task::spawn_blocking(move || cover.matches_remote(&bytes))
                                    .await.map_err(|_| "Couldn't check playlist artwork.".to_string())?
                            } => result,
                        };
                        let _ = events.send(Event::PlaylistCoverChecked {
                            id,
                            request,
                            images,
                            result,
                        });
                        waker.wake();
                    });
                }
                Command::ChoosePlaylistCover {
                    id,
                    request,
                    selected,
                } => {
                    let events = self.events.clone();
                    let waker = self.waker.clone();
                    tokio::spawn(async move {
                        let selected = selected.await;
                        let result = match selected {
                            None => Ok(None),
                            Some(file) => tokio::task::spawn_blocking(move || {
                                crate::playlist_cover::read(file.path()).map(Some)
                            })
                            .await
                            .unwrap_or_else(|_| {
                                Err("Couldn't prepare that image. Try another file.".into())
                            }),
                        };
                        let _ = events.send(Event::PlaylistCoverChosen {
                            id,
                            request,
                            result,
                        });
                        waker.wake();
                    });
                }
                Command::Shutdown => break,
                Command::SignIn {
                    request,
                    config,
                    login,
                } => {
                    if self.change_proxy(request, config) {
                        self.sign_in(*login);
                    }
                }
                Command::CancelSignIn => {
                    self.sign_in_attempt += 1;
                    if self.client.is_none() {
                        self.emit(Event::Auth(AuthStatus::SignedOut));
                    }
                }
                Command::SignedIn { attempt, result } => {
                    if attempt != self.sign_in_attempt {
                        continue;
                    }
                    match result {
                        Ok(session) => self.install_session(*session, true),
                        Err(error) => self.emit(Event::Auth(AuthStatus::Failed(error.to_string()))),
                    }
                }
                Command::SignOut => self.sign_out(),
                Command::RestartEngine(mut config) => {
                    // Audio settings must not revert a proxy change whose UI
                    // acknowledgement was still in flight when this was clicked.
                    config.proxy = self.engine_config.proxy.clone();
                    self.engine_config = config;
                    self.replace_engine();
                }
                Command::ApplyProxy { request, config } => {
                    self.change_proxy(request, config);
                }
                Command::Player(command) => match &self.player {
                    Some(player) => {
                        let _ = player.send(PlayerJob { command });
                    }
                    None => self.emit(Event::Error(
                        "Sign in to play music on this computer".into(),
                    )),
                },
                Command::Api(ApiRequest::Search { query, serial }) => self.search(query, serial),
                Command::Api(request) => {
                    self.dispatch(request);
                }
                Command::ApiFinished {
                    generation,
                    response,
                    expired,
                } => {
                    if generation != *self.session.borrow() {
                        continue;
                    }
                    if expired {
                        // The server no longer knows this token: the stored
                        // copy is useless and the form is the way back in.
                        self.forget_session();
                        self.emit(Event::Auth(AuthStatus::Failed(
                            "Your sign-in expired. Please sign in again.".into(),
                        )));
                        continue;
                    }
                    self.emit(Event::Api(response));
                }
                Command::Accent { url } => self.accent(url),
                Command::CheckForUpdates { manual, source } => {
                    self.check_for_updates(manual, source)
                }
                Command::InspectUpdate => {
                    let proxy = self.engine_config.proxy.clone();
                    let events = self.events.clone();
                    let waker = self.waker.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = crate::updates::updater(&proxy)
                            .map_err(|error| format!("{error:#}"))
                            .and_then(|updater| {
                                updater.installation().map_err(|error| error.to_string())
                            });
                        let _ = events.send(Event::UpdateSupport(result));
                        waker.wake();
                    });
                }
                Command::DownloadUpdate { release, source } => {
                    let proxy = self.engine_config.proxy.clone();
                    let events = self.events.clone();
                    let waker = self.waker.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = crate::updates::updater(&proxy)
                            .and_then(|updater| {
                                updater
                                    .with_source(source)
                                    .download(&release, |received, total| {
                                        let _ =
                                            events.send(Event::UpdateProgress { received, total });
                                        waker.wake();
                                    })
                            })
                            .map(Box::new)
                            .map_err(|error| format!("{error:#}"));
                        let _ = events.send(Event::UpdateDownloaded(result));
                        waker.wake();
                    });
                }
                Command::InstallUpdate {
                    prepared,
                    arguments,
                } => {
                    let proxy = self.engine_config.proxy.clone();
                    let events = self.events.clone();
                    let waker = self.waker.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = crate::updates::updater(&proxy)
                            .and_then(|updater| updater.handoff(*prepared, arguments))
                            .map_err(|error| format!("{error:#}"));
                        let _ = events.send(Event::UpdateInstalling(result));
                        waker.wake();
                    });
                }
                Command::Lyrics(request) => self.fetch_lyrics(*request),
                Command::LoadPlaylistCache { id, generation } => {
                    self.load_playlist_cache(id, generation)
                }
                Command::StorePlaylistCache {
                    id,
                    generation,
                    snapshot,
                    rows,
                    total,
                    next_offset,
                } => {
                    if let Some(account_id) = self.account_id() {
                        let path = self
                            .dirs
                            .account_playlist_cache_dir(&account_id)
                            .join(format!("{id}.json"));
                        if let Err(error) = cache_writes.try_send(PlaylistCacheWrite {
                            path,
                            account_id,
                            id,
                            generation,
                            snapshot,
                            rows,
                            total,
                            next_offset,
                        }) {
                            let write = error.into_inner();
                            log::warn!(
                                "unable to queue playlist cache {}: writer unavailable",
                                write.path.display()
                            );
                            self.emit(Event::PlaylistCacheStored {
                                account_id: write.account_id,
                                id: write.id,
                                generation: write.generation,
                                snapshot: write.snapshot,
                                success: false,
                            });
                        }
                    } else {
                        self.emit(Event::PlaylistCacheStored {
                            account_id: String::new(),
                            id,
                            generation,
                            snapshot,
                            success: false,
                        });
                    }
                }
                Command::LoadLikedSongsCache { generation } => {
                    if let Some(account_id) = self.account_id() {
                        let path = self.dirs.liked_songs_cache_file(&account_id);
                        let events = self.events.clone();
                        let waker = self.waker.clone();
                        tokio::spawn(async move {
                            let cache = crate::liked::read(&path, &account_id).await;
                            let _ = events.send(Event::LikedSongsCache {
                                account_id,
                                generation,
                                cache,
                            });
                            waker.wake();
                        });
                    }
                }
                Command::StoreLikedSongsCache(cache) => {
                    if self.account_id().as_deref() == Some(cache.account_id.as_str()) {
                        let path = self.dirs.liked_songs_cache_file(&cache.account_id);
                        if let Err(error) = crate::liked::write(&path, &cache).await {
                            log::warn!("unable to store the favourites cache: {error}");
                        }
                    }
                }
                Command::Radio { seed, generation } => self.radio(seed, generation),
            }
        }
        self.retire_engine();
        drop(cache_writes);
        let _ = cache_writer.await;
    }

    // ---- sign-in --------------------------------------------------------------

    /// The id this installation shows the server. It is made once and kept,
    /// so the server's device list has one entry per computer rather than one
    /// per sign-in.
    fn device_id(&self) -> String {
        let path = self.dirs.state.join("device-id");
        if let Ok(id) = std::fs::read_to_string(&path) {
            let id = id.trim();
            if id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return id.to_string();
            }
        }
        let id = crate::auth::new_device_id();
        if let Err(error) =
            std::fs::create_dir_all(&self.dirs.state).and_then(|()| std::fs::write(&path, &id))
        {
            log::warn!("unable to remember this device's id: {error}");
        }
        id
    }

    fn restore_session(&mut self) {
        self.restoring_proxy = true;
        let lease = self.credentials.lease(CredentialSlot::Proxy);
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let result = lease.load().await;
            let _ = commands.send(Command::ProxyRestored { lease, result });
        });
    }

    fn restore_stored_session(&mut self) {
        self.emit(Event::Auth(AuthStatus::Starting));
        let lease = self.credentials.lease(CredentialSlot::Session);
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let result = lease.load().await;
            let _ = commands.send(Command::SessionRestored { lease, result });
        });
    }

    fn on_session_restored(
        &mut self,
        lease: CredentialLease,
        result: Result<crate::credentials::Loaded, crate::credentials::Error>,
    ) {
        if !lease.current() || self.client.is_some() {
            return;
        }
        match result {
            Ok(loaded) => {
                if let Some(error) = loaded.warning {
                    self.emit(Event::Error(error.to_string()));
                }
                match loaded.grant {
                    Some(StoredGrant::Session(session)) => self.install_session(session, false),
                    _ => self.emit(Event::Auth(AuthStatus::SignedOut)),
                }
            }
            Err(error) => {
                self.emit(Event::Error(error.to_string()));
                self.emit(Event::Auth(AuthStatus::SignedOut));
            }
        }
    }

    fn sign_in(&mut self, login: Login) {
        self.sign_in_attempt += 1;
        let attempt = self.sign_in_attempt;
        self.emit(Event::Auth(AuthStatus::Connecting));
        let http = self.http.client();
        let device_id = self.device_id();
        let device_name = self.engine_config.device_name.clone();
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let result = match http {
                Ok(http) => crate::auth::sign_in(&http, &login, &device_id, &device_name).await,
                Err(error) => Err(ApiError::Network(error)),
            };
            let _ = commands.send(Command::SignedIn {
                attempt,
                result: result.map(Box::new),
            });
        });
    }

    /// Makes `session` the one every request and the player use. A session
    /// from the form is also remembered; one read back from the store is
    /// already there.
    fn install_session(&mut self, session: Session, remember: bool) {
        self.retire_engine();
        self.cancel_search();
        self.session.send_modify(|generation| *generation += 1);
        if remember {
            self.credentials.invalidate(CredentialSlot::Session);
            let lease = self.credentials.lease(CredentialSlot::Session);
            let saving = lease.save(StoredGrant::Session(session.clone()));
            let events = self.events.clone();
            let waker = self.waker.clone();
            tokio::spawn(async move {
                if let Err(error) = saving.await
                    && lease.current()
                    && error != crate::credentials::Error::Stale
                {
                    // Signed in, but the next launch will ask again.
                    let _ = events.send(Event::Error(error.to_string()));
                    waker.wake();
                }
            });
        }
        let username = session.username.clone();
        self.emit(Event::Server {
            address: session.server.clone(),
            username: username.clone(),
        });
        self.client = Some(Arc::new(ApiClient::new(
            self.http.clone(),
            Arc::clone(&self.activity),
            session,
            self.engine_config.device_name.clone(),
        )));
        self.emit(Event::Auth(AuthStatus::Connected { username }));
        self.start_engine(None);
    }

    /// Drops the session locally: no client, no player, nothing stored.
    fn forget_session(&mut self) {
        self.sign_in_attempt += 1;
        self.session.send_modify(|generation| *generation += 1);
        self.cancel_search();
        self.retire_engine();
        self.client = None;
        if let Err(error) = self.credentials.revoke_session() {
            self.emit(Event::Error(error.to_string()));
        }
        let lease = self.credentials.lease(CredentialSlot::Session);
        let deleting = lease.delete();
        tokio::spawn(async move {
            if let Err(error) = deleting.await {
                log::warn!("unable to delete the stored sign-in: {error}");
            }
        });
        self.emit(Event::Playback(LocalPlayback::Unavailable));
    }

    fn sign_out(&mut self) {
        if let Some(client) = self.client.clone()
            && let Ok(http) = self.http.client()
        {
            let device_name = self.engine_config.device_name.clone();
            tokio::spawn(async move {
                crate::auth::sign_out(&http, client.session(), &device_name).await;
            });
        }
        self.forget_session();
        self.emit(Event::Auth(AuthStatus::SignedOut));
    }

    // ---- the player --------------------------------------------------------------

    fn engine_notify(&self, reports: mpsc::UnboundedSender<LocalState>) -> crate::player::Notify {
        let events = self.events.clone();
        let waker = self.waker.clone();
        Arc::new(move |event| {
            if let EngineEvent::State(state) = event {
                let _ = reports.send(state.clone());
                let _ = events.send(Event::Local(Box::new(state)));
                waker.wake();
            }
        })
    }

    fn start_engine(&mut self, resume: Option<PlaybackResume>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if let Some(heard) = &self.heard {
            self.engine_config.initial_volume = heard.level();
        }
        let (reports, report_receiver) = mpsc::unbounded_channel();
        let engine = match Engine::start(
            &self.engine_config,
            Arc::clone(&client),
            self.http.clone(),
            tokio::runtime::Handle::current(),
            self.engine_notify(reports),
        ) {
            Ok(engine) => Arc::new(engine),
            Err(error) => {
                self.emit(Event::Playback(LocalPlayback::Failed(format!("{error:#}"))));
                return;
            }
        };
        tokio::spawn(report_playback(Arc::clone(&client), report_receiver));
        let (player, jobs) = mpsc::unbounded_channel();
        tokio::spawn(run_player(
            Arc::clone(&engine),
            client,
            jobs,
            self.events.clone(),
            self.waker.clone(),
        ));
        self.heard = Some(engine.heard());
        self.emit(Event::Playback(LocalPlayback::Ready {
            device_id: engine.device_id().to_string(),
        }));
        if let Some(resume) = resume
            && let Err(error) = engine.resume(resume)
        {
            log::warn!("unable to pick playback up again: {error:#}");
        }
        self.engine = Some(engine);
        self.player = Some(player);
    }

    fn retire_engine(&mut self) -> Option<PlaybackResume> {
        self.player = None;
        let engine = self.engine.take()?;
        let resume = engine.resume_point();
        // Joining the player thread waits for the output to play out.
        tokio::task::spawn_blocking(move || engine.shutdown());
        resume
    }

    /// An audio setting changed: a new engine takes over what the old one
    /// was playing.
    fn replace_engine(&mut self) {
        if self.client.is_none() {
            return;
        }
        let resume = self.retire_engine();
        self.start_engine(resume);
    }

    fn check_for_updates(&self, manual: bool, source: crate::updates::Source) {
        // A proxy still being restored or refused blocks the check, as it
        // blocks every other request.
        let usable = self.http.client().map(|_| ());
        let proxy = self.engine_config.proxy.clone();
        let events = self.events.clone();
        let waker = self.waker.clone();
        tokio::task::spawn_blocking(move || {
            let result = usable.and_then(|()| {
                crate::updates::updater(&proxy)
                    .and_then(|updater| updater.with_source(source).check())
                    .map_err(|error| format!("{error:#}"))
            });
            let _ = events.send(Event::UpdateChecked { manual, result });
            waker.wake();
        });
    }

    /// The server's mix seeded by a song, album, artist or playlist.
    fn radio(&self, seed: String, generation: u64) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let events = self.events.clone();
        let waker = self.waker.clone();
        let mut session = self.session.subscribe();
        session.borrow_and_update();
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = session.changed() => return,
                result = async {
                    let id = crate::util::uri_id(&seed).unwrap_or_default().to_string();
                    client.instant_mix(&id, RADIO_SIZE).await.map_err(|error| error.to_string())
                } => result,
            };
            let _ = events.send(Event::Radio {
                seed,
                generation,
                result,
            });
            waker.wake();
        });
    }

    fn fetch_lyrics(&self, request: LyricsRequest) {
        let http = self.http.client();
        let events = self.events.clone();
        let waker = self.waker.clone();
        let cache_dir = self.dirs.lyrics_cache_dir();
        let client = self.client.clone();
        tokio::spawn(async move {
            // The server's own words go first: they belong to the file that
            // is playing. LRCLIB is asked only when the server has none.
            let own = match (&client, crate::util::uri_id(&request.uri)) {
                (Some(client), Some(id)) => client.lyrics(id).await.ok().flatten(),
                _ => None,
            };
            let result = match own {
                Some(found) => Ok(Some(found)),
                None => match http {
                    Ok(http) => crate::lyrics::fetch(&http, &cache_dir, &request.query)
                        .await
                        .map_err(|error| format!("{error:#}")),
                    Err(error) => Err(error),
                },
            };
            let _ = events.send(Event::Lyrics {
                uri: request.uri,
                result,
            });
            waker.wake();
        });
    }

    /// Loads cached playlist items. The UI compares the cached snapshot with
    /// the live playlist before using them.
    fn load_playlist_cache(&self, id: String, generation: u64) {
        let Some(account_id) = self.account_id() else {
            return;
        };
        let events = self.events.clone();
        let waker = self.waker.clone();
        let path = self
            .dirs
            .account_playlist_cache_dir(&account_id)
            .join(format!("{id}.json"));
        tokio::spawn(async move {
            let cache = read_playlist_cache(path)
                .await
                .ok()
                .and_then(|(cached, appendable)| {
                    let total = cached
                        .total
                        .unwrap_or_else(|| cached.items.len().try_into().unwrap_or(u32::MAX));
                    if cached.items.len() > total as usize
                        || cached.next_offset.is_some_and(|offset| offset > total)
                    {
                        return None;
                    }
                    Some(PlaylistCache {
                        snapshot: cached.snapshot,
                        items: cached.items,
                        total,
                        next_offset: cached.next_offset,
                        appendable,
                    })
                });
            let _ = events.send(Event::PlaylistCache {
                account_id,
                id,
                generation,
                cache,
            });
            waker.wake();
        });
    }

    // ---- api ----------------------------------------------------------------

    fn cancel_search(&mut self) {
        for task in self.search_tasks.drain(..) {
            task.abort();
        }
    }

    fn search(&mut self, query: String, serial: u64) {
        self.cancel_search();
        if query.is_empty() {
            return;
        }
        self.emit(Event::Api(Box::new(ApiResponse::SearchStarted {
            query: query.clone(),
            serial,
            split: false,
        })));
        let task = self.dispatch(ApiRequest::Search { query, serial });
        self.search_tasks.extend(task);
    }

    fn dispatch(&self, request: ApiRequest) -> Option<tokio::task::AbortHandle> {
        let client = self.client.clone()?;
        let background_api = Arc::clone(&self.background_api);
        let background = request.background();
        let engine = self.engine.clone();
        let commands = self.commands.clone();
        let mut session = self.session.subscribe();
        let generation = *session.borrow_and_update();
        Some(
            tokio::spawn(async move {
                let (response, expired) = tokio::select! {
                    _ = session.changed() => return,
                    result = async {
                        let _background_permit = if background {
                            background_api.acquire_owned().await.ok()
                        } else {
                            None
                        };
                        handle(&client, engine.as_deref(), request).await
                    } => result,
                };
                // Apply completion on the command loop. A late response cannot
                // clear or repopulate a session created after sign-out.
                let _ = commands.send(Command::ApiFinished {
                    generation,
                    response: Box::new(response),
                    expired,
                });
            })
            .abort_handle(),
        )
    }

    fn accent(&self, url: String) {
        let art = self.art.clone();
        let events = self.events.clone();
        let waker = self.waker.clone();
        tokio::spawn(async move {
            if let Ok(bytes) = art.fetch(&url).await {
                let color = tokio::task::spawn_blocking(move || accent_color(&bytes))
                    .await
                    .ok()
                    .flatten();
                if let Some(color) = color {
                    let _ = events.send(Event::Accent { url, color });
                    waker.wake();
                }
            }
        });
    }
}

/// Feeds the engine in the order the interface asked, asking the server
/// about the songs a load or a queue addition names before handing them over.
async fn run_player(
    engine: Arc<Engine>,
    client: Arc<ApiClient>,
    mut jobs: mpsc::UnboundedReceiver<PlayerJob>,
    events: std::sync::mpsc::Sender<Event>,
    waker: Waker,
) {
    while let Some(PlayerJob { command }) = jobs.recv().await {
        let result = match command {
            PlayerCommand::Load(spec) => match resolve_load(&client, spec).await {
                Ok(load) => {
                    // The fade of what is on now blocks for a moment.
                    let engine = Arc::clone(&engine);
                    tokio::task::spawn_blocking(move || engine.load(load))
                        .await
                        .unwrap_or_else(|error| Err(anyhow::anyhow!("{error}")))
                }
                Err(error) => Err(anyhow::anyhow!("{error}")),
            },
            PlayerCommand::AddToQueue(uri) => match client.tracks_by_uri(&[uri]).await {
                Ok(tracks) if tracks.is_empty() => Err(anyhow::anyhow!("only songs can be queued")),
                Ok(tracks) => engine.enqueue(tracks),
                Err(error) => Err(anyhow::anyhow!("{error}")),
            },
            command => {
                let engine = Arc::clone(&engine);
                tokio::task::spawn_blocking(move || engine.command(command))
                    .await
                    .unwrap_or_else(|error| Err(anyhow::anyhow!("{error}")))
            }
        };
        if let Err(error) = result {
            let _ = events.send(Event::Error(format!("Playback error: {error}")));
            waker.wake();
        }
    }
}

/// Asks the server for the songs a load names.
async fn resolve_load(client: &ApiClient, spec: LoadSpec) -> ApiResult<Load> {
    if spec.autoplay {
        // What follows a list that ran out: the server's mix of its last
        // song, without that song again.
        let seed = spec
            .context_uri
            .clone()
            .or_else(|| spec.uris.last().cloned())
            .unwrap_or_default();
        let id = crate::util::uri_id(&seed).unwrap_or_default().to_string();
        let tracks: Vec<_> = client
            .instant_mix(&id, RADIO_SIZE)
            .await?
            .into_iter()
            .filter(|track| track.uri != seed)
            .map(|track| client.playable(track))
            .collect();
        return Ok(Load {
            context_uri: crate::util::station_uri(&seed),
            tracks,
            start: 0,
            position_ms: 0,
            play: spec.play,
            shuffle: Some(false),
            repeat: spec.repeat,
        });
    }
    let request = PlayRequest {
        context_uri: spec.context_uri.clone(),
        uris: spec.uris,
        offset_uri: spec.offset_uri,
        offset_position: spec.offset_index,
        position_ms: spec.position_ms,
    };
    let (tracks, start) = client.resolve(&request).await?;
    Ok(Load {
        // A lone song is its own context only by name; as a queue it is a
        // plain list of one.
        context_uri: spec
            .context_uri
            .filter(|uri| crate::util::uri_kind(uri) != Some("track")),
        tracks,
        start,
        position_ms: spec.position_ms,
        play: spec.play,
        shuffle: spec.shuffle,
        repeat: spec.repeat,
    })
}

/// Tells the server what this computer plays: a start, a stop, and progress
/// in between. The server's play counts and "recently played" come from it.
async fn report_playback(client: Arc<ApiClient>, mut states: mpsc::UnboundedReceiver<LocalState>) {
    let play_session = crate::auth::new_device_id();
    let mut reported: Option<(String, u64)> = None;
    let mut last = LocalState::default();
    let mut last_progress = Instant::now();
    let report = |event: ReportEvent, state: &LocalState, uri: &str, position_ms: u32| {
        let report = PlaybackReport {
            event,
            item_id: crate::util::uri_id(uri).unwrap_or_default().to_string(),
            play_session: play_session.clone(),
            position_ms,
            paused: state.playback == Playback::Paused,
            volume_percent: (u32::from(state.volume) * 100 / u32::from(u16::MAX)) as u8,
            repeat: state.repeat.server_name(),
            shuffle: state.shuffle,
        };
        let client = Arc::clone(&client);
        async move {
            if let Err(error) = client.report(&report).await {
                log::debug!("playback report not delivered: {error}");
            }
        }
    };
    loop {
        let state = match tokio::time::timeout(PROGRESS_INTERVAL, states.recv()).await {
            Ok(Some(state)) => state,
            Ok(None) => break,
            // Nothing changed for a while: a playing song still reports in.
            Err(_) => last.clone(),
        };
        let playing = state
            .track
            .as_ref()
            .filter(|_| matches!(state.playback, Playback::Playing | Playback::Paused))
            .map(|track| (track.uri.clone(), state.track_sequence));
        if playing != reported {
            if let Some((uri, _)) = reported.take() {
                // The song that was on ends where it had got to.
                let position = if last.track.as_ref().is_some_and(|track| track.uri == uri) {
                    last.position_now()
                } else {
                    0
                };
                report(ReportEvent::Stop, &last, &uri, position).await;
            }
            if let Some((uri, _)) = &playing {
                report(ReportEvent::Start, &state, uri, state.position_now()).await;
                last_progress = Instant::now();
            }
            reported = playing;
        } else if let Some((uri, _)) = &reported {
            let changed = state.playback != last.playback
                || state.seek_sequence != last.seek_sequence
                || state.shuffle != last.shuffle
                || state.repeat != last.repeat
                || state.volume != last.volume;
            if changed || last_progress.elapsed() >= PROGRESS_INTERVAL {
                report(ReportEvent::Progress, &state, uri, state.position_now()).await;
                last_progress = Instant::now();
            }
        }
        last = state;
    }
    if let Some((uri, _)) = reported {
        report(ReportEvent::Stop, &last, &uri, last.position_now()).await;
    }
}

fn unsupported<T>(what: &str) -> ApiResult<T> {
    Err(ApiError::Status {
        status: 0,
        message: what.to_string(),
    })
}

/// Answers one request from the interface. The flag says the server
/// rejected the session's token.
async fn handle(
    client: &ApiClient,
    engine: Option<&Engine>,
    request: ApiRequest,
) -> (ApiResponse, bool) {
    let expired = std::cell::Cell::new(false);
    macro_rules! call {
        ($call:expr) => {{
            let result = $call.await;
            if let Err(ApiError::SignInExpired) = &result {
                expired.set(true);
            }
            result
        }};
    }
    const NO_PODCASTS: &str = "Jellyfin has no podcasts.";

    let response = match request {
        ApiRequest::Me => ApiResponse::Me(call!(client.me())),
        ApiRequest::Devices => ApiResponse::Devices(call!(client.devices())),
        ApiRequest::PlaybackState { seq } => ApiResponse::PlaybackState {
            seq,
            result: call!(client.playback_state()),
        },
        ApiRequest::Queue { seq } => ApiResponse::Queue {
            seq,
            result: Ok(engine.map(Engine::queue).unwrap_or_default()),
        },
        ApiRequest::RecentlyPlayed {
            who,
            generation,
            before,
            limit,
        } => ApiResponse::RecentlyPlayed {
            who,
            generation,
            limit,
            result: call!(client.recently_played(limit, before.as_deref())),
        },
        ApiRequest::TopTracks {
            offset,
            full,
            generation,
        } => ApiResponse::TopTracks {
            result: call!(client.top_tracks(if full { 50 } else { 20 }, offset)),
            offset,
            full,
            generation,
        },
        ApiRequest::TopArtists { generation } => ApiResponse::TopArtists {
            generation,
            result: call!(client.top_artists(20)),
        },
        ApiRequest::Recommendations {
            seed_tracks,
            seed_artists,
            generation,
        } => {
            let seed = seed_tracks.first().or(seed_artists.first()).cloned();
            ApiResponse::Recommendations {
                generation,
                result: match seed {
                    Some(seed) => call!(client.instant_mix(&seed, 20)),
                    None => Ok(Vec::new()),
                },
            }
        }
        ApiRequest::LatestAlbums { generation } => ApiResponse::LatestAlbums {
            generation,
            result: call!(client.latest_albums(20)),
        },
        ApiRequest::MyPlaylists { offset, generation } => ApiResponse::MyPlaylists {
            offset,
            generation,
            result: call!(client.my_playlists(offset, 50)),
        },
        ApiRequest::Playlist { id, generation } => ApiResponse::Playlist {
            result: call!(client.playlist(&id)),
            id,
            generation,
        },
        ApiRequest::PlaylistItems {
            id,
            offset,
            generation,
        } => ApiResponse::PlaylistItems {
            result: call!(client.playlist_items(&id, offset, PLAYLIST_PAGE_SIZE)),
            id,
            offset,
            generation,
        },
        ApiRequest::PlaylistSample {
            id,
            offset,
            generation,
        } => ApiResponse::PlaylistSample {
            result: call!(client.playlist_items(&id, offset, PLAYLIST_PAGE_SIZE)),
            id,
            generation,
        },
        ApiRequest::CreatePlaylist {
            name,
            public,
            description,
        } => {
            ApiResponse::PlaylistCreated(call!(client.create_playlist(&name, public, &description)))
        }
        ApiRequest::UploadPlaylistCover {
            id,
            request,
            previous_urls,
            cover,
        } => ApiResponse::PlaylistCoverUploaded {
            request,
            previous_urls,
            result: call!(client.upload_playlist_cover(&id, &cover.encoded)),
            id,
            cover,
        },
        ApiRequest::UpdatePlaylist {
            id,
            name,
            description,
            public,
        } => ApiResponse::PlaylistUpdated {
            result: call!(client.update_playlist(
                &id,
                name.as_deref(),
                description.as_deref(),
                public
            )),
            id,
        },
        ApiRequest::CheckPlaylistDuplicates {
            playlist_id,
            playlist_name,
            items,
            position,
        } => {
            let uris: Vec<String> = items.iter().map(|item| item.uri().to_string()).collect();
            ApiResponse::PlaylistDuplicatesChecked {
                result: call!(client.playlist_duplicates(&playlist_id, &uris)),
                playlist_id,
                playlist_name,
                items,
                position,
            }
        }
        ApiRequest::AddToPlaylist {
            playlist_id,
            playlist_name,
            uris,
            position,
        } => ApiResponse::PlaylistItemsChanged {
            result: call!(client.add_playlist_items(&playlist_id, &uris, position)),
            id: playlist_id,
            message: format!("Added to {playlist_name}"),
        },
        ApiRequest::RemoveFromPlaylist {
            playlist_id, uris, ..
        } => ApiResponse::PlaylistItemsChanged {
            result: call!(client.remove_playlist_items(&playlist_id, &uris)),
            id: playlist_id,
            message: "Removed from playlist".to_string(),
        },
        ApiRequest::ReorderPlaylist {
            playlist_id,
            range_start,
            insert_before,
            ..
        } => ApiResponse::PlaylistItemsChanged {
            result: call!(client.reorder_playlist(&playlist_id, range_start, insert_before)),
            id: playlist_id,
            message: String::new(),
        },
        ApiRequest::DeletePlaylist { id } => ApiResponse::PlaylistDeleted {
            result: call!(client.delete_playlist(&id)),
            id,
        },
        ApiRequest::SavedTracks { offset, generation } => ApiResponse::SavedTracks {
            offset,
            generation,
            account_id: Some(client.user_id().to_string()),
            result: call!(client.saved_tracks(offset, 50)),
        },
        ApiRequest::SavedAlbums { offset } => ApiResponse::SavedAlbums {
            offset,
            result: call!(client.saved_albums(offset, 50)),
        },
        ApiRequest::FollowedArtists { after } => ApiResponse::FollowedArtists {
            result: call!(client.followed_artists(after.as_deref(), 50)),
            after,
        },
        ApiRequest::SetSaved { uris, saved } => ApiResponse::SavedChanged {
            result: call!(client.set_saved(&uris, saved)),
            uris,
            saved,
        },
        ApiRequest::Contains { uris } => ApiResponse::Contains {
            result: call!(client.contains(&uris)),
            uris,
        },
        ApiRequest::Search { query, serial } => ApiResponse::Search {
            result: call!(client.search(&query, 20)),
            query,
            serial,
        },
        ApiRequest::Artist { id } => ApiResponse::Artist {
            result: call!(client.artist(&id)),
            id,
        },
        ApiRequest::ArtistTopTracks { id } => ApiResponse::ArtistTopTracks {
            result: call!(client.artist_top_tracks(&id)),
            id,
        },
        ApiRequest::ArtistAlbums { id, groups, offset } => ApiResponse::ArtistAlbums {
            result: call!(client.artist_albums(&id, &groups, offset, 50)),
            id,
            groups,
            offset,
        },
        ApiRequest::RelatedArtists { id } => ApiResponse::RelatedArtists {
            result: call!(client.related_artists(&id)),
            id,
        },
        ApiRequest::Album { id } => ApiResponse::Album {
            result: call!(client.album(&id)),
            id,
        },
        ApiRequest::AlbumTracks {
            id,
            offset,
            generation,
        } => ApiResponse::AlbumTracks {
            result: call!(client.album_tracks(&id, offset, 50)),
            id,
            offset,
            generation,
        },
        ApiRequest::AlbumQueueTracks {
            id,
            offset,
            request,
        } => ApiResponse::AlbumQueueTracks {
            offset,
            request,
            result: call!(client.album_tracks(&id, offset, 50)),
        },
        ApiRequest::Track { id } => ApiResponse::Track {
            result: call!(client.track(&id)),
            id,
        },
        ApiRequest::SavedShows { offset } => ApiResponse::SavedShows {
            offset,
            result: Ok(Page::default()),
        },
        ApiRequest::SavedEpisodes { offset } => ApiResponse::SavedEpisodes {
            offset,
            result: Ok(Page::default()),
        },
        ApiRequest::Show { id } => ApiResponse::Show {
            id,
            result: unsupported(NO_PODCASTS),
        },
        ApiRequest::ShowEpisodes { id, offset } => ApiResponse::ShowEpisodes {
            id,
            offset,
            result: Ok(Page::default()),
        },
        ApiRequest::HomeEpisodes { generation, .. } => ApiResponse::HomeEpisodes {
            generation,
            result: Ok(Vec::new()),
        },
        ApiRequest::Episode { id } => ApiResponse::Episode {
            id,
            result: unsupported(NO_PODCASTS),
        },
        ApiRequest::Remote {
            action,
            device_id,
            play,
            position_ms,
            percent,
            flag,
            repeat,
        } => {
            let device = device_id.as_deref();
            let result = match action {
                RemoteAction::Play => call!(client.play(device, play.as_ref())),
                RemoteAction::Pause => call!(client.pause(device)),
                RemoteAction::Next => call!(client.next(device)),
                RemoteAction::Previous => call!(client.previous(device)),
                RemoteAction::Seek => call!(client.seek(position_ms, device)),
                RemoteAction::Volume => call!(client.set_volume(percent, device)),
                RemoteAction::Shuffle => call!(client.set_shuffle(flag, device)),
                RemoteAction::Repeat => call!(client.set_repeat(&repeat, device)),
            };
            ApiResponse::Remote { action, result }
        }
        ApiRequest::ShufflePlay { device_id, play } => {
            let device = device_id.as_deref();
            // Play first: a Jellyfin player shuffles the queue it has.
            let result = match call!(client.play(device, Some(&play))) {
                Ok(()) => call!(client.set_shuffle(true, device)),
                Err(error) => Err(error),
            };
            ApiResponse::Remote {
                action: RemoteAction::Play,
                result,
            }
        }
        ApiRequest::Transfer { device_id, play } => {
            // Hand the other player what this computer has in its queue.
            let queue = engine.map(Engine::queue).unwrap_or_default();
            let position_ms = engine.map_or(0, |engine| engine.state().position_now());
            let uris: Vec<String> = queue
                .currently_playing
                .iter()
                .chain(queue.queue.iter())
                .map(|item| item.uri().to_string())
                .collect();
            let result = if uris.is_empty() {
                unsupported("Play something here first, then move it to the other player.")
            } else {
                let request = PlayRequest {
                    uris,
                    position_ms,
                    ..PlayRequest::default()
                };
                match call!(client.play(Some(&device_id), Some(&request))) {
                    Ok(()) if !play => call!(client.pause(Some(&device_id))),
                    other => other,
                }
            };
            ApiResponse::Transferred { result, device_id }
        }
        ApiRequest::AddToQueue {
            uri,
            device_id,
            label,
        } => ApiResponse::QueueAdded {
            result: call!(client.add_to_queue(&[uri], device_id.as_deref())),
            label,
        },
        ApiRequest::AddManyToQueue {
            request,
            uris,
            device_id,
        } => {
            let result = call!(client.add_to_queue(&uris, device_id.as_deref()));
            ApiResponse::QueueBatchAdded {
                request,
                added: if result.is_ok() { uris.len() } else { 0 },
                result,
            }
        }
    };
    (response, expired.get())
}

/// A playlist's items on disk, valid for exactly one snapshot.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedPlaylist {
    snapshot: String,
    items: Vec<PlaylistItem>,
    /// Absent in the original whole-playlist cache format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total: Option<u32>,
    /// A value means this is a prefix. Absent means the cache is complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_offset: Option<u32>,
}

#[cfg(test)]
async fn read_cached_playlist(path: std::path::PathBuf) -> std::io::Result<CachedPlaylist> {
    tokio::task::spawn_blocking(move || read_cached_playlist_file(&path))
        .await
        .map_err(std::io::Error::other)?
}

fn read_cached_playlist_file(path: &std::path::Path) -> std::io::Result<CachedPlaylist> {
    let file = std::fs::File::open(path)?;
    // Parse on the file worker without keeping the entire JSON alongside
    // the deserialized playlist. Snapshot/count validation still follows.
    serde_json::from_reader(std::io::BufReader::new(file)).map_err(std::io::Error::other)
}

async fn read_playlist_cache(path: std::path::PathBuf) -> std::io::Result<(CachedPlaylist, bool)> {
    tokio::task::spawn_blocking(move || {
        // A writer may have created a row file that its manifest does not yet
        // reference. Hold the account lock through both reading and recovery.
        let lock = playlist_cache_lock(&path);
        if let Err(error) = &lock {
            log::warn!("unable to lock playlist cache {}: {error}", path.display());
        }
        let cache = read_incremental_playlist_cache_file(&path)
            .map(|cache| (cache, true))
            // A missing or damaged new cache must not hide an older JSON cache.
            .or_else(|_| read_cached_playlist_file(&path).map(|cache| (cache, false)));
        if lock.is_ok()
            && let Err(error) = cleanup_unreferenced_playlist_rows(&path)
        {
            log::warn!("unable to clean playlist cache {}: {error}", path.display());
        }
        cache
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
async fn write_cached_playlist(
    path: std::path::PathBuf,
    cached: CachedPlaylist,
) -> std::io::Result<()> {
    // Move the checkpoint into the file worker without another model clone.
    // Awaiting it preserves checkpoint order, including after playlist edits.
    tokio::task::spawn_blocking(move || write_cached_playlist_file(&path, &cached))
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(test)]
fn write_cached_playlist_file(
    path: &std::path::Path,
    cached: &CachedPlaylist,
) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let file = std::fs::File::create(&temporary)?;
    let result = (|| {
        // Buffer small serializer writes without retaining the whole JSON file.
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, cached).map_err(std::io::Error::other)?;
        writer.flush()?;
        // Windows requires the temporary file to be closed before replacement.
        drop(writer);
        crate::util::replace_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// The manifest stays constant-sized as the row file grows. A reader only
/// consumes `bytes`, so an interrupted append cannot become visible.
#[derive(serde::Serialize, serde::Deserialize)]
struct PlaylistCacheManifest {
    version: u8,
    snapshot: String,
    data_file: u64,
    bytes: u64,
    rows: u32,
    total: u32,
    next_offset: Option<u32>,
}

fn playlist_manifest_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension("manifest.json")
}

fn playlist_data_path(path: &std::path::Path, data_file: u64) -> std::path::PathBuf {
    path.with_extension(format!("rows.{data_file:016x}"))
}

fn playlist_cache_lock(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "playlist cache has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(".playlist-cache.lock"))?;
    file.lock()?;
    Ok(file)
}

/// Recover row files from interrupted replacements. Run under the account
/// lock so a new, unpublished row file cannot be mistaken for an orphan.
fn cleanup_unreferenced_playlist_rows(path: &std::path::Path) -> std::io::Result<()> {
    let current = match read_playlist_manifest(path) {
        Ok(manifest) => Some(manifest.data_file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "playlist cache has no parent",
        )
    })?;
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid playlist cache name",
            )
        })?;
    let prefix = format!("{stem}.rows.");
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
            continue;
        };
        if suffix.len() != 16 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let data_file = u64::from_str_radix(suffix, 16).map_err(std::io::Error::other)?;
        if Some(data_file) == current {
            continue;
        }
        if let Err(error) = std::fs::remove_file(entry.path())
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "unable to remove orphan playlist rows {}: {error}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn read_playlist_manifest(path: &std::path::Path) -> std::io::Result<PlaylistCacheManifest> {
    let file = std::fs::File::open(playlist_manifest_path(path))?;
    let manifest: PlaylistCacheManifest =
        serde_json::from_reader(std::io::BufReader::new(file)).map_err(std::io::Error::other)?;
    if manifest.version != 2
        || manifest.rows > manifest.total
        || manifest
            .next_offset
            .is_some_and(|offset| offset > manifest.total)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid playlist cache manifest",
        ));
    }
    Ok(manifest)
}

fn read_incremental_playlist_cache_file(path: &std::path::Path) -> std::io::Result<CachedPlaylist> {
    use std::io::Read;

    let manifest = read_playlist_manifest(path)?;
    let file = std::fs::File::open(playlist_data_path(path, manifest.data_file))?;
    if file.metadata()?.len() < manifest.bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated playlist cache data",
        ));
    }
    let reader = std::io::BufReader::new(file.take(manifest.bytes));
    let mut items = Vec::new();
    for block in serde_json::Deserializer::from_reader(reader).into_iter::<Vec<PlaylistItem>>() {
        let block = block.map_err(std::io::Error::other)?;
        if items.is_empty() {
            items = block;
        } else {
            items.extend(block);
        }
        if items.len() > manifest.rows as usize {
            break;
        }
    }
    if items.len() != manifest.rows as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "playlist cache row count does not match its manifest",
        ));
    }
    Ok(CachedPlaylist {
        snapshot: manifest.snapshot,
        items,
        total: Some(manifest.total),
        next_offset: manifest.next_offset,
    })
}

/// Keep writes in command order without making the command loop wait for disk.
async fn store_playlist_caches(
    mut writes: mpsc::Receiver<PlaylistCacheWrite>,
    events: std::sync::mpsc::Sender<Event>,
    waker: Waker,
) {
    while let Some(write) = writes.recv().await {
        let PlaylistCacheWrite {
            path,
            account_id,
            id,
            generation,
            snapshot,
            rows,
            total,
            next_offset,
        } = write;
        let result = write_incremental_playlist_cache(
            path.clone(),
            snapshot.clone(),
            rows,
            total,
            next_offset,
        )
        .await;
        if let Err(error) = &result {
            log::warn!("unable to store playlist cache {}: {error}", path.display());
        }
        let _ = events.send(Event::PlaylistCacheStored {
            account_id,
            id,
            generation,
            snapshot,
            success: result.is_ok(),
        });
        waker.wake();
    }
}

async fn write_incremental_playlist_cache(
    path: std::path::PathBuf,
    snapshot: String,
    rows: PlaylistCacheRows,
    total: u32,
    next_offset: Option<u32>,
) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        write_incremental_playlist_cache_file(&path, snapshot, rows, total, next_offset)
    })
    .await
    .map_err(std::io::Error::other)?
}

fn write_incremental_playlist_cache_file(
    path: &std::path::Path,
    snapshot: String,
    rows: PlaylistCacheRows,
    total: u32,
    next_offset: Option<u32>,
) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};

    if next_offset.is_some_and(|offset| offset > total) {
        return Err(Error::new(ErrorKind::InvalidInput, "offset exceeds total"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = playlist_cache_lock(path)?;
    match rows {
        PlaylistCacheRows::Replace(items) => {
            let count = u32::try_from(items.len())
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "too many playlist rows"))?;
            if count > total {
                return Err(Error::new(ErrorKind::InvalidInput, "rows exceed total"));
            }
            let (file, data_file) = create_playlist_data_file(path)?;
            let data_path = playlist_data_path(path, data_file);
            let result = (|| {
                let bytes = write_playlist_block(file, &items)?;
                write_playlist_manifest(
                    path,
                    &PlaylistCacheManifest {
                        version: 2,
                        snapshot,
                        data_file,
                        bytes,
                        rows: count,
                        total,
                        next_offset,
                    },
                )
            })();
            if result.is_err() {
                if let Err(error) = std::fs::remove_file(&data_path) {
                    log::warn!(
                        "unable to remove incomplete playlist rows {}: {error}",
                        data_path.display()
                    );
                }
            } else if let Err(error) = cleanup_unreferenced_playlist_rows(path) {
                log::warn!("unable to clean playlist cache {}: {error}", path.display());
            }
            result
        }
        PlaylistCacheRows::Append {
            previous_rows,
            previous_offset,
            items,
        } => {
            let mut manifest = read_playlist_manifest(path)?;
            if manifest.snapshot != snapshot
                || manifest.total != total
                || manifest.rows as usize != previous_rows
                || manifest.next_offset.unwrap_or(manifest.total) != previous_offset
            {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "playlist cache changed before append",
                ));
            }
            let added = u32::try_from(items.len())
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "too many playlist rows"))?;
            let count = manifest
                .rows
                .checked_add(added)
                .filter(|count| *count <= total)
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "rows exceed total"))?;
            let data_path = playlist_data_path(path, manifest.data_file);
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(data_path)?;
            if file.metadata()?.len() < manifest.bytes {
                return Err(Error::new(ErrorKind::UnexpectedEof, "truncated cache data"));
            }
            // Drop bytes from a write whose manifest was never published.
            file.set_len(manifest.bytes)?;
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(manifest.bytes))?;
            let bytes = write_playlist_block(file, &items)?;
            manifest.bytes = bytes;
            manifest.rows = count;
            manifest.next_offset = next_offset;
            write_playlist_manifest(path, &manifest)
        }
    }
}

fn create_playlist_data_file(path: &std::path::Path) -> std::io::Result<(std::fs::File, u64)> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    for attempt in 0..1024 {
        let data_file = seed.wrapping_add(attempt);
        let candidate = playlist_data_path(path, data_file);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(candidate)
        {
            Ok(file) => return Ok((file, data_file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "playlist cache data names exhausted",
    ))
}

fn write_playlist_block(file: std::fs::File, items: &[PlaylistItem]) -> std::io::Result<u64> {
    use std::io::Write;

    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, items).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    let file = writer.into_inner().map_err(|error| error.into_error())?;
    // Publish the manifest only after its referenced bytes are durable.
    file.sync_all()?;
    Ok(file.metadata()?.len())
}

fn write_playlist_manifest(
    path: &std::path::Path,
    manifest: &PlaylistCacheManifest,
) -> std::io::Result<()> {
    use std::io::Write;

    let target = playlist_manifest_path(path);
    let temporary = target.with_extension("json.tmp");
    let result = (|| {
        let file = std::fs::File::create(&temporary)?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, manifest).map_err(std::io::Error::other)?;
        writer.flush()?;
        writer
            .into_inner()
            .map_err(|error| error.into_error())?
            .sync_all()?;
        crate::util::replace_file(&temporary, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod playlist_cache_tests {
    use super::{
        CachedPlaylist, PlaylistCacheRows, playlist_data_path, playlist_manifest_path,
        read_cached_playlist, read_incremental_playlist_cache_file, read_playlist_cache,
        read_playlist_manifest, write_cached_playlist, write_incremental_playlist_cache_file,
    };
    use crate::api::models::{PlayableItem, PlaylistItem, Track};

    #[test]
    fn the_original_complete_cache_format_remains_readable() {
        let cached: CachedPlaylist =
            serde_json::from_str(r#"{"snapshot":"old","items":[]}"#).unwrap();

        assert_eq!(cached.snapshot, "old");
        assert_eq!(cached.total, None);
        assert_eq!(cached.next_offset, None);
    }

    #[test]
    fn incremental_checkpoints_append_only_new_rows_and_recover_from_failed_publication() {
        let root = std::env::temp_dir().join(format!(
            "jellifast-playlist-cache-incremental-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("playlist.json");
        let row = |uri: &str| PlaylistItem {
            item: Some(PlayableItem::Track(Track {
                uri: uri.into(),
                ..Track::default()
            })),
            ..PlaylistItem::default()
        };
        let first = vec![row("jellyfin:track:one"), PlaylistItem::default()];
        let added = row("jellyfin:track:three");
        write_incremental_playlist_cache_file(
            &path,
            "same".into(),
            PlaylistCacheRows::Replace(first.clone()),
            100,
            Some(50),
        )
        .unwrap();
        let initial = read_playlist_manifest(&path).unwrap();
        assert_eq!(initial.rows, 2);
        assert_eq!(
            initial.bytes as usize,
            serde_json::to_vec(&first).unwrap().len() + 1
        );

        let append = || PlaylistCacheRows::Append {
            previous_rows: 2,
            previous_offset: 50,
            items: vec![added.clone()],
        };
        let temporary = playlist_manifest_path(&path).with_extension("json.tmp");
        std::fs::create_dir(&temporary).unwrap();
        assert!(
            write_incremental_playlist_cache_file(&path, "same".into(), append(), 100, Some(75))
                .is_err()
        );
        assert_eq!(
            read_incremental_playlist_cache_file(&path).unwrap().items,
            first
        );
        assert_eq!(read_playlist_manifest(&path).unwrap().bytes, initial.bytes);
        std::fs::remove_dir(&temporary).unwrap();

        write_incremental_playlist_cache_file(&path, "same".into(), append(), 100, Some(75))
            .unwrap();
        let committed = read_playlist_manifest(&path).unwrap();
        assert_eq!(committed.rows, 3);
        assert_eq!(
            committed.bytes - initial.bytes,
            (serde_json::to_vec(std::slice::from_ref(&added))
                .unwrap()
                .len()
                + 1) as u64,
            "an append writes only its new block, including after a failed publication"
        );
        assert_eq!(
            std::fs::metadata(playlist_data_path(&path, committed.data_file))
                .unwrap()
                .len(),
            committed.bytes
        );
        let restored = read_incremental_playlist_cache_file(&path).unwrap();
        assert_eq!(restored.items, [first, vec![added]].concat());
        assert_eq!(restored.next_offset, Some(75));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn incremental_cache_replaces_changed_snapshot_and_keeps_legacy_reader() {
        let root = std::env::temp_dir().join(format!(
            "jellifast-playlist-cache-migration-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("playlist.json");
        write_cached_playlist(
            path.clone(),
            CachedPlaylist {
                snapshot: "legacy".into(),
                items: vec![PlaylistItem::default()],
                total: Some(10),
                next_offset: Some(5),
            },
        )
        .await
        .unwrap();
        let (cached, appendable) = read_playlist_cache(path.clone()).await.unwrap();
        assert_eq!(cached.snapshot, "legacy");
        assert!(!appendable);

        write_incremental_playlist_cache_file(
            &path,
            "new".into(),
            PlaylistCacheRows::Replace(vec![PlaylistItem::default(); 2]),
            10,
            Some(5),
        )
        .unwrap();
        let old_data = read_playlist_manifest(&path).unwrap().data_file;
        let (cached, appendable) = read_playlist_cache(path.clone()).await.unwrap();
        assert_eq!(cached.snapshot, "new");
        assert!(appendable);
        assert_eq!(
            read_playlist_cache(path.clone())
                .await
                .unwrap()
                .0
                .items
                .len(),
            2
        );

        std::fs::OpenOptions::new()
            .write(true)
            .open(playlist_data_path(&path, old_data))
            .unwrap()
            .set_len(0)
            .unwrap();
        let (cached, appendable) = read_playlist_cache(path.clone()).await.unwrap();
        assert_eq!(cached.snapshot, "legacy");
        assert!(!appendable);

        write_incremental_playlist_cache_file(
            &path,
            "newer".into(),
            PlaylistCacheRows::Replace(vec![PlaylistItem::default()]),
            1,
            None,
        )
        .unwrap();
        assert_eq!(
            read_playlist_cache(path.clone()).await.unwrap().0.snapshot,
            "newer"
        );
        assert!(!playlist_data_path(&path, old_data).exists());
        assert_eq!(read_cached_playlist(path).await.unwrap().snapshot, "legacy");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reopening_a_cache_removes_rows_left_by_an_interrupted_replacement() {
        let root = std::env::temp_dir().join(format!(
            "jellifast-playlist-cache-orphan-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("playlist.json");
        write_incremental_playlist_cache_file(
            &path,
            "current".into(),
            PlaylistCacheRows::Replace(vec![PlaylistItem::default()]),
            1,
            None,
        )
        .unwrap();
        let current = read_playlist_manifest(&path).unwrap().data_file;
        let orphan = playlist_data_path(&path, current.wrapping_add(1));
        let blocked = playlist_data_path(&path, current.wrapping_add(2));
        let recoverable = playlist_data_path(&path, current.wrapping_add(3));
        let other_playlist = playlist_data_path(&root.join("other.json"), 1);
        std::fs::write(&orphan, b"unfinished rows").unwrap();
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(&recoverable, b"unfinished rows").unwrap();
        std::fs::write(&other_playlist, b"unrelated rows").unwrap();

        let (cached, appendable) = read_playlist_cache(path.clone()).await.unwrap();
        assert_eq!(cached.snapshot, "current");
        assert!(appendable);
        assert!(
            !orphan.exists(),
            "recovery must remove unreferenced row files"
        );
        assert!(playlist_data_path(&path, current).exists());
        assert!(
            !recoverable.exists(),
            "one failed removal must not stop cleanup"
        );
        assert!(blocked.is_dir());
        assert!(other_playlist.exists());
        std::fs::remove_dir(&blocked).unwrap();

        let before_manifest = root.join("cold.json");
        let orphan = playlist_data_path(&before_manifest, 1);
        std::fs::write(&orphan, b"unfinished first checkpoint").unwrap();
        assert!(read_playlist_cache(before_manifest).await.is_err());
        assert!(
            !orphan.exists(),
            "a crash before the first manifest is recoverable"
        );

        let orphan = playlist_data_path(&path, current.wrapping_add(1));
        std::fs::write(&orphan, b"unfinished rows").unwrap();
        write_incremental_playlist_cache_file(
            &path,
            "next".into(),
            PlaylistCacheRows::Replace(vec![PlaylistItem::default()]),
            1,
            None,
        )
        .unwrap();
        assert!(!orphan.exists());
        assert!(!playlist_data_path(&path, current).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn the_file_reader_accepts_legacy_caches_and_ignores_unknown_fields() {
        let root = std::env::temp_dir().join(format!(
            "jellifast-playlist-cache-legacy-read-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("playlist.json");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            &path,
            b"{\"snapshot\":\"old\",\"items\":[],\"future_field\":true} \n\t",
        )
        .unwrap();

        let cached = read_cached_playlist(path).await.unwrap();

        assert_eq!(cached.snapshot, "old");
        assert!(cached.items.is_empty());
        assert_eq!(cached.total, None);
        assert_eq!(cached.next_offset, None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn the_file_reader_rejects_missing_corrupt_and_trailing_data() {
        let root = std::env::temp_dir().join(format!(
            "jellifast-playlist-cache-invalid-read-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("playlist.json");
        std::fs::create_dir_all(&root).unwrap();
        assert!(read_cached_playlist(path.clone()).await.is_err());
        for bytes in [
            b"".as_slice(),
            b"{\"snapshot\":\"partial\",\"items\":[",
            b"{\"snapshot\":\"bad utf8: \xff\",\"items\":[]}",
            b"{\"snapshot\":\"missing items\"}",
            b"{\"snapshot\":\"ok\",\"items\":[]}{}",
            b"{\"snapshot\":\"ok\",\"items\":[]}trailing",
        ] {
            std::fs::write(&path, bytes).unwrap();

            assert!(read_cached_playlist(path.clone()).await.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_new_checkpoint_atomically_replaces_the_previous_one() {
        let root = std::env::temp_dir().join(format!(
            "jellifast-playlist-cache-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("playlist.json");
        let cached = |snapshot: &str| CachedPlaylist {
            snapshot: snapshot.into(),
            items: Vec::new(),
            total: Some(10_000),
            next_offset: Some(500),
        };

        write_cached_playlist(path.clone(), cached("first"))
            .await
            .unwrap();
        write_cached_playlist(path.clone(), cached("second"))
            .await
            .unwrap();

        let text = tokio::fs::read_to_string(&path).await.unwrap();
        let stored: CachedPlaylist = serde_json::from_str(&text).unwrap();
        assert_eq!(stored.snapshot, "second");
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn streaming_preserves_the_cache_bytes_and_duplicate_unavailable_rows() {
        let root = std::env::temp_dir().join(format!(
            "jellifast-playlist-cache-stream-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("playlist.json");
        let song = PlayableItem::Track(Track {
            uri: "jellyfin:track:duplicate".into(),
            name: "Song with \"quotes\", newlines\nand 日本語".into(),
            is_playable: Some(false),
            ..Track::default()
        });
        let rows = [
            PlaylistItem {
                item: Some(song.clone()),
                ..PlaylistItem::default()
            },
            PlaylistItem {
                track: Some(song),
                ..PlaylistItem::default()
            },
            PlaylistItem::default(),
        ];
        let cached = CachedPlaylist {
            snapshot: "unchanged-snapshot".into(),
            items: (0..500).flat_map(|_| rows.clone()).collect(),
            total: Some(2_000),
            next_offset: Some(1_500),
        };
        let expected = serde_json::to_vec(&cached).unwrap();
        assert!(
            expected.len() > 64 * 1024,
            "exercise multiple buffer flushes"
        );

        write_cached_playlist(path.clone(), cached).await.unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, expected, "existing readers see the identical format");
        let restored = read_cached_playlist(path.clone()).await.unwrap();
        assert_eq!(restored.snapshot, "unchanged-snapshot");
        assert_eq!(restored.items.len(), 1_500);
        for chunk in restored.items.chunks(3) {
            assert_eq!(chunk, rows);
        }
        assert_eq!(restored.total, Some(2_000));
        assert_eq!(restored.next_offset, Some(1_500));
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_checkpoint_keeps_existing_data_and_cleans_only_its_temporary_file() {
        let root = std::env::temp_dir().join(format!(
            "jellifast-playlist-cache-failure-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("playlist.json");
        let temporary = path.with_extension("json.tmp");
        let cached = || CachedPlaylist {
            snapshot: "new".into(),
            items: Vec::new(),
            total: Some(0),
            next_offset: None,
        };
        std::fs::create_dir_all(&temporary).unwrap();
        let previous = br#"{"snapshot":"old","items":[]}"#;
        std::fs::write(&path, previous).unwrap();

        assert!(write_cached_playlist(path.clone(), cached()).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        assert!(temporary.is_dir(), "a failed create does not own this path");

        // Force final replacement to fail after serialization and flushing.
        std::fs::remove_dir(&temporary).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("preserved"), previous).unwrap();
        assert!(write_cached_playlist(path.clone(), cached()).await.is_err());
        assert_eq!(std::fs::read(path.join("preserved")).unwrap(), previous);
        assert!(!temporary.exists(), "discard the failed checkpoint");
        std::fs::remove_dir_all(root).unwrap();
    }
}
