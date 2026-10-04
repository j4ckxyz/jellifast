---
title: What Jellyfin Offers
description: Which parts of Jellyfin Jellifast uses, and what is not there yet.
nav_order: 2
---

Jellifast began as a client for a streaming service. Each part of that
interface is now served by the nearest thing a Jellyfin server has. This page says which, and where
there is nothing behind a control yet.

## What maps to what

| In the interface | From Jellyfin |
| --- | --- |
| Liked Songs, the heart on a song | Favourite songs |
| Albums, Artists in Your Library | Every album and album artist in the music libraries |
| The heart on an album, artist or playlist | Favourite |
| Playlists | Playlists the account can open, without films |
| Home: Recently added | Albums by date added |
| Home: Recently played, the Recents tab | Songs by last played date, plus what this computer played |
| Home: Your top songs, Top Songs page | Songs by play count |
| Home: Your top artists | The artists of the most played songs |
| Home: Recommended for you | Instant Mix of a top song |
| Song, album, artist and playlist radio | Instant Mix |
| Autoplay when a list ends | Instant Mix of the last song |
| Artist page: popular songs | The artist's songs by play count |
| Artist page: discography, appears on | Albums by album artist, albums by contributing artist |
| Artist page: related artists | Similar artists |
| Lyrics | The server's lyrics, then LRCLIB |
| Connect to a device | Other Jellyfin sessions that accept remote control |
| Audio quality | Maximum streaming bitrate; Original sends files as they are |
| Normalize volume | The server's per-song loudness gain |

## Not there yet

- **Audiobooks and podcasts.** The Podcasts shelf is hidden and the podcast
  pages are unused.
- **Genres**, as a way to browse.
- **Music videos.**
- **More than one server**, Quick Connect and single sign-on plugins.
- **Offline listening** and a disk cache for audio. The Audio cache setting has
  no effect.
- **Being controlled** from another Jellyfin client. Jellifast controls others;
  it does not register itself as a remote-control target.
- **SyncPlay.**
- **Playback above 44.1 kHz or in more than two channels** without conversion.
- **Whose playlist it is.** Every playlist the account can open is shown as its
  own; the server refuses an edit the account may not make.
- **Turning play reporting off.**

## What Jellyfin does not have

- **Playlist folders and a custom playlist order.** The sidebar's custom order
  and pins are kept on this computer.
- **A date for when a song became a favourite.** Liked Songs sorts by the date
  the song was added to the library.
- **Followers, popularity and editorial playlists.**
