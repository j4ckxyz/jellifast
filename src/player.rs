//! Playback on this computer.
//!
//! The engine owns one thread that holds the play queue, decodes what the
//! Jellyfin server streams and feeds the output. Its state is folded into a
//! [`LocalState`] snapshot that is pushed to the interface whenever something
//! changed; commands from the interface arrive over a channel and are applied
//! between packets, so a skip never waits for a song to finish.
//!
//! A song is fetched once, from start to end, into memory while it plays.
//! The decoder reads from that buffer, waits for bytes that have not arrived
//! yet, and seeks within it.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use crate::api::ApiClient;
use crate::api::client::Playable;
use crate::api::models::{ArtistRef, PlayableItem, Queue, Track};
use crate::audio::{AudioPacket, Converter, NUM_CHANNELS, SAMPLE_RATE, Sink, SoftVolume};
use crate::http::Http;
use crate::resample::Resampler;
use crate::sink::{AudioControl, ErrorHook, RodioSink};
use crate::vis::{AudioTap, Tapped};

/// How long the first bytes of a song may take to arrive.
const STREAM_START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a stalled download may hold the decoder before it gives up.
const STREAM_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A song's download may run for as long as the song is listened to.
const STREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
/// Previous goes back a song only this early in the current one; later it
/// starts the current song again.
const RESTART_THRESHOLD_MS: u32 = 3_000;
/// The next song starts downloading when the current one has this left.
const PRELOAD_BEFORE_END_MS: u32 = 20_000;
/// How often the listener's position is corrected while playing.
const POSITION_INTERVAL: Duration = Duration::from_secs(1);
/// Songs in a row that may fail to open before the queue stops.
const MAX_FAILURES: u32 = 3;
/// Songs of the queue shown beyond the playing one.
const QUEUE_VIEW: usize = 200;

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub device_name: String,
    /// The most the server may send per second, in kbit/s. Zero asks for the
    /// file as it is.
    pub bitrate_kbps: u16,
    pub normalisation: bool,
    pub autoplay: bool,
    pub gapless: bool,
    pub backend: Option<String>,
    pub audio_device: Option<String>,
    pub initial_volume: u16,
    pub volume_dir: PathBuf,
    pub audio_cache_dir: Option<PathBuf>,
    pub audio_cache_limit: Option<u64>,
    /// Output buffer length in milliseconds.
    pub buffer_ms: u32,
    pub tap: Arc<AudioTap>,
    /// The equalizer's settings, shared with the window that sets them.
    pub eq: crate::eq::SharedEq,
    /// Proxy used for every request to the server.
    pub proxy: crate::settings::ProxyConfig,
}

