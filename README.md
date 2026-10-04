# Jellifast

A small, fast, native music player for [Jellyfin](https://jellyfin.org).
Sign in with your server's address, your user name and your password, and your
music library opens in an interface built like a streaming app: Home, search,
albums, artists, playlists, a queue, lyrics, an equalizer, and optional Winamp
skins and MilkDrop visuals.

Jellifast is a fork of [Spotifast](https://github.com/crmne/spotifast) by
Carmine Paolino, with Spotify taken out and a Jellyfin server put in its place.
The interface, the audio output, the visualisers and the desktop integration are
Spotifast's. It is not affiliated with the Jellyfin project or with Spotify.

## What works

- **Sign-in** with server address, user name and password. Only the access token
  the server returns is kept, in the system credential store.
- **Library**: every album and album artist, playlists, favourite songs
  ("Liked Songs"), recently added, recently played and most played.
- **Search** across songs, albums, artists and playlists.
- **Playback on this computer**: the original file where the player can decode
  it (MP3, FLAC, AAC, ALAC, Vorbis, WAV, AIFF), the server's MP3 conversion
  otherwise or under a bitrate limit. Queue, shuffle, repeat, seeking, gapless
  album playback and volume normalisation from the server's loudness data.
- **Playlists**: create, rename, describe, change the cover, add, remove,
  reorder, delete.
- **Favourites** for songs, albums, artists and playlists.
- **Instant Mix** as song, album, artist and playlist radio, and as what plays
  when a list runs out.
- **Lyrics** from the server, with [LRCLIB](https://lrclib.net) as the fallback.
- **Play reporting**, so play counts and "last played" on the server stay true.
- **Other players**: control another Jellyfin client signed in to the same
  server, or hand it what this computer is playing.

[What Jellyfin Offers](docs/_reference/what-jellyfin-offers.md) has the full
list and what is not there yet.

## Build

```sh
cargo run --release
```

Rust 1.98 is pinned in `rust-toolchain.toml`. The default build includes
MilkDrop, which needs CMake, a C++ compiler and libclang;
`cargo run --release --no-default-features` leaves it out. On Linux, install the
ALSA and D-Bus development packages first.

`cargo run --no-default-features --example jellyfin_probe` signs in to a server
and exercises what the app asks of it, without the interface. It uses Jellyfin's
public demo server unless `JELLYFIN_URL`, `JELLYFIN_USER` and
`JELLYFIN_PASSWORD` say otherwise.

## Documentation

- [Getting started](docs/_guide/getting-started.md)
- [Everyday use](docs/_guide/using-jellifast.md)
- [How it connects](docs/_reference/how-it-connects.md) and
  [Privacy](docs/_reference/privacy.md)
- [Settings and files](docs/_reference/settings-and-files.md)
- [Contributing](CONTRIBUTING.md)

## License

MIT. See [LICENSE](LICENSE).
