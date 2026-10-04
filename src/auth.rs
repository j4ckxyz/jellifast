//! Signing in to a Jellyfin server.
//!
//! The server address, a user name and a password are exchanged once for an
//! access token (`POST /Users/AuthenticateByName`). The password is never
//! stored: only the token, in the platform credential store. Every later
//! request carries the token in Jellyfin's `Authorization: MediaBrowser`
//! header.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::ApiError;

/// How this client names itself to the server, in the dashboard's device list.
pub const CLIENT_NAME: &str = "Jellifast";

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// A signed-in server session. Deliberately has no Debug implementation: it
/// contains a usable access token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// The server's base address, with a scheme and without a trailing slash.
    pub server: String,
    pub server_id: String,
    pub server_name: String,
    pub user_id: String,
    pub username: String,
    pub token: String,
    /// The id this installation signed in with. The token belongs to it.
    pub device_id: String,
}

impl Session {
    pub fn valid(&self) -> bool {
        !self.token.is_empty()
            && !self.user_id.is_empty()
            && !self.device_id.is_empty()
            && reqwest::Url::parse(&self.server)
                .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
    }

    /// The value of the `Authorization` header for this session.
    pub fn authorization(&self, device_name: &str) -> String {
        authorization(&self.device_id, device_name, Some(&self.token))
    }
}

/// What the sign-in form asks for. No Debug implementation: it holds the
/// password until the server has exchanged it for a token.
#[derive(Clone, PartialEq, Eq)]
pub struct Login {
    pub server: String,
    pub username: String,
    pub password: String,
}

/// Jellyfin's client identification header, with the token once there is one.
pub fn authorization(device_id: &str, device_name: &str, token: Option<&str>) -> String {
    let mut value = format!(
        "MediaBrowser Client=\"{CLIENT_NAME}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
        header_text(device_name),
        header_text(device_id),
        env!("CARGO_PKG_VERSION"),
    );
    if let Some(token) = token {
        value.push_str(&format!(", Token=\"{}\"", header_text(token)));
    }
    value
}

/// Header values are ASCII without quotes; a computer named anything else
/// still signs in.
fn header_text(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control() && *c != '"' && *c != ',')
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        "Computer".into()
    } else {
        cleaned
    }
}

/// A fresh random device id for this installation.
pub fn new_device_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The addresses to try for what was typed into the server field. An address
/// with a scheme is taken as written; a bare host is tried over HTTPS first
/// and then HTTP, the way a home server on port 8096 answers.
pub fn server_candidates(input: &str) -> Result<Vec<String>, ApiError> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(invalid("Enter your Jellyfin server's address."));
    }
    let candidates: Vec<String> = if trimmed.contains("://") {
        vec![trimmed.to_string()]
    } else {
        vec![format!("https://{trimmed}"), format!("http://{trimmed}")]
    };
    for candidate in &candidates {
        let url = reqwest::Url::parse(candidate)
            .map_err(|_| invalid("That doesn't look like a server address."))?;
        if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
            return Err(invalid(
                "The server address must start with http:// or https://.",
            ));
        }
    }
    Ok(candidates)
}