impl EngineConfig {
    fn max_bitrate(&self) -> Option<u32> {
        (self.bitrate_kbps > 0).then(|| u32::from(self.bitrate_kbps) * 1000)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Playback {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RepeatMode {
    #[default]
    Off,
    Context,
    Track,
}

impl RepeatMode {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Context,
            Self::Context => Self::Track,
            Self::Track => Self::Off,
        }
    }

    pub fn api_name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Context => "context",
            Self::Track => "track",
        }
    }

    pub fn from_api(name: &str) -> Self {
        match name {
            "context" => Self::Context,
            "track" => Self::Track,
            _ => Self::Off,
        }
    }

    /// The name Jellyfin's playback reports use.
    pub fn server_name(self) -> &'static str {
        match self {
            Self::Off => "RepeatNone",
            Self::Context => "RepeatAll",
            Self::Track => "RepeatOne",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalTrack {
    pub uri: String,
    pub title: String,
    pub artists: Vec<ArtistRef>,
    pub album: String,
    pub art_url: Option<String>,
    pub art_small_url: Option<String>,
    pub duration_ms: u32,
    pub is_episode: bool,
}

impl LocalTrack {
    pub fn artist_names(&self) -> String {
        crate::api::models::join_names(self.artists.iter().map(|artist| artist.name.as_str()))
    }

    fn of(track: &Track) -> Self {
        Self {
            uri: track.uri.clone(),
            title: track.name.clone(),
            artists: track.artists.clone(),
            album: track
                .album
                .as_ref()
                .map(|album| album.name.clone())
                .unwrap_or_default(),
            art_url: track.image(640).map(str::to_string),
            art_small_url: track.image(64).map(str::to_string),
            duration_ms: track.duration_ms,
            is_episode: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalState {
    pub playback: Playback,
    pub track: Option<LocalTrack>,
    pub position_ms: u32,
    /// When `position_ms` was observed; `None` while not advancing.
    pub position_at: Option<Instant>,
    pub volume: u16,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    /// The player is up and signed in to the server.
    pub connected: bool,
    pub username: String,
    pub active_client: String,
    pub error: Option<String>,
    pub seek_sequence: u64,
    /// A newly loaded track, including another play of the same URI.
    pub track_sequence: u64,
    /// Another play of the same track started, and its start position has
    /// not arrived yet. The track looks unchanged, so the `Playing` or
    /// `Paused` that brings the position counts as a seek for media
    /// controls, which would otherwise count on past the end.
    pub replay_pending: bool,
}

/// What playback was doing when its engine went away, so the next one can
/// pick it up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interrupted {
    pub uri: String,
    pub position_ms: u32,
    /// Playing or loading, as opposed to paused.
    pub playing: bool,
}

impl LocalState {
    /// The track and position to come back to, if something was on.
    pub fn interrupted(&self) -> Option<Interrupted> {
        let track = self.track.as_ref()?;
        if self.playback == Playback::Stopped {
            return None;
        }
        Some(Interrupted {
            uri: track.uri.clone(),
            position_ms: self.position_now(),
            playing: matches!(self.playback, Playback::Playing | Playback::Loading),
        })
    }

    /// The position now, interpolated from the last report while playing.
    pub fn position_now(&self) -> u32 {
        match (self.playback, self.position_at) {
            (Playback::Playing, Some(at)) => {
                let elapsed = at.elapsed().as_millis() as u32;
                let limit = self
                    .track
                    .as_ref()
                    .map_or(u32::MAX, |track| track.duration_ms.max(self.position_ms));
                self.position_ms.saturating_add(elapsed).min(limit)
            }
            _ => self.position_ms,
        }
    }

    pub fn is_active(&self) -> bool {
        self.track.is_some() && self.playback != Playback::Stopped
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadSpec {
    pub context_uri: Option<String>,
    pub uris: Vec<String>,
    pub offset_uri: Option<String>,
    pub offset_index: Option<u32>,
    pub position_ms: u32,
    pub play: bool,
    pub shuffle: Option<bool>,
    /// Explicit repeat preference for a new load. Otherwise the engine keeps
    /// its current one.
    pub repeat: Option<RepeatMode>,
    /// Play the server's mix seeded by `context_uri` rather than the context
    /// itself: what follows when a list has run out.
    pub autoplay: bool,
}

/// Songs resolved from a [`LoadSpec`], ready for the engine.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Load {
    pub context_uri: Option<String>,
    pub tracks: Vec<Playable>,
    /// Which of `tracks` starts.
    pub start: usize,
    pub position_ms: u32,
    pub play: bool,
    pub shuffle: Option<bool>,
    pub repeat: Option<RepeatMode>,
}

/// Everything the engine was doing, for the engine that replaces it after an
/// audio setting changed.
#[derive(Clone, Debug)]
pub struct PlaybackResume {
    queue: PlayQueue,
    position_ms: u32,
    playing: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerCommand {
    Toggle,
    Next,
    Previous,
    /// Remove manually queued tracks and keep context tracks.
    ClearQueue,
    /// Queue a track after the ones already queued.
    AddToQueue(String),
    Seek(u32),
    /// The volume to keep.
    Volume(u16),
    /// The slider mid-drag.
    VolumePreview(u16),
    Shuffle(bool),
    Repeat(RepeatMode),
    Load(LoadSpec),
    /// Take over what another player was playing. The interface loads it
    /// here itself; the engine has nothing more to do.
    Transfer,
}

#[allow(clippy::large_enum_variant)]
pub enum EngineEvent {
    State(LocalState),
    SessionEnded,
}

pub type Notify = Arc<dyn Fn(EngineEvent) + Send + Sync>;

// ---- the queue ---------------------------------------------------------------

/// What plays, and in which order.
///
/// A context (an album, a playlist, a plain list) plays in `order`. Songs
/// queued by hand play first, one after another, and the context continues
/// where it was once they are through.
#[derive(Clone, Debug, Default)]
struct PlayQueue {
    context_uri: Option<String>,
    tracks: Vec<Playable>,
    /// Indices into `tracks`, in play order.
    order: Vec<usize>,
    /// The place in `order` the context is at.
    at: Option<usize>,
    manual: VecDeque<Playable>,
    current: Option<Playable>,
    /// The current song came from `manual`, not the context.
    current_manual: bool,
    shuffle: bool,
    repeat: RepeatMode,
}

impl PlayQueue {
    fn load(&mut self, context_uri: Option<String>, tracks: Vec<Playable>, start: usize) {
        self.context_uri = context_uri;
        self.tracks = tracks;
        let start = start.min(self.tracks.len().saturating_sub(1));
        self.order = (0..self.tracks.len()).collect();
        self.at = (!self.tracks.is_empty()).then_some(start);
        if self.shuffle {
            self.shuffle_from(self.at);
        }
        self.current = self.context_track().cloned();
        self.current_manual = false;
    }

    fn context_track(&self) -> Option<&Playable> {
        self.tracks.get(*self.order.get(self.at?)?)
    }

    /// Shuffles the order, keeping the song at `first` (a place in the
    /// current order) at the front so it keeps playing.
    fn shuffle_from(&mut self, first: Option<usize>) {
        use rand::seq::SliceRandom;
        let keep = first.and_then(|at| self.order.get(at).copied());
        let mut rest: Vec<usize> = (0..self.tracks.len())
            .filter(|index| Some(*index) != keep)
            .collect();
        rest.shuffle(&mut rand::rng());
        self.order = keep.into_iter().chain(rest).collect();
        self.at = (!self.order.is_empty()).then_some(0);
    }

    fn set_shuffle(&mut self, shuffle: bool) {
        if self.shuffle == shuffle {
            return;
        }
        self.shuffle = shuffle;
        if shuffle {
            self.shuffle_from(self.at);
        } else {
            let playing = self.at.and_then(|at| self.order.get(at).copied());
            self.order = (0..self.tracks.len()).collect();
            self.at = playing.or((!self.order.is_empty()).then_some(0));
        }
    }

    /// Moves to what follows. `ended` is a song running out, as opposed to a
    /// skip: only then does Repeat One play the same song again.
    fn advance(&mut self, ended: bool) -> Option<Playable> {
        if ended && self.repeat == RepeatMode::Track && self.current.is_some() {
            return self.current.clone();
        }
        if let Some(next) = self.manual.pop_front() {
            self.current = Some(next);
            self.current_manual = true;
            return self.current.clone();
        }
        let next = match self.at {
            Some(at) if at + 1 < self.order.len() => Some(at + 1),
            Some(_) if self.repeat != RepeatMode::Off && !self.order.is_empty() => {
                if self.shuffle {
                    self.shuffle_from(None);
                }
                Some(0)
            }
            _ => None,
        };
        self.at = Some(next?);
        self.current_manual = false;
        self.current = self.context_track().cloned();
        self.current.clone()
    }

    /// Moves to what came before, or `None` when the current song is the
    /// first and should start again.
    fn retreat(&mut self) -> Option<Playable> {
        if self.current_manual {
            // Back from a queued song is the context song it interrupted.
            self.current_manual = false;
            self.current = self.context_track().cloned();
            return self.current.clone();
        }
        let previous = match self.at {
            Some(at) if at > 0 => at - 1,
            Some(_) if self.repeat == RepeatMode::Context && self.order.len() > 1 => {
                self.order.len() - 1
            }
            _ => return None,
        };
        self.at = Some(previous);
        self.current = self.context_track().cloned();
        self.current.clone()
    }

    /// What plays after the current song, without moving.
    fn peek(&self) -> Option<&Playable> {
        if self.repeat == RepeatMode::Track {
            return self.current.as_ref();
        }
        if let Some(next) = self.manual.front() {
            return Some(next);
        }
        match self.at {
            Some(at) if at + 1 < self.order.len() => self.tracks.get(self.order[at + 1]),
            // A reshuffled repeat cannot be foreseen.
            Some(_) if self.repeat == RepeatMode::Context && !self.shuffle => {
                self.tracks.get(*self.order.first()?)
            }
            _ => None,
        }
    }

    fn upcoming(&self) -> Vec<PlayableItem> {
        let context = self
            .at
            .map(|at| &self.order[(at + 1).min(self.order.len())..])
            .unwrap_or_default()
            .iter()
            .filter_map(|index| self.tracks.get(*index));
        self.manual
            .iter()
            .chain(context)
            .take(QUEUE_VIEW)
            .map(|playable| PlayableItem::Track(playable.track.clone()))
            .collect()
    }

    fn view(&self) -> Queue {
        Queue {
            currently_playing: self
                .current
                .as_ref()
                .map(|playable| PlayableItem::Track(playable.track.clone())),
            queue: self.upcoming(),
        }
    }
}

// ---- the stream --------------------------------------------------------------

#[derive(Default)]
struct StreamData {
    bytes: Vec<u8>,
    /// The length the server announced, when it did.
    total: Option<u64>,
    done: bool,
    error: Option<String>,
}

/// A song arriving from the server, readable while it is still downloading.
struct Stream {
    data: Mutex<StreamData>,
    arrived: Condvar,
    cancelled: AtomicBool,
}

/// Stops the download when the last reader of a stream goes away.
struct StreamHandle {
    stream: Arc<Stream>,
    task: tokio::task::AbortHandle,
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stream.cancelled.store(true, Ordering::SeqCst);
        self.task.abort();
        self.stream.arrived.notify_all();
    }
}

/// Where songs come from: the server, through the app's HTTP client.
struct Source {
    client: Arc<ApiClient>,
    http: Http,
    runtime: tokio::runtime::Handle,
    max_bitrate: Option<u32>,
    play_session: String,
}

impl Source {
    fn open(&self, playable: &Playable) -> Result<Arc<StreamHandle>> {
        let id = playable
            .track
            .id
            .clone()
            .ok_or_else(|| anyhow!("the song has no id"))?;
        let url = self
            .client
            .stream_url(&id, self.max_bitrate, &self.play_session);
        let authorization = self.client.authorization();
        let http = self.http.client().map_err(|error| anyhow!(error))?;
        let stream = Arc::new(Stream {
            data: Mutex::new(StreamData::default()),
            arrived: Condvar::new(),
            cancelled: AtomicBool::new(false),
        });
        let sink = Arc::clone(&stream);
        let task = self.runtime.spawn(async move {
            let result = download(&http, &url, &authorization, &sink).await;
            let mut data = sink.data.lock().unwrap_or_else(|lock| lock.into_inner());
            match result {
                Ok(()) => data.done = true,
                Err(error) => data.error = Some(error),
            }
            drop(data);
            sink.arrived.notify_all();
        });
        Ok(Arc::new(StreamHandle {
            stream,
            task: task.abort_handle(),
        }))
    }
}

async fn download(
    http: &reqwest::Client,
    url: &str,
    authorization: &str,
    stream: &Stream,
) -> std::result::Result<(), String> {
    let mut response = http
        .get(url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .timeout(STREAM_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| error.without_url().to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 => "your sign-in expired".to_string(),
            404 => "the server no longer has this song".to_string(),
            _ => format!("the server answered {status}"),
        });
    }
    {
        let mut data = stream.data.lock().unwrap_or_else(|lock| lock.into_inner());
        data.total = response.content_length().filter(|length| *length > 0);
        if let Some(total) = data.total {
            data.bytes.reserve(usize::try_from(total).unwrap_or(0));
        }
    }
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| error.without_url().to_string())?
    {
        if stream.cancelled.load(Ordering::SeqCst) {
            return Ok(());
        }
        stream
            .data
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .bytes
            .extend_from_slice(&chunk);
        stream.arrived.notify_all();
    }
    Ok(())
}

/// One decoder's view of a stream: its own position in the shared bytes.
struct StreamReader {
    handle: Arc<StreamHandle>,
    position: u64,
}

impl StreamReader {
    fn failure(message: &str) -> std::io::Error {
        std::io::Error::other(message.to_string())
    }

    /// Waits until `enough` says the stream has what is needed, the download
    /// ends, or it has stalled for too long.
    fn wait<'a>(
        &'a self,
        enough: impl Fn(&StreamData) -> bool,
    ) -> std::io::Result<std::sync::MutexGuard<'a, StreamData>> {
        let stream = &self.handle.stream;
        let mut data = stream.data.lock().unwrap_or_else(|lock| lock.into_inner());
        let mut waited_from = Instant::now();
        let mut seen = data.bytes.len();
        loop {
            if let Some(error) = &data.error {
                return Err(Self::failure(error));
            }
            if enough(&data) || data.done {
                return Ok(data);
            }
            if stream.cancelled.load(Ordering::SeqCst) {
                return Err(Self::failure("the stream was cancelled"));
            }
            let limit = if seen == 0 {
                STREAM_START_TIMEOUT
            } else {
                STREAM_STALL_TIMEOUT
            };
            if waited_from.elapsed() > limit {
                return Err(Self::failure("the server stopped sending this song"));
            }
            data = stream
                .arrived
                .wait_timeout(data, Duration::from_millis(250))
                .unwrap_or_else(|lock| lock.into_inner())
                .0;
            if data.bytes.len() != seen {
                seen = data.bytes.len();
                waited_from = Instant::now();
            }
        }
    }
}

