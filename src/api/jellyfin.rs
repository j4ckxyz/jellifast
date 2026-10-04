//! Jellyfin response shapes, and their translation into the app's models.
//!
//! Every field the server may omit or null is optional or defaulted, so an
//! answer from an older or newer server degrades to a blank field instead of
//! a failed page.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer};

use super::models::*;

/// Jellyfin counts time in 100-nanosecond ticks.
pub const TICKS_PER_MS: i64 = 10_000;

/// The scheme of the app's own resource names: `jellyfin:<kind>:<id>`.
pub const URI_SCHEME: &str = "jellyfin";

/// The context that plays every favourite song, as the app names it.
pub fn favourites_uri(user_id: &str) -> String {
    format!("{URI_SCHEME}:user:{user_id}:collection")
}

pub fn uri(kind: &str, id: &str) -> String {
    format!("{URI_SCHEME}:{kind}:{id}")
}

fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NameId {
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserData {
    #[serde(default, deserialize_with = "null_default")]
    pub is_favorite: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub play_count: u32,
    #[serde(default)]
    pub last_played_date: Option<String>,
}

/// `BaseItemDto`: a song, album, artist, playlist or genre.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    #[serde(default, deserialize_with = "null_default")]
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, rename = "Type", deserialize_with = "null_default")]
    pub kind: String,
    #[serde(default)]
    pub run_time_ticks: Option<i64>,
    #[serde(default)]
    pub cumulative_run_time_ticks: Option<i64>,
    #[serde(default)]
    pub index_number: Option<u32>,
    #[serde(default)]
    pub parent_index_number: Option<u32>,
    #[serde(default)]
    pub production_year: Option<i32>,
    #[serde(default)]
    pub premiere_date: Option<String>,
    #[serde(default)]
    pub date_created: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub album_primary_image_tag: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub album_artists: Vec<NameId>,
    #[serde(default, deserialize_with = "null_default")]
    pub artist_items: Vec<NameId>,
    #[serde(default, deserialize_with = "null_default")]
    pub artists: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub image_tags: HashMap<String, String>,
    #[serde(default, deserialize_with = "null_default")]
    pub genres: Vec<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub child_count: Option<u32>,
    #[serde(default)]
    pub song_count: Option<u32>,
    #[serde(default)]
    pub user_data: Option<UserData>,
    /// The entry's id within its playlist, which removal and moves address.
    #[serde(default)]
    pub playlist_item_id: Option<String>,
    #[serde(default)]
    pub media_type: Option<String>,
    /// The loudness correction the server measured for this song, in dB.
    #[serde(default)]
    pub normalization_gain: Option<f32>,
    #[serde(default)]
    pub has_lyrics: Option<bool>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub official_rating: Option<String>,
}

impl Item {
    pub fn duration_ms(&self) -> u32 {
        ticks_to_ms(self.run_time_ticks.unwrap_or(0))
    }

    pub fn is_favorite(&self) -> bool {
        self.user_data.as_ref().is_some_and(|data| data.is_favorite)
    }
}

pub fn ticks_to_ms(ticks: i64) -> u32 {
    u32::try_from(ticks.max(0) / TICKS_PER_MS).unwrap_or(u32::MAX)
}

