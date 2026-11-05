//! Dashboard sessions: random ids, CSRF tokens, an in-memory cache over the `web_sessions` table.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::{header, HeaderMap};
use chrono::{NaiveDateTime, Utc};
use rand::rngs::OsRng;
use rand::RngCore;

use crate::db::Db;

/// Server-side lifetime of a login. The cookie itself is a browser-session cookie, like the TS one.
pub const MAX_SESSION_AGE_SECS: i64 = 30 * 24 * 60 * 60;

pub const SESSION_COOKIE: &str = "sid";
pub const OAUTH_STATE_COOKIE: &str = "oauth_state";

#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: String,
    pub username: String,
    pub csrf_token: String,
    created_unix: i64,
}

pub struct SessionStore {
    db: Db,
    cache: Mutex<HashMap<String, Session>>,
}

/// `n` random bytes from the OS as lowercase hex.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

fn is_valid_sid(sid: &str) -> bool {
    sid.len() == 64 && sid.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_created(text: Option<&str>) -> i64 {
    text.and_then(|t| NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S").ok())
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or_else(|| Utc::now().timestamp())
}

impl SessionStore {
    pub fn new(db: Db) -> Self {
        SessionStore {
            db,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Looks a session up in memory, then in the database. Expired sessions are removed.
    pub fn get(&self, sid: &str) -> Option<Session> {
        if !is_valid_sid(sid) {
            return None;
        }
        let cached = self.cache().get(sid).cloned();
        let session = match cached {
            Some(session) => session,
            None => {
                let row = match self.db.get_web_session(sid) {
                    Ok(Some(row)) if row.guild_member => row,
                    Ok(_) => return None,
                    Err(err) => {
                        tracing::error!("failed to load web session: {err:#}");
                        return None;
                    }
                };
                let session = Session {
                    user_id: row.user_id,
                    username: row.username,
                    csrf_token: row.csrf_token,
                    created_unix: parse_created(row.created_at.as_deref()),
                };
                self.cache().insert(sid.to_string(), session.clone());
                session
            }
        };
        if Utc::now().timestamp() - session.created_unix > MAX_SESSION_AGE_SECS {
            self.delete(sid);
            return None;
        }
        Some(session)
    }

    /// Creates and persists a new login. Returns `(sid, session)`.
    pub fn create(&self, user_id: &str, username: &str) -> anyhow::Result<(String, Session)> {
        let sid = random_hex(32);
        let session = Session {
            user_id: user_id.to_string(),
            username: username.to_string(),
            csrf_token: random_hex(24),
            created_unix: Utc::now().timestamp(),
        };
        self.db.save_web_session(
            &sid,
            &session.user_id,
            &session.username,
            true,
            &session.csrf_token,
            None,
        )?;
        self.cache().insert(sid.clone(), session.clone());
        Ok((sid, session))
    }

    pub fn delete(&self, sid: &str) {
        self.cache().remove(sid);
        if let Err(err) = self.db.delete_web_session(sid) {
            tracing::error!("failed to delete web session: {err:#}");
        }
    }
}

/// All cookies of the request. Values are percent-decoded when valid.
pub fn parse_cookies(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for value in headers.get_all(header::COOKIE) {
        let Ok(text) = value.to_str() else { continue };
        for part in text.split(';') {
            if let Some((name, raw)) = part.split_once('=') {
                let decoded = urlencoding::decode(raw.trim())
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| raw.trim().to_string());
                out.entry(name.trim().to_string()).or_insert(decoded);
            }
        }
    }
    out
}

/// Builds a `Set-Cookie` value. `max_age` of `Some(0)` clears the cookie.
pub fn set_cookie(
    name: &str,
    value: &str,
    path: &str,
    max_age: Option<i64>,
    secure: bool,
) -> String {
    let mut cookie = format!("{name}={value}; HttpOnly; Path={path}; SameSite=Lax");
    if let Some(age) = max_age {
        cookie.push_str(&format!("; Max-Age={age}"));
    }
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Constant-time string comparison (for CSRF tokens and OAuth state).
pub fn secure_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_eq_compares_content_and_length() {
        assert!(secure_eq("abc", "abc"));
        assert!(!secure_eq("abc", "abd"));
        assert!(!secure_eq("abc", "abcd"));
    }

    #[test]
    fn cookie_attributes() {
        let c = set_cookie("sid", "x", "/", None, true);
        assert_eq!(c, "sid=x; HttpOnly; Path=/; SameSite=Lax; Secure");
        let c = set_cookie("sid", "", "/", Some(0), false);
        assert_eq!(c, "sid=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0");
    }

    #[test]
    fn parses_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "a=1; sid=abc%20d;broken; b=2".parse().unwrap(),
        );
        let cookies = parse_cookies(&headers);
        assert_eq!(cookies.get("a").map(String::as_str), Some("1"));
        assert_eq!(cookies.get("sid").map(String::as_str), Some("abc d"));
        assert_eq!(cookies.get("b").map(String::as_str), Some("2"));
    }

    #[test]
    fn sid_must_be_64_hex_chars() {
        assert!(is_valid_sid(&random_hex(32)));
        assert!(!is_valid_sid("short"));
        assert!(!is_valid_sid(&"z".repeat(64)));
    }
}