impl Read for StreamReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let position = self.position;
        let data = self.wait(|data| data.bytes.len() as u64 > position)?;
        let available = &data.bytes[(position as usize).min(data.bytes.len())..];
        let count = available.len().min(out.len());
        out[..count].copy_from_slice(&available[..count]);
        drop(data);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let target = match to {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => {
                // The end is known from the announced length, or once the
                // whole song has arrived.
                let data = self.wait(|data| data.total.is_some())?;
                let end = data.total.unwrap_or(data.bytes.len() as u64);
                end.checked_add_signed(delta)
            }
        };
        self.position =
            target.ok_or_else(|| Self::failure("a seek before the start of the song"))?;
        Ok(self.position)
    }
}

impl MediaSource for StreamReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        let data = self
            .handle
            .stream
            .data
            .lock()
            .unwrap_or_else(|lock| lock.into_inner());
        data.total
            .or_else(|| data.done.then_some(data.bytes.len() as u64))
    }
}

// ---- the decoder -------------------------------------------------------------

/// One song being decoded into the pipeline's format.
struct Decoding {
    playable: Playable,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    buffer: Option<SampleBuffer<f32>>,
    /// The converter to the pipeline's rate, for a source at another one.
    resampler: Option<Resampler>,
    source_rate: u32,
    /// Stereo frames decoded so far, at the source's rate, counted from the
    /// start of the song.
    frames: u64,
    /// The loudness correction applied to every sample.
    gain: f64,
    /// Keeps the download alive while this decoder reads from it.
    _stream: Arc<StreamHandle>,
}

