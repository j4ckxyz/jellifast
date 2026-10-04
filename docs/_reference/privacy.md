---
title: Privacy
description: What Jellifast sends, to whom, and what stays on this computer.
nav_order: 3
---

Jellifast has no telemetry, analytics, or hosted service. It talks to the
Jellyfin server you sign in to, and to three other places for specific tasks.

- **Your Jellyfin server** receives your user name and password once at
  sign-in, and afterwards an access token with every request. It is told what
  this computer plays, so its play counts and history stay true. See
  [How It Connects](how-it-connects.md) for every request.
- **lrclib.net** receives the artist, title, album and length of the playing
  song when the lyrics panel is open and the server has no lyrics for it.
- **GitHub** is asked once a day whether a newer release exists. Automatic
  checks can be turned off in Settings. No personal data is sent.
- **GitHub** serves the MilkDrop preset packs the first time MilkDrop opens
  with an empty preset folder.

The password is never stored. The access token is kept in the system credential
store and deleted by **Sign out**. Logs never contain the token or the
password.