pub fn ms_to_ticks(ms: u32) -> i64 {
    i64::from(ms) * TICKS_PER_MS
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Items {
    #[serde(default, deserialize_with = "null_default")]
    pub items: Vec<Item>,
    #[serde(default, deserialize_with = "null_default")]
    pub total_record_count: u32,
    #[serde(default, deserialize_with = "null_default")]
    pub start_index: u32,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserDto {
    #[serde(default, deserialize_with = "null_default")]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub primary_image_tag: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LyricLine {
    #[serde(default, deserialize_with = "null_default")]
    pub text: String,
    #[serde(default)]
    pub start: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LyricDto {
    #[serde(default, deserialize_with = "null_default")]
    pub lyrics: Vec<LyricLine>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlayState {
    #[serde(default)]
    pub position_ticks: Option<i64>,
    #[serde(default, deserialize_with = "null_default")]
    pub is_paused: bool,
    #[serde(default)]
    pub volume_level: Option<u8>,
    #[serde(default)]
    pub repeat_mode: Option<String>,
    #[serde(default)]
    pub playback_order: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SessionInfo {
    #[serde(default, deserialize_with = "null_default")]
    pub id: String,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub supports_remote_control: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub supports_media_control: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub playable_media_types: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub supported_commands: Vec<String>,
    #[serde(default)]
    pub play_state: Option<PlayState>,
    #[serde(default)]
    pub now_playing_item: Option<Item>,
}

/// The server's pictures of an item, as the three sizes the interface asks
/// for: a row thumbnail, a grid card, a page header.
pub fn images(server: &str, id: &str, tag: Option<&str>) -> Vec<Image> {
    let Some(tag) = tag.filter(|tag| !tag.is_empty()) else {
        return Vec::new();
    };
    if id.is_empty() {
        return Vec::new();
    }
    [640u32, 300, 64]
        .into_iter()
        .map(|size| Image {
            url: format!(
                "{server}/Items/{id}/Images/Primary?tag={tag}&fillWidth={size}&fillHeight={size}&quality=90"
            ),
            width: Some(size),
            height: Some(size),
        })
        .collect()
}

fn artist_refs(items: &[NameId], names: &[String]) -> Vec<ArtistRef> {
    if !items.is_empty() {
        return items
            .iter()
            .map(|artist| ArtistRef {
                uri: artist.id.as_deref().map(|id| uri("artist", id)),
                id: artist.id.clone(),
                name: artist.name.clone(),
            })
            .collect();
    }
    names
        .iter()
        .map(|name| ArtistRef {
            id: None,
            name: name.clone(),
            uri: None,
        })
        .collect()
}

fn release_date(item: &Item) -> Option<String> {
    item.premiere_date
        .as_deref()
        .and_then(|date| date.get(..10))
        .map(str::to_string)
        .or_else(|| item.production_year.map(|year| year.to_string()))
}

pub fn track(server: &str, item: &Item) -> Track {
    let album = item.album_id.as_deref().map(|album_id| Album {
        id: album_id.to_string(),
        name: item.album.clone().unwrap_or_default(),
        uri: uri("album", album_id),
        album_type: Some("album".into()),
        images: images(server, album_id, item.album_primary_image_tag.as_deref()),
        artists: artist_refs(
            &item.album_artists,
            &item.album_artist.clone().into_iter().collect::<Vec<_>>(),
        ),
        release_date: release_date(item),
        ..Album::default()
    });
    // A song outside any album, or in one without a cover, shows its own.
    let album = match album {
        Some(mut album) => {
            if album.images.is_empty() {
                album.images = images(
                    server,
                    &item.id,
                    item.image_tags.get("Primary").map(String::as_str),
                );
            }
            Some(album)
        }
        None => {
            let own = images(
                server,
                &item.id,
                item.image_tags.get("Primary").map(String::as_str),
            );
            (!own.is_empty() || item.album.is_some()).then(|| Album {
                name: item.album.clone().unwrap_or_default(),
                images: own,
                ..Album::default()
            })
        }
    };
    let mut artists = artist_refs(&item.artist_items, &item.artists);
    if artists.is_empty() {
        artists = artist_refs(
            &item.album_artists,
            &item.album_artist.clone().into_iter().collect::<Vec<_>>(),
        );
    }
    Track {
        id: Some(item.id.clone()),
        name: item.name.clone(),
        uri: uri("track", &item.id),
        duration_ms: item.duration_ms(),
        explicit: false,
        artists,
        album,
        track_number: item.index_number,
        disc_number: item.parent_index_number,
        is_playable: Some(true),
        popularity: None,
        ..Track::default()
    }
}

pub fn album(server: &str, item: &Item) -> Album {
    Album {
        id: item.id.clone(),
        name: item.name.clone(),
        uri: uri("album", &item.id),
        album_type: Some("album".into()),
        total_tracks: item.song_count.or(item.child_count),
        images: images(
            server,
            &item.id,
            item.image_tags.get("Primary").map(String::as_str),
        ),
        artists: artist_refs(
            &item.album_artists,
            &item.album_artist.clone().into_iter().collect::<Vec<_>>(),
        ),
        release_date: release_date(item),
        genres: item.genres.clone(),
        ..Album::default()
    }
}

pub fn artist(server: &str, item: &Item) -> Artist {
    Artist {
        id: item.id.clone(),
        name: item.name.clone(),
        uri: uri("artist", &item.id),
        images: images(
            server,
            &item.id,
            item.image_tags.get("Primary").map(String::as_str),
        ),
        genres: item.genres.clone(),
        ..Artist::default()
    }
}

/// A playlist as its owner sees it. Jellyfin lists the playlists a user may
/// open; every one of them is presented as that user's.
pub fn playlist(server: &str, item: &Item, owner: &Owner) -> Playlist {
    let total = item.child_count.unwrap_or(0);
    Playlist {
        id: item.id.clone(),
        name: item.name.clone(),
        uri: uri("playlist", &item.id),
        description: item.overview.clone().filter(|text| !text.is_empty()),
        images: images(
            server,
            &item.id,
            item.image_tags.get("Primary").map(String::as_str),
        ),
        owner: owner.clone(),
        public: None,
        collaborative: false,
        snapshot_id: snapshot(item),
        tracks: Some(TrackCount { total }),
        items_count: Some(TrackCount { total }),
    }
}

/// What changes whenever the playlist does: the server's revision tag and
/// the number of entries.
pub fn snapshot(item: &Item) -> Option<String> {
    let etag = item.etag.as_deref().filter(|etag| !etag.is_empty())?;
    Some(format!("{etag}-{}", item.child_count.unwrap_or(0)))
}

pub fn playlist_item(server: &str, item: &Item, user_id: &str) -> PlaylistItem {
    PlaylistItem {
        added_at: item.date_created.clone(),
        added_by: Some(UserRef {
            id: Some(user_id.to_string()),
        }),
        is_local: false,
        item: Some(PlayableItem::Track(track(server, item))),
        track: None,
    }
}

pub fn user(server: &str, dto: &UserDto) -> User {
    let images = match dto.primary_image_tag.as_deref() {
        Some(tag) if !tag.is_empty() => vec![Image {
            url: format!(
                "{server}/Users/{}/Images/Primary?tag={tag}&quality=90",
                dto.id
            ),
            width: None,
            height: None,
        }],
        _ => Vec::new(),
    };
    User {
        id: dto.id.clone(),
        display_name: dto.name.clone(),
        images,
        uri: Some(uri("user", &dto.id)),
    }
}

pub fn lyrics(dto: &LyricDto) -> Option<crate::lyrics::Lyrics> {
    let lines: Vec<crate::lyrics::Line> = dto
        .lyrics
        .iter()
        .filter_map(|line| {
            let text = line.text.trim();
            (!text.is_empty()).then(|| crate::lyrics::Line {
                at_ms: line.start.map(ticks_to_ms),
                text: text.to_string(),
            })
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    let synced = lines.iter().all(|line| line.at_ms.is_some());
    let lines = if synced {
        lines
    } else {
        lines
            .into_iter()
            .map(|line| crate::lyrics::Line {
                at_ms: None,
                ..line
            })
            .collect()
    };
    Some(crate::lyrics::Lyrics {
        lines,
        synced,
        instrumental: false,
    })
}

/// A page of `items` out of `total`, starting at `offset`.
pub fn page<T>(items: Vec<T>, total: u32, offset: u32, limit: u32) -> Page<T> {
    let end = offset.saturating_add(items.len() as u32);
    Page {
        next: (end < total && !items.is_empty()).then(|| "more".to_string()),
        items,
        total,
        limit,
        offset,
    }
}

pub fn device(session: &SessionInfo) -> Device {
    let state = session.play_state.as_ref();
    Device {
        id: Some(session.id.clone()),
        name: match (&session.device_name, &session.client) {
            (Some(device), Some(client)) => format!("{device} ({client})"),
            (Some(device), None) => device.clone(),
            (None, Some(client)) => client.clone(),
            (None, None) => "Jellyfin client".into(),
        },
        is_active: session.now_playing_item.is_some(),
        is_restricted: false,
        volume_percent: state.and_then(|state| state.volume_level),
        supports_volume: Some(
            session
                .supported_commands
                .iter()
                .any(|command| command == "SetVolume"),
        ),
        kind: "Computer".into(),
    }
}

pub fn playback_state(server: &str, session: &SessionInfo) -> Option<PlaybackState> {
    let item = session.now_playing_item.as_ref()?;
    if item
        .media_type
        .as_deref()
        .is_some_and(|kind| kind != "Audio")
    {
        return None;
    }
    let state = session.play_state.clone().unwrap_or_default();
    Some(PlaybackState {
        device: Some(device(session)),
        repeat_state: match state.repeat_mode.as_deref() {
            Some("RepeatAll") => "context".into(),
            Some("RepeatOne") => "track".into(),
            _ => "off".into(),
        },
        shuffle_state: state.playback_order.as_deref() == Some("Shuffle"),
        context: None,
        timestamp: 0,
        progress_ms: state.position_ticks.map(ticks_to_ms),
        is_playing: !state.is_paused,
        item: Some(PlayableItem::Track(track(server, item))),
        currently_playing_type: Some("track".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "http://nas.local:8096";

    fn song() -> Item {
        serde_json::from_str(
            r#"{
                "Name": "Bend", "Id": "a794", "Type": "Audio",
                "RunTimeTicks": 2675461220, "ProductionYear": 2006, "IndexNumber": 1,
                "ParentIndexNumber": null, "Genres": null,
                "PremiereDate": "2006-01-01T00:00:00.0000000Z",
                "UserData": {"PlayCount": 15, "IsFavorite": true},
                "Artists": ["Binaerpilot"],
                "ArtistItems": [{"Name": "Binaerpilot", "Id": "9ebc"}],
                "Album": "You Can't Stop Da Funk", "AlbumId": "f21e",
                "AlbumPrimaryImageTag": "95b1",
                "AlbumArtists": [{"Name": "Binaerpilot", "Id": "9ebc"}],
                "ImageTags": {}, "NormalizationGain": -6.6
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn a_song_becomes_a_track_with_its_album_cover_and_artists() {
        let track = track(SERVER, &song());
        assert_eq!(track.uri, "jellyfin:track:a794");
        assert_eq!(track.duration_ms, 267_546);
        assert_eq!(track.track_number, Some(1));
        assert_eq!(
            track.artists[0].uri.as_deref(),
            Some("jellyfin:artist:9ebc")
        );
        let album = track.album.unwrap();
        assert_eq!(album.uri, "jellyfin:album:f21e");
        assert_eq!(album.release_date.as_deref(), Some("2006-01-01"));
        assert_eq!(
            pick_image(&album.images, 64),
            Some(
                "http://nas.local:8096/Items/f21e/Images/Primary?tag=95b1&fillWidth=64&fillHeight=64&quality=90"
            )
        );
        assert_eq!(
            pick_image(&album.images, 300)
                .unwrap()
                .matches("300")
                .count(),
            2
        );
        assert!(song().is_favorite());
    }

    #[test]
    fn a_song_without_an_album_keeps_its_own_picture() {
        let item: Item = serde_json::from_str(
            r#"{"Name": "Loose", "Id": "t1", "Type": "Audio", "ImageTags": {"Primary": "tag"}}"#,
        )
        .unwrap();
        let track = track(SERVER, &item);
        assert!(
            track
                .image(64)
                .unwrap()
                .contains("/Items/t1/Images/Primary")
        );
        assert!(track.artists.is_empty());
    }

    #[test]
    fn pages_know_whether_more_follows() {
        let first = page(vec![1, 2], 5, 0, 2);
        assert_eq!(first.next_offset(), Some(2));
        let last = page(vec![5], 5, 4, 2);
        assert_eq!(last.next_offset(), None);
        let empty: Page<u8> = page(vec![], 5, 2, 2);
        assert_eq!(empty.next_offset(), None);
    }

    #[test]
    fn lyrics_are_synced_only_when_every_line_has_a_time() {
        let synced: LyricDto = serde_json::from_str(
            r#"{"Lyrics": [{"Text": "one", "Start": 10000000}, {"Text": " two ", "Start": 25000000}]}"#,
        )
        .unwrap();
        let found = lyrics(&synced).unwrap();
        assert!(found.synced);
        assert_eq!(found.lines[1].at_ms, Some(2500));
        assert_eq!(found.lines[1].text, "two");
        let plain: LyricDto =
            serde_json::from_str(r#"{"Lyrics": [{"Text": "one", "Start": 1}, {"Text": "two"}]}"#)
                .unwrap();
        let found = lyrics(&plain).unwrap();
        assert!(!found.synced);
        assert!(found.lines.iter().all(|line| line.at_ms.is_none()));
        assert!(lyrics(&LyricDto::default()).is_none());
    }
}
