---
title: Getting Started
description: Sign in to a Jellyfin server and play music.
nav_order: 1
---

## Sign in

Jellifast asks for three things:

1. **Server address.** What you type into a browser to reach Jellyfin, such as
   `https://jellyfin.example.org` or `192.168.1.5:8096`. Without `http://` or
   `https://`, Jellifast tries HTTPS first and then HTTP. A server behind a
   path, such as `https://example.org/jellyfin`, works as written.
2. **User name.**
3. **Password.** Leave it empty for an account without one.

Press **Sign in** or Enter. The password goes to your server once and is not
stored. The server answers with an access token, which Jellifast keeps in the
system credential store: Keychain on macOS, Credential Manager on Windows,
Secret Service on Linux. The next launch signs in with that token.

The address and user name are remembered in the settings file to fill the form
again. **Sign out** in Settings tells the server to forget the token and deletes
the stored copy.

If your network needs a proxy, **Proxy Settings** under the form applies before
the sign-in request is made.

## Play something

Double-click a song, or press Play on an album, artist, playlist or
**Liked Songs**. Music plays on this computer through the default audio output;
Settings can name another one.

- **Liked Songs** are your Jellyfin favourites. The heart on a song adds or
  removes it.
- **Albums** and **Artists** list the whole library. Their heart marks a
  favourite.
- **Audio quality** in Settings is **Original** by default: every file as it is.
  A limit of 96, 160 or 320 kbps has the server convert larger files to MP3.
- The device button in the player bar lists other Jellyfin apps signed in to
  the same server that accept remote control.

## Links

`jellifast://track/ID`, `jellifast://album/ID`, `jellifast://artist/ID`,
`jellifast://playlist/ID` and `jellifast://search/WORDS` open in the running
app. **Copy link** in a menu copies the item's page in the server's own web
interface instead, which anyone with access to the server can open.