impl Decoding {
    fn open(playable: Playable, handle: Arc<StreamHandle>, normalise: bool) -> Result<Self> {
        let reader = StreamReader {
            handle: Arc::clone(&handle),
            position: 0,
        };
        // A file has a length; the server's live conversion does not, and
        // its header promises no frames at all, which gapless trimming
        // cannot subtract an encoder delay from.
        let is_file = reader.wait(|data| !data.bytes.is_empty())?.total.is_some();
        let stream = MediaSourceStream::new(Box::new(reader), MediaSourceStreamOptions::default());
        let probed = symphonia::default::get_probe()
            .format(
                &Hint::new(),
                stream,
                &FormatOptions {
                    enable_gapless: is_file,
                    ..FormatOptions::default()
                },
                &MetadataOptions::default(),
            )
            .context("unrecognised audio format")?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("no audio in the stream"))?;
        let track_id = track.id;
        let source_rate = track
            .codec_params
            .sample_rate
            .ok_or_else(|| anyhow!("the stream does not state its sample rate"))?;
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .context("unsupported audio codec")?;
        let gain = match playable.gain_db.filter(|_| normalise) {
            Some(db) => 10f64.powf(f64::from(db.clamp(-24.0, 12.0)) / 20.0),
            None => 1.0,
        };
        Ok(Self {
            playable,
            format,
            decoder,
            track_id,
            buffer: None,
            resampler: Resampler::new(source_rate, SAMPLE_RATE, NUM_CHANNELS as usize),
            source_rate,
            frames: 0,
            gain,
            _stream: handle,
        })
    }

    /// The place in the song the decoder has reached.
    fn position_ms(&self) -> u32 {
        (self.frames.saturating_mul(1000) / u64::from(self.source_rate.max(1)))
            .try_into()
            .unwrap_or(u32::MAX)
    }

    /// The next stretch of sound in the pipeline's format, or `None` at the
    /// end of the song.
    fn next(&mut self) -> Result<Option<Vec<f64>>> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(DecodeError::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(DecodeError::ResetRequired) => return Ok(None),
                Err(error) => return Err(anyhow!("{error}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                // A damaged packet is a click, not the end of the song.
                Err(DecodeError::DecodeError(error)) => {
                    log::debug!("skipping a damaged packet: {error}");
                    continue;
                }
                Err(DecodeError::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(anyhow!("{error}")),
            };
            let spec = *decoded.spec();
            let channels = spec.channels.count().max(1);
            let frames = decoded.frames();
            if frames == 0 {
                continue;
            }
            let buffer = match &mut self.buffer {
                Some(buffer) if buffer.capacity() >= frames * channels => buffer,
                slot => slot.insert(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec)),
            };
            buffer.copy_interleaved_ref(decoded);
            let samples = buffer.samples();
            let stereo: Vec<f32> = match channels {
                1 => samples
                    .iter()
                    .flat_map(|sample| [*sample, *sample])
                    .collect(),
                2 => samples.to_vec(),
                // Front left and right carry the music of a surround mix
                // closely enough for stereo speakers.
                more => samples
                    .chunks_exact(more)
                    .flat_map(|frame| [frame[0], frame[1]])
                    .collect(),
            };
            self.frames += frames as u64;
            let converted = match &mut self.resampler {
                Some(resampler) => resampler.process(&stereo),
                None => stereo,
            };
            if converted.is_empty() {
                continue;
            }
            let gain = self.gain;
            return Ok(Some(
                converted
                    .into_iter()
                    .map(|sample| f64::from(sample) * gain)
                    .collect(),
            ));
        }
    }

    fn seek(&mut self, position_ms: u32) -> Result<u32> {
        let time = Time::new(
            u64::from(position_ms / 1000),
            f64::from(position_ms % 1000) / 1000.0,
        );
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|error| anyhow!("{error}"))?;
        self.decoder.reset();
        self.resampler = Resampler::new(self.source_rate, SAMPLE_RATE, NUM_CHANNELS as usize);
        // The container lands on a packet boundary at or before the target.
        let landed = self
            .format
            .tracks()
            .iter()
            .find(|track| track.id == self.track_id)
            .and_then(|track| track.codec_params.time_base)
            .map(|base| {
                let time = base.calc_time(seeked.actual_ts);
                time.seconds * 1000 + (time.frac * 1000.0) as u64
            })
            .unwrap_or(u64::from(position_ms));
        self.frames = landed * u64::from(self.source_rate) / 1000;
        Ok(u32::try_from(landed).unwrap_or(u32::MAX))
    }
}

/// What decoding the start of a song found, for diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodeProbe {
    /// The rate of the file or of the server's conversion.
    pub source_rate: u32,
    /// Stereo frames produced at the pipeline's rate.
    pub frames: usize,
    /// The loudest sample, to tell sound from silence.
    pub peak: f64,
    /// Where an accurate seek to the middle of what was asked for landed.
    pub seeked_to_ms: Option<u32>,
}

/// Streams a song from the server and decodes its first `seconds`, without
/// opening an audio output. `examples/jellyfin_probe.rs` runs it against a
/// real server.
pub fn decode_probe(
    client: Arc<ApiClient>,
    http: Http,
    runtime: tokio::runtime::Handle,
    playable: Playable,
    max_bitrate: Option<u32>,
    seconds: u32,
) -> Result<DecodeProbe> {
    let source = Source {
        client,
        http,
        runtime,
        max_bitrate,
        play_session: crate::auth::new_device_id(),
    };
    let handle = source.open(&playable)?;
    let mut decoding = Decoding::open(playable, handle, false)?;
    let wanted = seconds as usize * SAMPLE_RATE as usize;
    let mut frames = 0;
    let mut peak = 0f64;
    while frames < wanted {
        let Some(samples) = decoding.next()? else {
            break;
        };
        frames += samples.len() / NUM_CHANNELS as usize;
        peak = samples
            .iter()
            .fold(peak, |peak, sample| peak.max(sample.abs()));
    }
    let seeked_to_ms = decoding.seek(seconds * 500).ok();
    decoding.next()?;
    Ok(DecodeProbe {
        source_rate: decoding.source_rate,
        frames,
        peak,
        seeked_to_ms,
    })
}

// ---- the engine --------------------------------------------------------------

enum Message {
    Load(Box<Load>),
    Enqueue(Vec<Playable>),
    Restore(Box<PlaybackResume>),
    Toggle,
    Next,
    Previous,
    ClearQueue,
    Seek(u32),
    Shuffle(bool),
    Repeat(RepeatMode),
    Shutdown,
}

