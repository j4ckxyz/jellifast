//! Jellyfin server client.
//!
//! `jellyfin` holds the server's response shapes and turns them into the
//! app's own `models`; `client` makes the requests.

pub mod client;
pub mod jellyfin;
pub mod models;

pub use client::{ApiClient, ApiError, NetActivity, PlayRequest};
