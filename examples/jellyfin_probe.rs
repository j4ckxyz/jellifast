//! Signs in to a Jellyfin server and exercises what the app asks of it:
//! the library, search, a playlist, a song's stream and its decoding.
//!
//! ```sh
//! JELLYFIN_URL=https://demo.jellyfin.org/stable JELLYFIN_USER=demo JELLYFIN_PASSWORD= \
//!     cargo run --no-default-features --example jellyfin_probe
//! ```
//!
//! It reads only: nothing on the server changes, apart from the play it
//! reports with `--play`. That flag also runs the real player for a few
//! seconds with the volume at zero, through the default audio output.

use std::sync::Arc;

use jellifast::api::{ApiClient, NetActivity, PlayRequest};
use jellifast::auth::{self, Login};
use jellifast::http::Http;

fn main() -> anyhow::Result<()> {
    let login = Login {
        server: std::env::var("JELLYFIN_URL")
            .unwrap_or_else(|_| "https://demo.jellyfin.org/stable".into()),
        username: std::env::var("JELLYFIN_USER").unwrap_or_else(|_| "demo".into()),
        password: std::env::var("JELLYFIN_PASSWORD").unwrap_or_default(),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let http = Http::default();
    let session = runtime.block_on(auth::sign_in(
        &http.client().map_err(anyhow::Error::msg)?,
        &login,
        &auth::new_device_id(),
        "Jellifast probe",
    ))?;
    println!(
        "signed in to {} ({}) as {}",
        session.server_name, session.server, session.username
    );
    let client = Arc::new(ApiClient::new(
        http.clone(),
        Arc::new(NetActivity::default()),
        session.clone(),
        "Jellifast probe".into(),
    ));
    let playable = runtime.block_on(async {
        let me = client.me().await?;
        println!("account: {} ({})", me.name(), me.id);

        let albums = client.saved_albums(0, 5).await?;
        println!("albums: {} in the library", albums.total);
        for saved in &albums.items {
            println!(
                "  {} · {}",
                saved.album.name,
                saved
                    .album
                    .artists
                    .first()
                    .map_or("", |artist| &artist.name)
            );
        }
        let artists = client.followed_artists(None, 5).await?;
        println!("album artists: {:?}", artists.total);
        let favourites = client.saved_tracks(0, 5).await?;
        println!("favourite songs: {}", favourites.total);
        let latest = client.latest_albums(5).await?;
        println!("recently added: {}", latest.len());
        let recent = client.recently_played(5, None).await?;
        println!("recently played: {:?}", recent.total);
        let top = client.top_tracks(5, 0).await?;
        println!("most played: {}", top.total);
        println!("top artists: {}", client.top_artists(5).await?.len());

        let playlists = client.my_playlists(0, 5).await?;
        println!("playlists: {}", playlists.total);
        if let Some(playlist) = playlists.items.first() {
            let items = client.playlist_items(&playlist.id, 0, 5).await?;
            println!(
                "  {}: {} songs, snapshot {:?}",
                playlist.name,
                items.total,
                client.playlist(&playlist.id).await?.snapshot_id
            );
            let context = client.context_tracks(&playlist.uri).await?;
            println!("  as a context: {} songs", context.len());
        }

        let album = albums
            .items
            .first()
            .ok_or_else(|| anyhow::anyhow!("the library has no albums"))?;
        let full = client.album(&album.album.id).await?;
        let tracks = full.tracks.clone().unwrap_or_default();
        println!("album {}: {} songs", full.name, tracks.total);
        let first = tracks
            .items
            .first()
            .ok_or_else(|| anyhow::anyhow!("the album has no songs"))?
            .clone();
        println!(
            "  first: {} · {} · {} ms · cover {}",
            first.name,
            first.artist_names(),
            first.duration_ms,
            first.image(300).is_some()
        );
        if let Some(artist) = first.artists.first().and_then(|artist| artist.id.clone()) {
            let artist_page = client.artist(&artist).await?;
            println!(
                "artist {}: {} top songs, {} albums, {} similar",
                artist_page.name,
                client.artist_top_tracks(&artist).await?.len(),
                client
                    .artist_albums(&artist, "album,single", 0, 20)
                    .await?
                    .total,
                client.related_artists(&artist).await?.len(),
            );
        }
        let word = first.name.split_whitespace().next().unwrap_or("a");
        let found = client.search(word, 5).await?;
        println!(
            "search {word:?}: {} songs, {} albums, {} artists, {} playlists",
            found.tracks.map_or(0, |page| page.total),
            found.albums.map_or(0, |page| page.total),
            found.artists.map_or(0, |page| page.total),
            found.playlists.map_or(0, |page| page.total),
        );
        let id = first.id.clone().unwrap_or_default();
        println!(
            "instant mix: {} songs",
            client.instant_mix(&id, 20).await?.len()
        );
        println!(
            "lyrics on the server: {}",
            client.lyrics(&id).await?.is_some()
        );
        println!(
            "favourite: {:?}",
            client.contains(std::slice::from_ref(&first.uri)).await?
        );
        println!("other players: {}", client.devices().await?.len());
        let (resolved, start) = client
            .resolve(&PlayRequest::context(full.uri.clone()).starting_at_uri(first.uri.clone()))
            .await?;
        println!(
            "album as a queue: {} songs from index {start}",
            resolved.len()
        );
        anyhow::Ok(client.playable(first))
    })?;

    for (label, bitrate) in [("original", None), ("128 kbps", Some(128_000))] {
        let probe = jellifast::player::decode_probe(
            Arc::clone(&client),
            http.clone(),
            runtime.handle().clone(),
            playable.clone(),
            bitrate,
            4,
        )?;
        println!("decoded {label}: {probe:?}");
    }
    if std::env::args().any(|argument| argument == "--play") {
        play(&runtime, &client, &http, &playable)?;
    }
    runtime.block_on(auth::sign_out(
        &http.client().map_err(anyhow::Error::msg)?,
        &session,
        "Jellifast probe",
    ));
    println!("signed out");
    Ok(())
}

/// Runs the engine as the app does: load an album, skip, seek, pause.
fn play(
    runtime: &tokio::runtime::Runtime,
    client: &Arc<ApiClient>,
    http: &Http,
    first: &jellifast::api::client::Playable,
) -> anyhow::Result<()> {
    use jellifast::player::{Engine, EngineConfig, EngineEvent, Load, PlayerCommand};
    use std::time::Duration;

    let config = EngineConfig {
        device_name: "Jellifast probe".into(),
        bitrate_kbps: 0,
        normalisation: true,
        autoplay: false,
        gapless: true,
        backend: None,
        audio_device: None,
        initial_volume: 0,
        volume_dir: std::env::temp_dir(),
        audio_cache_dir: None,
        audio_cache_limit: None,
        buffer_ms: 100,
        tap: jellifast::vis::AudioTap::new(),
        eq: jellifast::eq::shared(),
        proxy: jellifast::settings::ProxyConfig::Off,
    };
    let (states, seen) = std::sync::mpsc::channel();
    let engine = Engine::start(
        &config,
        Arc::clone(client),
        http.clone(),
        runtime.handle().clone(),
        Arc::new(move |event| {
            if let EngineEvent::State(state) = event {
                let _ = states.send(state);
            }
        }),
    )?;
    let show = |label: &str| {
        std::thread::sleep(Duration::from_millis(1500));
        let mut last = None;
        while let Ok(state) = seen.try_recv() {
            last = Some(state);
        }
        let state = last.unwrap_or_else(|| engine.state());
        println!(
            "{label}: {:?} {:?} at {} ms, error {:?}, queue {}",
            state.playback,
            state.track.as_ref().map(|track| track.title.clone()),
            state.position_now(),
            state.error,
            engine.queue().queue.len(),
        );
    };
    let tracks = runtime.block_on(
        client.context_tracks(
            &first
                .track
                .album
                .as_ref()
                .map(|album| album.uri.clone())
                .unwrap_or_else(|| first.track.uri.clone()),
        ),
    )?;
    // A second song, so Next has somewhere to go.
    let mut tracks = tracks;
    tracks.push(first.clone());
    engine.load(Load {
        context_uri: None,
        tracks,
        start: 0,
        position_ms: 0,
        play: true,
        shuffle: None,
        repeat: None,
    })?;
    show("loaded");
    engine.command(PlayerCommand::Seek(60_000))?;
    show("seeked to 60 s");
    engine.command(PlayerCommand::Toggle)?;
    show("paused");
    engine.command(PlayerCommand::Toggle)?;
    show("resumed");
    engine.command(PlayerCommand::Next)?;
    show("next");
    engine.command(PlayerCommand::Previous)?;
    show("previous");
    engine.shutdown();
    println!("engine stopped");
    Ok(())
}