pub struct Engine {
    messages: mpsc::Sender<Message>,
    device_id: String,
    state: Arc<Mutex<LocalState>>,
    queue: Arc<Mutex<PlayQueue>>,
    volume: SoftVolume,
    audio: Arc<AudioControl>,
    notify: Notify,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Engine {
    /// Starts the player for a signed-in session. Nothing is fetched or
    /// opened until something is loaded.
    pub fn start(
        config: &EngineConfig,
        client: Arc<ApiClient>,
        http: Http,
        runtime: tokio::runtime::Handle,
        notify: Notify,
    ) -> Result<Self> {
        let state = Arc::new(Mutex::new(LocalState {
            volume: config.initial_volume,
            connected: true,
            username: client.session().username.clone(),
            ..LocalState::default()
        }));
        let queue = Arc::new(Mutex::new(PlayQueue::default()));
        let volume = SoftVolume::new(config.initial_volume);
        let audio = AudioControl::new(config.buffer_ms);
        let (messages, inbox) = mpsc::channel();
        let device_id = client.session().device_id.clone();
        let source = Source {
            play_session: crate::auth::new_device_id(),
            max_bitrate: config.max_bitrate(),
            client,
            http,
            runtime,
        };
        let player_state = Arc::clone(&state);
        let player_queue = Arc::clone(&queue);
        let player_notify = Arc::clone(&notify);
        let player_audio = Arc::clone(&audio);
        let player_volume = volume.clone();
        let config = config.clone();
        let thread = std::thread::Builder::new()
            .name("jellifast-player".into())
            .spawn(move || {
                let normalisation = Arc::new(AtomicU64::new(1.0f64.to_bits()));
                let report_state = Arc::clone(&player_state);
                let report_notify = Arc::clone(&player_notify);
                let report: ErrorHook = Arc::new(move |message: String| {
                    let snapshot = {
                        let mut current =
                            report_state.lock().unwrap_or_else(|lock| lock.into_inner());
                        current.error = Some(message);
                        current.clone()
                    };
                    report_notify(EngineEvent::State(snapshot));
                });
                // The output applies volume to queued audio; the tap reads
                // the same level to place the limiter's ceiling.
                let output = Box::new(RodioSink::new(
                    config.audio_device.clone(),
                    report,
                    Box::new(player_volume.clone()),
                    config.buffer_ms,
                    Arc::clone(&player_audio),
                ));
                let sink = Box::new(Tapped::new(
                    output,
                    Arc::clone(&config.tap),
                    Box::new(player_volume),
                    false,
                    Arc::clone(&config.eq),
                    Arc::clone(&normalisation),
                ));
                Player {
                    inbox,
                    sink,
                    converter: Converter,
                    state: player_state,
                    shared_queue: player_queue,
                    notify: player_notify,
                    audio: player_audio,
                    source,
                    queue: PlayQueue::default(),
                    current: None,
                    preloaded: None,
                    wanted: false,
                    sink_running: false,
                    normalise: config.normalisation,
                    gapless: config.gapless,
                    normalisation,
                    positioned_at: Instant::now(),
                    failures: 0,
                }
                .run();
            })
            .context("unable to start the player thread")?;
        notify(EngineEvent::State(
            state
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .clone(),
        ));
        Ok(Self {
            messages,
            device_id,
            state,
            queue,
            volume,
            audio,
            notify,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn state(&self) -> LocalState {
        self.state
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
    }

    /// The playing song and what follows it.
    pub fn queue(&self) -> Queue {
        self.queue
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .view()
    }

    /// What this engine is heard at, kept past the engine itself: the next
    /// engine starts there.
    pub(crate) fn heard(&self) -> Heard {
        Heard {
            state: Arc::clone(&self.state),
        }
    }

    pub fn shutdown(&self) {
        let _ = self.messages.send(Message::Shutdown);
        // A song waiting on the network must not hold shutdown up.
        self.audio.stopped();
        if let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .take()
        {
            let _ = thread.join();
        }
    }

    /// Everything to hand the engine that replaces this one.
    pub fn resume_point(&self) -> Option<PlaybackResume> {
        let state = self.state();
        let queue = self
            .queue
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone();
        queue.current.as_ref()?;
        if state.playback == Playback::Stopped {
            return None;
        }
        Some(PlaybackResume {
            queue,
            position_ms: state.position_now(),
            playing: matches!(state.playback, Playback::Playing | Playback::Loading),
        })
    }

    pub fn resume(&self, resume: PlaybackResume) -> Result<()> {
        self.send(Message::Restore(Box::new(resume)))
    }

    fn send(&self, message: Message) -> Result<()> {
        self.messages
            .send(message)
            .map_err(|_| anyhow!("the player has stopped"))
    }

    fn interrupt_if_playing(&self) {
        let playing = self
            .state
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .playback
            == Playback::Playing;
        if playing {
            self.audio.interrupt();
        }
    }

    /// Starts resolved songs. The sound on now fades out at once; the new
    /// song follows as soon as its first bytes arrive.
    pub fn load(&self, load: Load) -> Result<()> {
        if load.tracks.is_empty() {
            bail!("nothing to play");
        }
        self.interrupt_if_playing();
        self.send(Message::Load(Box::new(load)))
    }

    /// Queues resolved songs after the ones already queued.
    pub fn enqueue(&self, tracks: Vec<Playable>) -> Result<()> {
        self.send(Message::Enqueue(tracks))
    }

    fn note_volume(&self, volume: u16) {
        self.volume.set(volume);
        let snapshot = {
            let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
            if state.volume == volume {
                return;
            }
            state.volume = volume;
            state.clone()
        };
        (self.notify)(EngineEvent::State(snapshot));
    }

    /// Applies a command that needs nothing from the server. `Load` and
    /// `AddToQueue` name songs by URI; the backend resolves them and calls
    /// [`Engine::load`] and [`Engine::enqueue`].
    pub fn command(&self, command: PlayerCommand) -> Result<()> {
        match command {
            PlayerCommand::Toggle => self.send(Message::Toggle),
            PlayerCommand::Next => {
                self.interrupt_if_playing();
                self.send(Message::Next)
            }
            PlayerCommand::Previous => {
                self.interrupt_if_playing();
                self.send(Message::Previous)
            }
            PlayerCommand::ClearQueue => self.send(Message::ClearQueue),
            PlayerCommand::Seek(position_ms) => self.send(Message::Seek(position_ms)),
            PlayerCommand::Volume(volume) | PlayerCommand::VolumePreview(volume) => {
                self.note_volume(volume);
                Ok(())
            }
            PlayerCommand::Shuffle(enabled) => self.send(Message::Shuffle(enabled)),
            PlayerCommand::Repeat(mode) => self.send(Message::Repeat(mode)),
            PlayerCommand::Transfer => Ok(()),
            PlayerCommand::Load(_) | PlayerCommand::AddToQueue(_) => {
                bail!("songs must be resolved before they reach the player")
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.messages.send(Message::Shutdown);
    }
}

/// What an engine is heard at, for the engine that replaces it.
#[derive(Clone)]
pub(crate) struct Heard {
    state: Arc<Mutex<LocalState>>,
}

impl Heard {
    /// The level set last.
    pub(crate) fn level(&self) -> u16 {
        self.state
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .volume
    }
}

/// The player thread's side of the engine.
struct Player {
    inbox: mpsc::Receiver<Message>,
    sink: Box<dyn Sink>,
    converter: Converter,
    state: Arc<Mutex<LocalState>>,
    shared_queue: Arc<Mutex<PlayQueue>>,
    notify: Notify,
    audio: Arc<AudioControl>,
    source: Source,
    queue: PlayQueue,
    current: Option<Decoding>,
    /// The download of the song that follows, started ahead of its turn.
    preloaded: Option<(String, Arc<StreamHandle>)>,
    /// Play, as opposed to pause.
    wanted: bool,
    sink_running: bool,
    normalise: bool,
    gapless: bool,
    /// The gain applied to the playing song, for the visualisers to undo.
    normalisation: Arc<AtomicU64>,
    positioned_at: Instant,
    failures: u32,
}

impl Player {
    fn run(mut self) {
        loop {
            let active = self.wanted && self.current.is_some();
            let message = if active {
                match self.inbox.try_recv() {
                    Ok(message) => Some(message),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            } else {
                match self.inbox.recv() {
                    Ok(message) => Some(message),
                    Err(_) => break,
                }
            };
            match message {
                Some(Message::Shutdown) => break,
                Some(message) => self.handle(message),
                None => self.step(),
            }
        }
        self.stop_sink();
    }

    fn update(&self, change: impl FnOnce(&mut LocalState)) {
        let snapshot = {
            let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
            change(&mut state);
            state.clone()
        };
        (self.notify)(EngineEvent::State(snapshot));
    }

    fn publish_queue(&self) {
        *self
            .shared_queue
            .lock()
            .unwrap_or_else(|lock| lock.into_inner()) = self.queue.clone();
    }

    fn start_sink(&mut self) {
        if !self.sink_running {
            let _ = self.sink.start();
            self.sink_running = true;
        }
    }

    fn stop_sink(&mut self) {
        if self.sink_running {
            let _ = self.sink.stop();
            self.sink_running = false;
        }
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Load(load) => {
                if let Some(shuffle) = load.shuffle {
                    self.queue.shuffle = shuffle;
                }
                if let Some(repeat) = load.repeat {
                    self.queue.repeat = repeat;
                }
                self.queue.load(load.context_uri, load.tracks, load.start);
                self.wanted = load.play;
                self.failures = 0;
                self.open_current(load.position_ms);
            }
            Message::Restore(resume) => {
                self.queue = resume.queue;
                self.wanted = resume.playing;
                self.failures = 0;
                self.open_current(resume.position_ms);
            }
            Message::Enqueue(tracks) => {
                self.queue.manual.extend(tracks);
                self.preloaded = None;
                self.publish_queue();
            }
            Message::ClearQueue => {
                self.queue.manual.clear();
                self.preloaded = None;
                self.publish_queue();
            }
            Message::Toggle => {
                if self.current.is_none() {
                    // Stopped at the end of a list: Play starts it over.
                    if self.queue.current.is_some() {
                        self.wanted = true;
                        self.open_current(0);
                    }
                } else if self.wanted {
                    self.pause();
                } else {
                    self.wanted = true;
                    self.start_sink();
                    let position = self.current.as_ref().map_or(0, Decoding::position_ms);
                    self.positioned_at = Instant::now();
                    self.update(|state| {
                        state.playback = Playback::Playing;
                        state.position_ms = position;
                        state.position_at = Some(Instant::now());
                    });
                }
            }
            Message::Next => {
                if self.queue.advance(false).is_some() {
                    self.wanted = true;
                    self.failures = 0;
                    self.open_current(0);
                } else {
                    self.finish();
                }
            }
            Message::Previous => {
                let early = self
                    .state
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .position_now()
                    < RESTART_THRESHOLD_MS;
                let went_back = early && self.queue.retreat().is_some();
                if went_back || self.current.is_none() {
                    self.wanted = true;
                    self.failures = 0;
                    self.open_current(0);
                } else {
                    // The interrupt that came with Previous emptied the
                    // output; the same song starts over.
                    self.audio.track_changed();
                    self.wanted = true;
                    self.seek(0);
                    self.start_sink();
                    self.update(|state| {
                        state.playback = Playback::Playing;
                        state.position_at = Some(Instant::now());
                    });
                }
            }
            Message::Seek(position_ms) => self.seek(position_ms),
            Message::Shuffle(shuffle) => {
                self.queue.set_shuffle(shuffle);
                self.preloaded = None;
                self.publish_queue();
                self.update(|state| state.shuffle = shuffle);
            }
            Message::Repeat(repeat) => {
                self.queue.repeat = repeat;
                self.preloaded = None;
                self.publish_queue();
                self.update(|state| state.repeat = repeat);
            }
            Message::Shutdown => {}
        }
    }

    fn pause(&mut self) {
        self.wanted = false;
        // The output plays its queue out as it fades, so the decoder's place
        // is where the listener stopped.
        self.stop_sink();
        let position = self.current.as_ref().map_or(0, Decoding::position_ms);
        self.update(|state| {
            state.playback = Playback::Paused;
            state.position_ms = position;
            state.position_at = None;
        });
    }

    fn seek(&mut self, position_ms: u32) {
        let Some(current) = &mut self.current else {
            return;
        };
        let duration = current.playable.track.duration_ms;
        let target = if duration > 0 {
            position_ms.min(duration.saturating_sub(250))
        } else {
            position_ms
        };
        // The place asked for shows at once and holds still: reaching it may
        // wait for that part of the song to arrive.
        {
            let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
            state.position_ms = target;
            state.position_at = None;
            let snapshot = state.clone();
            drop(state);
            (self.notify)(EngineEvent::State(snapshot));
        }
        match current.seek(target) {
            Ok(landed) => {
                self.audio.seeked();
                self.positioned_at = Instant::now();
                let playing = self.wanted;
                self.update(|state| {
                    state.position_ms = landed;
                    state.position_at = playing.then(Instant::now);
                    state.seek_sequence = state.seek_sequence.wrapping_add(1);
                });
            }
            Err(error) => {
                log::warn!("seek failed: {error:#}");
                // Playback carries on from where the decoder still is.
                let position = self.current.as_ref().map_or(0, Decoding::position_ms);
                let playing = self.wanted;
                self.update(|state| {
                    state.position_ms = position;
                    state.position_at = playing.then(Instant::now);
                });
            }
        }
    }

    /// Opens the queue's current song at `position_ms` and, when playback is
    /// wanted, starts it.
    fn open_current(&mut self, position_ms: u32) {
        self.current = None;
        let Some(playable) = self.queue.current.clone() else {
            self.finish();
            return;
        };
        self.publish_queue();
        let track = LocalTrack::of(&playable.track);
        let shuffle = self.queue.shuffle;
        let repeat = self.queue.repeat;
        self.update(|state| {
            state.replay_pending = state
                .track
                .as_ref()
                .is_some_and(|previous| previous.uri == track.uri);
            state.track = Some(track);
            state.track_sequence = state.track_sequence.wrapping_add(1);
            state.playback = Playback::Loading;
            state.position_ms = position_ms;
            state.position_at = None;
            state.shuffle = shuffle;
            state.repeat = repeat;
            state.error = None;
        });
        let opened = self.take_stream(&playable).and_then(|handle| {
            let mut decoding = Decoding::open(playable.clone(), handle, self.normalise)?;
            if position_ms > 0 {
                decoding.seek(position_ms)?;
            }
            Ok(decoding)
        });
        // Whatever the outcome, the gate a skip closed opens again.
        self.audio.track_changed();
        match opened {
            Ok(decoding) => {
                self.normalisation
                    .store(decoding.gain.to_bits(), Ordering::Relaxed);
                let position = decoding.position_ms();
                self.current = Some(decoding);
                self.failures = 0;
                self.positioned_at = Instant::now();
                let playing = self.wanted;
                if playing {
                    self.start_sink();
                } else {
                    self.stop_sink();
                }
                self.update(|state| {
                    state.playback = if playing {
                        Playback::Playing
                    } else {
                        Playback::Paused
                    };
                    state.position_ms = position;
                    state.position_at = playing.then(Instant::now);
                    if std::mem::take(&mut state.replay_pending) {
                        state.seek_sequence = state.seek_sequence.wrapping_add(1);
                    }
                });
            }
            Err(error) => self.failed(&playable, error),
        }
    }

    /// The stream for `playable`: the one started ahead of time, or a new one.
    fn take_stream(&mut self, playable: &Playable) -> Result<Arc<StreamHandle>> {
        match self.preloaded.take() {
            Some((uri, handle)) if uri == playable.track.uri => Ok(handle),
            _ => self.source.open(playable),
        }
    }

    fn failed(&mut self, playable: &Playable, error: anyhow::Error) {
        log::warn!("unable to play {}: {error:#}", playable.track.uri);
        self.failures += 1;
        let message = format!("Couldn't play “{}”: {error}", playable.track.name);
        self.update(|state| {
            state.error = Some(message);
            state.replay_pending = false;
        });
        // One missing song must not stop an album; a server that is gone
        // must not burn through the whole queue.
        if self.failures < MAX_FAILURES && self.wanted && self.queue.advance(false).is_some() {
            self.open_current(0);
        } else {
            self.finish();
        }
    }

    /// The queue has nothing more: the output plays out and playback stops.
    fn finish(&mut self) {
        self.current = None;
        self.preloaded = None;
        self.wanted = false;
        self.audio.stopped();
        self.stop_sink();
        self.publish_queue();
        self.update(|state| {
            state.playback = Playback::Stopped;
            state.position_ms = 0;
            state.position_at = None;
        });
    }

    /// Decodes one stretch of the playing song into the output.
    fn step(&mut self) {
        let Some(current) = &mut self.current else {
            return;
        };
        match current.next() {
            Ok(Some(samples)) => {
                if let Err(error) = self
                    .sink
                    .write(AudioPacket::Samples(samples), &mut self.converter)
                {
                    // The output reported itself; playback waits for the
                    // listener to fix it and press Play.
                    log::warn!("audio output: {error}");
                    self.pause();
                    return;
                }
                self.correct_position();
                self.preload_next();
            }
            Ok(None) => self.ended(),
            Err(error) => {
                let playable = current.playable.clone();
                self.failed(&playable, error);
            }
        }
    }

    /// Tells the interface where the listener is: where the decoder is, less
    /// what still waits in the output.
    fn correct_position(&mut self) {
        if self.positioned_at.elapsed() < POSITION_INTERVAL {
            return;
        }
        self.positioned_at = Instant::now();
        let Some(current) = &self.current else {
            return;
        };
        let buffered = u32::try_from(self.audio.buffered().as_millis()).unwrap_or(0);
        let position = current.position_ms().saturating_sub(buffered);
        self.update(|state| {
            state.position_ms = position;
            state.position_at = Some(Instant::now());
        });
    }

    /// Starts the following song's download near the end of this one, so the
    /// change needs no wait.
    fn preload_next(&mut self) {
        if self.preloaded.is_some() {
            return;
        }
        let Some(current) = &self.current else {
            return;
        };
        let duration = current.playable.track.duration_ms;
        if duration == 0 || current.position_ms().saturating_add(PRELOAD_BEFORE_END_MS) < duration {
            return;
        }
        let Some(next) = self.queue.peek().cloned() else {
            return;
        };
        if next.track.uri == current.playable.track.uri {
            return;
        }
        match self.source.open(&next) {
            Ok(handle) => self.preloaded = Some((next.track.uri.clone(), handle)),
            Err(error) => log::debug!("unable to preload the next song: {error:#}"),
        }
    }

    /// The playing song ran out.
    fn ended(&mut self) {
        if self.queue.advance(true).is_none() {
            self.finish();
            return;
        }
        if !self.gapless {
            // Without gapless playback each song fades out and in.
            self.stop_sink();
        }
        self.open_current(0);
    }
}

// ---- the playlist tree ---------------------------------------------------------

/// The order and grouping of the sidebar's playlists, as the session file
/// remembers it. Jellyfin keeps playlists in one flat list, so the app only
/// ever builds this from what the listener arranged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rootlist {
    pub entries: Vec<RootlistEntry>,
    /// Playlists the account may add songs to, by URI.
    pub editable: std::collections::BTreeSet<String>,
}

/// One row of the playlist tree.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RootlistEntry {
    /// A playlist, by its URI.
    Playlist(String),
    /// A folder opens; everything until its end sits inside it.
    FolderStart {
        id: String,
        name: String,
    },
    FolderEnd,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(name: &str) -> Playable {
        Playable {
            track: Track {
                id: Some(name.into()),
                name: name.into(),
                uri: format!("jellyfin:track:{name}"),
                duration_ms: 180_000,
                ..Track::default()
            },
            gain_db: None,
        }
    }

    fn songs(names: &[&str]) -> Vec<Playable> {
        names.iter().map(|name| song(name)).collect()
    }

    fn name(playable: Option<Playable>) -> Option<String> {
        playable.map(|playable| playable.track.name)
    }

    fn names(queue: &PlayQueue) -> Vec<String> {
        queue
            .upcoming()
            .iter()
            .map(|item| item.name().to_string())
            .collect()
    }

    #[test]
    fn a_context_plays_from_the_chosen_song_to_its_end() {
        let mut queue = PlayQueue::default();
        queue.load(None, songs(&["a", "b", "c"]), 1);
        assert_eq!(name(queue.current.clone()).as_deref(), Some("b"));
        assert_eq!(names(&queue), ["c"]);
        assert_eq!(name(queue.advance(true)).as_deref(), Some("c"));
        assert_eq!(queue.advance(true), None);
    }

    #[test]
    fn queued_songs_play_first_and_the_context_continues_after() {
        let mut queue = PlayQueue::default();
        queue.load(None, songs(&["a", "b", "c"]), 0);
        queue.manual.extend(songs(&["x", "y"]));
        assert_eq!(names(&queue), ["x", "y", "b", "c"]);
        assert_eq!(name(queue.peek().cloned()).as_deref(), Some("x"));
        assert_eq!(name(queue.advance(false)).as_deref(), Some("x"));
        assert_eq!(name(queue.advance(true)).as_deref(), Some("y"));
        assert_eq!(name(queue.advance(true)).as_deref(), Some("b"));
        assert_eq!(names(&queue), ["c"]);
    }

    #[test]
    fn previous_from_a_queued_song_returns_to_the_context_song() {
        let mut queue = PlayQueue::default();
        queue.load(None, songs(&["a", "b"]), 0);
        queue.manual.extend(songs(&["x"]));
        queue.advance(false);
        assert_eq!(name(queue.retreat()).as_deref(), Some("a"));
        assert_eq!(queue.retreat(), None, "the first song starts over");
    }

    #[test]
    fn repeat_one_replays_a_song_that_ran_out_but_not_a_skipped_one() {
        let mut queue = PlayQueue::default();
        queue.load(None, songs(&["a", "b"]), 0);
        queue.repeat = RepeatMode::Track;
        assert_eq!(name(queue.advance(true)).as_deref(), Some("a"));
        assert_eq!(name(queue.advance(false)).as_deref(), Some("b"));
    }

    #[test]
    fn repeat_all_wraps_in_both_directions() {
        let mut queue = PlayQueue::default();
        queue.load(None, songs(&["a", "b"]), 1);
        queue.repeat = RepeatMode::Context;
        assert_eq!(name(queue.peek().cloned()).as_deref(), Some("a"));
        assert_eq!(name(queue.advance(true)).as_deref(), Some("a"));
        assert_eq!(name(queue.retreat()).as_deref(), Some("b"));
    }

    #[test]
    fn shuffle_keeps_the_playing_song_and_plays_every_other_once() {
        let mut queue = PlayQueue::default();
        queue.load(None, songs(&["a", "b", "c", "d", "e"]), 2);
        queue.set_shuffle(true);
        assert_eq!(name(queue.current.clone()).as_deref(), Some("c"));
        let mut rest = names(&queue);
        rest.sort();
        assert_eq!(rest, ["a", "b", "d", "e"]);
        // Turning shuffle off continues in listed order from the same song.
        queue.set_shuffle(false);
        assert_eq!(names(&queue), ["d", "e"]);
    }

    #[test]
    fn a_shuffled_load_starts_with_the_chosen_song() {
        let mut queue = PlayQueue {
            shuffle: true,
            ..PlayQueue::default()
        };
        queue.load(None, songs(&["a", "b", "c", "d"]), 3);
        assert_eq!(name(queue.current.clone()).as_deref(), Some("d"));
        assert_eq!(names(&queue).len(), 3);
    }

    #[test]
    fn the_position_runs_on_while_playing_and_stops_at_the_end() {
        let state = LocalState {
            playback: Playback::Playing,
            track: Some(LocalTrack {
                duration_ms: 1_000,
                ..LocalTrack::default()
            }),
            position_ms: 990,
            position_at: Some(Instant::now() - Duration::from_secs(5)),
            ..LocalState::default()
        };
        assert_eq!(state.position_now(), 1_000);
        let paused = LocalState {
            playback: Playback::Paused,
            position_at: None,
            ..state
        };
        assert_eq!(paused.position_now(), 990);
    }

    #[test]
    fn a_reader_sees_bytes_as_they_arrive_and_the_end_once_done() {
        let stream = Arc::new(Stream {
            data: Mutex::new(StreamData {
                bytes: b"hello".to_vec(),
                total: Some(10),
                ..StreamData::default()
            }),
            arrived: Condvar::new(),
            cancelled: AtomicBool::new(false),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let task = runtime.spawn(async {}).abort_handle();
        let handle = Arc::new(StreamHandle {
            stream: Arc::clone(&stream),
            task,
        });
        let mut reader = StreamReader {
            handle,
            position: 0,
        };
        let mut out = [0u8; 8];
        assert_eq!(reader.read(&mut out).unwrap(), 5);
        assert_eq!(&out[..5], b"hello");
        assert_eq!(reader.byte_len(), Some(10));
        assert_eq!(reader.seek(SeekFrom::End(-4)).unwrap(), 6);
        let feeder = Arc::clone(&stream);
        let feeding = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let mut data = feeder.data.lock().unwrap();
            data.bytes.extend_from_slice(b"world");
            data.done = true;
            drop(data);
            feeder.arrived.notify_all();
        });
        assert_eq!(reader.read(&mut out).unwrap(), 4);
        assert_eq!(&out[..4], b"orld");
        assert_eq!(reader.read(&mut out).unwrap(), 0, "the end of the song");
        feeding.join().unwrap();
    }
}
