---
title: How It Connects
description: The sign-in, what is stored, and every request Jellifast makes.
nav_order: 1
---

## One server, one token

Jellifast talks to one Jellyfin server, the one named at sign-in. There is no
hosted service in between and no account other than the one on that server.

Signing in sends `POST /Users/AuthenticateByName` with the user name and
password. Before that, `GET /System/Info/Public` checks that the address
answers as a Jellyfin server; an address typed without a scheme is tried over
HTTPS and then HTTP. The server answers with an access token. Every later
request carries it in the `Authorization: MediaBrowser …` header, together with
the client's name, version, this computer's device name and a device id made
once per installation (`device-id` in the state directory). The token never
goes into an address, so it does not reach server or proxy access logs through
a URL.

If the server answers 401 to any request, the token is no longer valid:
Jellifast forgets it and shows the sign-in form. **Sign out** calls
`POST /Sessions/Logout` and deletes the stored token.

## What the client stores

- **The session** (server address, user id, user name, device id and access
  token) in the platform credential store: Secret Service on Linux, Keychain on
  macOS, Credential Manager on Windows. The password is never stored.
- **The server address and user name** in the settings file, to fill the
  sign-in form.
- **Artwork** in the cache directory, within the budget you set.
- **Lyrics** from LRCLIB in the cache directory, for a month.
- **Favourite songs and playlist rows** as metadata in the cache directory,
  scoped to the account, so they show before the server answers.
- **Listening history** made on this computer, in the state directory.

A song is fetched into memory while it plays and is not written to disk. The
**Audio cache** setting inherited from the interface has no effect yet.

## Requests to the server

| What | Request |
| --- | --- |
| Account | `GET /Users/Me` |
| Albums, songs, playlists, favourites, recently and most played, search | `GET /Items` with filters and sort orders |
| Album artists, artist search | `GET /Artists/AlbumArtists`, `GET /Artists` |
| One album, artist, playlist or song | `GET /Items/{id}` |
| Similar artists | `GET /Artists/{id}/Similar` |
| Instant Mix (radio, autoplay, recommendations) | `GET /Items/{id}/InstantMix` |
| Playlist rows | `GET /Playlists/{id}/Items` |
| Playlist edits | `POST /Playlists`, `POST`/`DELETE /Playlists/{id}/Items`, `POST /Playlists/{id}/Items/{entry}/Move/{index}`, `POST /Items/{id}`, `POST /Playlists/{id}`, `POST /Items/{id}/Images/Primary`, `DELETE /Items/{id}` |
| Favourites | `POST`/`DELETE /UserFavoriteItems/{id}`, falling back to `/Users/{user}/FavoriteItems/{id}` on servers before 10.9 |
| Lyrics | `GET /Audio/{id}/Lyrics` |
| Audio | `GET /Audio/{id}/universal` |
| Artwork | `GET /Items/{id}/Images/Primary` |
| Play reporting | `POST /Sessions/Playing`, `/Sessions/Playing/Progress`, `/Sessions/Playing/Stopped` |
| Other players | `GET /Sessions`, `POST /Sessions/{id}/Playing…`, `POST /Sessions/{id}/Command` |

At most six requests run at once. Requests are not retried on their own; a
failed page offers **Retry**.

## Playback

Playback runs on its own thread. For each song it requests
`/Audio/{id}/universal`, naming the formats it decodes itself: MP3, FLAC, AAC
and ALAC in MP4, Vorbis in Ogg, WAV and AIFF. The server sends the file as it
is when the format is one of those and the bitrate is within the limit set in
Settings (none by default). Otherwise it converts to MP3, at 320 kbps unless
the limit is lower. Opus files are always converted.

The stream downloads from start to end while it plays. A seek within what has
arrived is immediate; a seek beyond it waits for the download to reach that
point. The song after the current one starts downloading twenty seconds before
the end, so an album plays without gaps.

Audio is converted to stereo at 44.1 kHz before the equalizer, the visualisers
and the output. A file at another rate, or with more channels, is resampled or
folded down to that.

**Normalize volume** applies the gain the server measured for each song
(`NormalizationGain`). Songs the server has not analysed play as they are.

The queue lives in the player, not on the server: a context (album, playlist,
artist, favourites, a mix) is read in full when it starts, up to 5,000 songs,
and shuffled or repeated locally.

## Play reporting

When a song starts, pauses, resumes, is sought in, and every ten seconds while
it plays, Jellifast tells the server where playback is, and when it stops. The
server's play counts, "last played" dates and dashboard activity come from
these reports. They cannot be turned off yet.

## Other players

The device button lists the server's other sessions that play audio and accept
remote control. Choosing one hands it this computer's current queue. While
another player is active and this one is not, the player bar shows and controls
that player through the server. Jellifast itself does not accept remote control
from other clients.

## Proxy

Settings → Proxy has four modes: **Off**, **System** (environment variables,
and the OS proxy on macOS and Windows), **HTTP** and **SOCKS5**. Every request
follows the mode: the server, audio, artwork, lyrics, update checks and
MilkDrop preset downloads. A configuration that cannot be built produces an
error and is never replaced by a direct connection. The proxy password is kept
in the credential store.

## Other destinations

- **lrclib.net**, when the lyrics panel is open and the server has no lyrics
  for the song: the artist, title, album and length are sent.
- **api.github.com and github.com**, for the daily update check (which can be
  turned off) and for update downloads, from the `j4ckxyz/jellifast` releases.
- **github.com**, once, when MilkDrop opens with an empty preset folder: the
  projectM preset packs.
