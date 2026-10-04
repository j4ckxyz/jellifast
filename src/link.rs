//! Links as they arrive from outside: the desktop's URL handler, the command
//! line, and a second launch handing one to the running instance.
//!
//! Every accepted shape comes back as the one canonical URI,
//! `jellyfin:<kind>:<id>` or `jellyfin:search:<encoded query>`, or nothing
//! when it is not something the app can open.

/// Resource pages. Search links carry text instead of a resource id.
const KINDS: [&str; 4] = ["track", "album", "artist", "playlist"];

/// The canonical form of a context URI. A playlist named with its owner,
/// `jellyfin:user:NAME:playlist:ID`, is the plain `jellyfin:playlist:ID`
/// the app's models hold; strict comparisons need the one shape.
pub fn canonical_context_uri(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("jellyfin:user:") {
        const PLAYLIST: &str = ":playlist:";
        if let Some(at) = rest.find(PLAYLIST) {
            let id = &rest[at + PLAYLIST.len()..];
            if !id.is_empty() && !id.contains(':') {
                return format!("jellyfin:playlist:{id}");
            }
        }
    }
    uri.to_owned()
}

/// The canonical `jellyfin:<kind>:<id>` behind `text`, or `None` when it is
/// not a link to a track, album, artist, playlist, or search.
///
/// Accepted: `jellyfin:track:ID`, `jellyfin:user:NAME:playlist:ID`, and the
/// URL shape the desktop hands over, `jellifast://track/ID`. A Jellyfin
/// server's own web addresses carry an id but not what it names, so they
/// are not links the app can open.
pub fn parse(text: &str) -> Option<String> {
    if let Some(query) = search_query(text) {
        return Some(format!(
            "jellyfin:search:{}",
            percent_encoding::utf8_percent_encode(&query, percent_encoding::NON_ALPHANUMERIC)
        ));
    }
    let text = text.trim();
    let mut segments: Vec<&str> = if let Some(rest) = text.strip_prefix("jellifast://") {
        path_segments(rest)
    } else {
        text.strip_prefix("jellyfin:")?
            .split(':')
            .filter(|part| !part.is_empty())
            .collect()
    };
    // A playlist named with its owner: jellyfin:user:NAME:playlist:ID.
    if segments.len() >= 4 && segments[0] == "user" && segments[2] == "playlist" {
        segments.drain(..2);
    }
    let [kind, id, ..] = segments.as_slice() else {
        return None;
    };
    let kind = kind.to_ascii_lowercase();
    if !KINDS.contains(&kind.as_str()) || !is_id(id) {
        return None;
    }
    Some(format!("jellyfin:{kind}:{id}"))
}

/// Decodes a search link once. A path's `+` is literal, not a form-encoded
/// space. Canonical links encode the whole query so delimiters and Unicode
/// survive command-line, D-Bus, Apple Event and line-based socket delivery.
pub fn search_query(text: &str) -> Option<String> {
    let text = text.trim();
    let encoded = if let Some(rest) = text.strip_prefix("jellifast://") {
        let path = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
        let (kind, query) = path.split_once('/').unwrap_or((path, ""));
        if !kind.eq_ignore_ascii_case("search") {
            return None;
        }
        query
    } else {
        let (kind, query) = text.strip_prefix("jellyfin:")?.split_once(':')?;
        if !kind.eq_ignore_ascii_case("search") {
            return None;
        }
        query
    };
    let query = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()?;
    (!query.chars().any(char::is_control)).then(|| query.into_owned())
}

/// The path of a URL split at slashes, its query and fragment dropped,
/// empty segments (a trailing slash) with them.
fn path_segments(rest: &str) -> Vec<&str> {
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    rest[..end]
        .split('/')
        .filter(|part| !part.is_empty())
        .collect()
}

/// Jellyfin ids are hexadecimal, some with dashes; anything else on a link
/// is not one, whatever hands it over.
fn is_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SONG: &str = "a794b7c125d436df8dc2d870ad7b7e1e";

    #[test]
    fn every_link_shape_becomes_the_one_uri() {
        for (link, uri) in [
            (
                format!("jellyfin:track:{SONG}"),
                format!("jellyfin:track:{SONG}"),
            ),
            (
                format!("jellyfin:user:jack:playlist:{SONG}"),
                format!("jellyfin:playlist:{SONG}"),
            ),
            (
                format!("jellifast://album/{SONG}"),
                format!("jellyfin:album:{SONG}"),
            ),
            (
                format!("jellifast://artist/{SONG}/?from=share#top"),
                format!("jellyfin:artist:{SONG}"),
            ),
            (
                format!("  jellyfin:Artist:{SONG}\n"),
                format!("jellyfin:artist:{SONG}"),
            ),
            (
                "jellyfin:track:a794b7c1-25d4-36df-8dc2-d870ad7b7e1e".to_string(),
                "jellyfin:track:a794b7c1-25d4-36df-8dc2-d870ad7b7e1e".to_string(),
            ),
        ] {
            assert_eq!(parse(&link).as_deref(), Some(uri.as_str()), "{link}");
        }
    }

    /// What is not a page here is refused rather than guessed at.
    #[test]
    fn anything_else_is_not_a_link() {
        for text in [
            "",
            "jellyfin:",
            "jellyfin:track:",
            "jellyfin:show:abc",
            "jellyfin:episode:abc",
            "jellyfin:user:me:collection",
            "jellyfin:track:not/an/id",
            "jellifast://",
            "jellifast://track",
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
            "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC",
            "https://music.example.org/web/#/details?id=a794b7c125d436df8dc2d870ad7b7e1e",
            "file:///track/abc",
            "track:abc",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
        let long = format!("jellyfin:track:{}", "x".repeat(65));
        assert_eq!(parse(&long), None);
    }

    #[test]
    fn a_playlist_named_with_its_owner_is_the_plain_playlist() {
        assert_eq!(
            canonical_context_uri("jellyfin:user:jack:playlist:abc"),
            "jellyfin:playlist:abc"
        );
        assert_eq!(
            canonical_context_uri("jellyfin:user:jack:collection"),
            "jellyfin:user:jack:collection"
        );
        assert_eq!(
            canonical_context_uri("jellyfin:album:abc"),
            "jellyfin:album:abc"
        );
    }

    #[test]
    fn search_links_preserve_the_query_across_normalization_and_delivery() {
        for (link, query) in [
            (
                "jellyfin:search:artist:Radiohead year:1997",
                "artist:Radiohead year:1997",
            ),
            ("jellifast://search/%E6%9D%B1%E4%BA%AC", "東京"),
            ("jellifast://search/AC%2FDC?from=share#top", "AC/DC"),
            ("jellifast://search/C%2B%2B+100%25", "C+++100%"),
            ("jellyfin:search:%2520", "%20"),
            ("jellifast://search", ""),
            ("jellifast://search/", ""),
            ("jellyfin:search:", ""),
        ] {
            assert_eq!(search_query(link).as_deref(), Some(query), "{link}");
            let canonical = parse(link).unwrap();
            assert_eq!(search_query(&canonical).as_deref(), Some(query));
            assert_eq!(parse(&canonical).as_ref(), Some(&canonical));
            assert!(canonical.is_ascii() && !canonical.contains(['\n', ' ']));
        }
        for invalid in [
            "https://example.com/search/song",
            "https://open.spotify.com/search/song",
            "file:///search/song",
            "jellyfin:search:bad%FFutf8",
            "jellifast://search/line%0Abreak",
            "jellyfin:search:zero%00byte",
        ] {
            assert_eq!(parse(invalid), None, "{invalid}");
        }
    }
}
