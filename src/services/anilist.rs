//! AniList GraphQL client, rate limiting, list caching and score aggregation.
//! OWNER: AniList agent. STUB: replace with the real implementation.

use crate::db::Db;

pub struct Anilist {
    #[allow(dead_code)]
    db: Db,
    #[allow(dead_code)]
    client: reqwest::Client,
}

impl Anilist {
    pub fn new(db: Db, client: reqwest::Client) -> Self {
        Anilist { db, client }
    }
}
