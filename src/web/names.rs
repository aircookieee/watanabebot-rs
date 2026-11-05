//! Discord id -> display name lookups for the dashboard, with a TTL cache.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serenity::all::{GuildId, Http, UserId};
use tokio::task::JoinSet;

/// How long a resolved name is reused.
const NAME_TTL: Duration = Duration::from_secs(10 * 60);
/// How long a failed lookup (the raw id is shown) is remembered, so errors do not trigger a retry storm.
const FAILURE_TTL: Duration = Duration::from_secs(60);
/// Upper bound for a single REST lookup.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);
/// The cache is swept of expired entries when it grows past this size.
const SWEEP_THRESHOLD: usize = 4096;

struct Entry {
    name: String,
    expires: Instant,
}

pub struct NameResolver {
    http: Arc<Http>,
    guild: Option<GuildId>,
    cache: Mutex<HashMap<String, Entry>>,
}

fn parse_snowflake(id: &str) -> Option<u64> {
    id.parse::<u64>().ok().filter(|v| *v != 0)
}

/// Resolves one id: guild member display name, else the user name, else `None`.
async fn fetch_name(http: &Http, guild: Option<GuildId>, id: &str) -> Option<String> {
    let user_id = UserId::new(parse_snowflake(id)?);
    if let Some(guild) = guild {
        if let Ok(member) = http.get_member(guild, user_id).await {
            let name = member
                .nick
                .clone()
                .filter(|n| !n.is_empty())
                .or_else(|| member.user.global_name.clone().filter(|n| !n.is_empty()))
                .unwrap_or_else(|| member.user.name.clone());
            return Some(name);
        }
    }
    let user = http.get_user(user_id).await.ok()?;
    Some(
        user.global_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| user.name.clone()),
    )
}

impl NameResolver {
    pub fn new(http: Arc<Http>, guild: Option<GuildId>) -> Self {
        NameResolver {
            http,
            guild,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cached(&self, id: &str) -> Option<String> {
        let cache = self.cache();
        cache
            .get(id)
            .filter(|e| e.expires > Instant::now())
            .map(|e| e.name.clone())
    }

    fn store(&self, id: &str, name: String, ttl: Duration) {
        let mut cache = self.cache();
        if cache.len() >= SWEEP_THRESHOLD {
            let now = Instant::now();
            cache.retain(|_, e| e.expires > now);
        }
        cache.insert(
            id.to_string(),
            Entry {
                name,
                expires: Instant::now() + ttl,
            },
        );
    }

    /// Resolves every distinct id concurrently. Every id of the input is present in the result
    /// (the id itself is used when no name can be found).
    pub async fn resolve_many<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let mut missing = HashSet::new();
        for id in ids {
            if out.contains_key(id) || missing.contains(id) {
                continue;
            }
            match self.cached(id) {
                Some(name) => {
                    out.insert(id.to_string(), name);
                }
                None => {
                    missing.insert(id.to_string());
                }
            }
        }

        let mut tasks = JoinSet::new();
        for id in missing {
            let http = self.http.clone();
            let guild = self.guild;
            tasks.spawn(async move {
                let found = tokio::time::timeout(LOOKUP_TIMEOUT, fetch_name(&http, guild, &id))
                    .await
                    .ok()
                    .flatten();
                (id, found)
            });
        }
        while let Some(joined) = tasks.join_next().await {
            let Ok((id, found)) = joined else { continue };
            match found {
                Some(name) => {
                    self.store(&id, name.clone(), NAME_TTL);
                    out.insert(id, name);
                }
                None => {
                    self.store(&id, id.clone(), FAILURE_TTL);
                    out.insert(id.clone(), id);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cached_names_are_served_without_rest_calls() {
        let resolver = NameResolver::new(Arc::new(Http::new("test")), None);
        resolver.store("123", "Chika".to_string(), NAME_TTL);
        let names = resolver.resolve_many(["123", "123"]).await;
        assert_eq!(names.get("123").map(String::as_str), Some("Chika"));
        assert_eq!(names.len(), 1);
    }

    #[tokio::test]
    async fn invalid_ids_fall_back_to_the_id() {
        let resolver = NameResolver::new(Arc::new(Http::new("test")), None);
        let names = resolver.resolve_many(["0", "not-a-number"]).await;
        assert_eq!(names.get("0").map(String::as_str), Some("0"));
        assert_eq!(
            names.get("not-a-number").map(String::as_str),
            Some("not-a-number")
        );
    }

    #[test]
    fn expired_entries_are_ignored() {
        let resolver = NameResolver::new(Arc::new(Http::new("test")), None);
        resolver.store("1", "old".to_string(), Duration::from_secs(0));
        assert!(resolver.cached("1").is_none());
    }
}