fn invalid(message: &str) -> ApiError {
    ApiError::Status {
        status: 0,
        message: message.to_string(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PublicInfo {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    server_name: Option<String>,
    #[serde(default)]
    product_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthResult {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    server_id: Option<String>,
    #[serde(default)]
    user: Option<AuthUser>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthUser {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

/// Finds the server behind what was typed and exchanges the user name and
/// password for a session.
pub async fn sign_in(
    http: &reqwest::Client,
    login: &Login,
    device_id: &str,
    device_name: &str,
) -> Result<Session, ApiError> {
    if login.username.trim().is_empty() {
        return Err(invalid("Enter your user name."));
    }
    let candidates = server_candidates(&login.server)?;
    let mut last_error = None;
    let mut found = None;
    for candidate in candidates {
        match probe(http, &candidate).await {
            Ok(info) => {
                found = Some((candidate, info));
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let Some((server, info)) = found else {
        return Err(last_error.unwrap_or_else(|| invalid("No Jellyfin server answered there.")));
    };
    let response = http
        .post(format!("{server}/Users/AuthenticateByName"))
        .header(
            reqwest::header::AUTHORIZATION,
            authorization(device_id, device_name, None),
        )
        .json(&serde_json::json!({
            "Username": login.username.trim(),
            "Pw": login.password,
        }))
        .send()
        .await
        .map_err(|error| ApiError::Network(error.without_url().to_string()))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(ApiError::Status {
            status: status.as_u16(),
            message: "Wrong user name or password.".into(),
        });
    }
    if !status.is_success() {
        return Err(ApiError::Status {
            status: status.as_u16(),
            message: format!("The server refused the sign-in ({status})."),
        });
    }
    let result: AuthResult = response
        .json()
        .await
        .map_err(|error| ApiError::Decode(error.without_url().to_string()))?;
    let (Some(token), Some(user)) = (result.access_token, result.user) else {
        return Err(ApiError::Decode("the sign-in answer had no token".into()));
    };
    Ok(Session {
        server_id: result.server_id.or(info.id).unwrap_or_default(),
        server_name: info.server_name.unwrap_or_default(),
        username: user
            .name
            .unwrap_or_else(|| login.username.trim().to_string()),
        user_id: user.id,
        token,
        device_id: device_id.to_string(),
        server,
    })
}

/// Asks an address whether a Jellyfin server lives there.
async fn probe(http: &reqwest::Client, server: &str) -> Result<PublicInfo, ApiError> {
    let response = http
        .get(format!("{server}/System/Info/Public"))
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|error| ApiError::Network(error.without_url().to_string()))?;
    if !response.status().is_success() {
        return Err(invalid("No Jellyfin server answered at that address."));
    }
    let info: PublicInfo = response
        .json()
        .await
        .map_err(|_| invalid("That address answered, but not as a Jellyfin server."))?;
    if info.id.is_none() && info.product_name.is_none() {
        return Err(invalid(
            "That address answered, but not as a Jellyfin server.",
        ));
    }
    Ok(info)
}

/// Tells the server this token is finished with. Best effort: the local copy
/// is forgotten whether or not the server hears it.
pub async fn sign_out(http: &reqwest::Client, session: &Session, device_name: &str) {
    let _ = http
        .post(format!("{}/Sessions/Logout", session.server))
        .header(
            reqwest::header::AUTHORIZATION,
            session.authorization(device_name),
        )
        .timeout(PROBE_TIMEOUT)
        .send()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_is_tried_over_https_then_http() {
        assert_eq!(
            server_candidates(" music.example.org/ ").unwrap(),
            ["https://music.example.org", "http://music.example.org"]
        );
        assert_eq!(
            server_candidates("192.168.1.5:8096").unwrap(),
            ["https://192.168.1.5:8096", "http://192.168.1.5:8096"]
        );
    }

    #[test]
    fn a_written_scheme_and_base_path_are_kept() {
        assert_eq!(
            server_candidates("http://nas.local:8096/jellyfin/").unwrap(),
            ["http://nas.local:8096/jellyfin"]
        );
        assert!(server_candidates("ftp://nas.local").is_err());
        assert!(server_candidates("  ").is_err());
    }

    #[test]
    fn the_header_names_the_client_and_carries_the_token_last() {
        let header = authorization("abc123", "Jack's \"Mac\", desk", Some("secret"));
        assert_eq!(
            header,
            format!(
                "MediaBrowser Client=\"Jellifast\", Device=\"Jack's Mac desk\", DeviceId=\"abc123\", Version=\"{}\", Token=\"secret\"",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert!(!authorization("abc123", "ünï", None).contains("Token"));
    }

    #[test]
    fn a_session_needs_a_token_and_a_web_address() {
        let session = Session {
            server: "http://nas.local:8096".into(),
            server_id: "s".into(),
            server_name: "NAS".into(),
            user_id: "u".into(),
            username: "jack".into(),
            token: "t".into(),
            device_id: "d".into(),
        };
        assert!(session.valid());
        assert!(
            !Session {
                token: String::new(),
                ..session.clone()
            }
            .valid()
        );
        assert!(
            !Session {
                server: "nas.local".into(),
                ..session
            }
            .valid()
        );
    }
}
