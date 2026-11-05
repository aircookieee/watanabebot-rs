//! Spotify client-credentials client for the daily Love Live song.
//! OWNER: AniList agent. STUB: replace with the real implementation.

pub struct Spotify {
    #[allow(dead_code)]
    client: reqwest::Client,
}

impl Spotify {
    pub fn new(client: reqwest::Client) -> Self {
        Spotify { client }
    }
}
