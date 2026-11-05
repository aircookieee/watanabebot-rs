//! Discord OAuth2 login and logout.

use std::collections::HashMap;

use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;
use serenity::all::UserId;

use super::http;
use super::session::{
    self, parse_cookies, random_hex, secure_eq, set_cookie, OAUTH_STATE_COOKIE, SESSION_COOKIE,
};
use super::Ctx;

const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
const ME_URL: &str = "https://discord.com/api/users/@me";
/// Lifetime of the `oauth_state` cookie: long enough to finish the Discord consent screen.
const STATE_MAX_AGE_SECS: i64 = 600;

/// `GET /auth/discord`: remembers a random `state` in a short-lived cookie and redirects to Discord.
pub fn login(ctx: &Ctx) -> Response {
    let state = random_hex(16);
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &ctx.opts.client_id)
        .append_pair("redirect_uri", &ctx.opts.redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "identify")
        .append_pair("state", &state)
        .finish();
    let mut res = http::redirect(&format!("{AUTHORIZE_URL}?{query}"));
    http::add_cookie(
        &mut res,
        &set_cookie(
            OAUTH_STATE_COOKIE,
            &state,
            "/auth",
            Some(STATE_MAX_AGE_SECS),
            ctx.opts.secure_cookies,
        ),
    );
    res
}

/// `GET /auth/callback`: verifies `state`, exchanges the code and creates the session.
pub async fn callback(ctx: &Ctx, headers: &HeaderMap, query: &HashMap<String, String>) -> Response {
    let cookies = parse_cookies(headers);
    let state_ok = match (cookies.get(OAUTH_STATE_COOKIE), query.get("state")) {
        (Some(expected), Some(given)) => !expected.is_empty() && secure_eq(expected, given),
        _ => false,
    };

    let mut res = if !state_ok {
        http::send(StatusCode::BAD_REQUEST, "Invalid OAuth state.")
    } else {
        complete_login(ctx, query).await
    };
    // The state is single use, whatever the outcome.
    http::add_cookie(
        &mut res,
        &set_cookie(
            OAUTH_STATE_COOKIE,
            "",
            "/auth",
            Some(0),
            ctx.opts.secure_cookies,
        ),
    );
    res
}

async fn complete_login(ctx: &Ctx, query: &HashMap<String, String>) -> Response {
    let Some(code) = query.get("code").filter(|c| !c.is_empty()) else {
        return http::send(StatusCode::BAD_REQUEST, "Missing OAuth code");
    };
    let opts = &ctx.opts;
    let client = &ctx.app.client;

    let token_res = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", opts.client_id.as_str()),
            ("client_secret", opts.client_secret.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", opts.redirect_uri.as_str()),
        ])
        .send()
        .await;
    let token: Value = match token_res {
        Ok(res) => res.json().await.unwrap_or(Value::Null),
        Err(err) => {
            tracing::warn!("Discord token exchange failed: {err}");
            return http::send(StatusCode::BAD_GATEWAY, "Could not reach Discord.");
        }
    };
    let Some(access_token) = token.get("access_token").and_then(Value::as_str) else {
        return http::send(StatusCode::BAD_REQUEST, "Discord OAuth failed.");
    };

    let user: Value = match client.get(ME_URL).bearer_auth(access_token).send().await {
        Ok(res) => res.json().await.unwrap_or(Value::Null),
        Err(err) => {
            tracing::warn!("Discord user lookup failed: {err}");
            return http::send(StatusCode::BAD_GATEWAY, "Could not reach Discord.");
        }
    };
    let (Some(user_id), Some(username)) = (
        user.get("id").and_then(Value::as_str),
        user.get("username").and_then(Value::as_str),
    ) else {
        return http::send(StatusCode::BAD_REQUEST, "Discord OAuth failed.");
    };

    if !verify_guild_membership(ctx, user_id).await {
        return http::send(
            StatusCode::FORBIDDEN,
            "You must be a member of the configured Discord server.",
        );
    }

    match ctx.sessions.create(user_id, username) {
        Ok((sid, _)) => {
            let mut res = http::redirect("/");
            http::add_cookie(
                &mut res,
                &set_cookie(SESSION_COOKIE, &sid, "/", None, opts.secure_cookies),
            );
            res
        }
        Err(err) => {
            tracing::error!("failed to create web session: {err:#}");
            http::send(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not create a session.",
            )
        }
    }
}

async fn verify_guild_membership(ctx: &Ctx, user_id: &str) -> bool {
    let Some(guild) = ctx.opts.member_guild else {
        return false;
    };
    let Some(user) = user_id.parse::<u64>().ok().filter(|v| *v != 0) else {
        return false;
    };
    match ctx.app.http.get_member(guild, UserId::new(user)).await {
        Ok(_) => true,
        Err(err) => {
            tracing::debug!("guild membership check for {user_id} failed: {err}");
            false
        }
    }
}

/// `POST /auth/logout`: the frontend submits a plain form without a CSRF token, so this only
/// ever ends the caller's own session (worst case an attacker logs a user out).
pub fn logout(ctx: &Ctx, headers: &HeaderMap) -> Response {
    if let Some(sid) = parse_cookies(headers).get(SESSION_COOKIE) {
        ctx.sessions.delete(sid);
    }
    let mut res = http::redirect("/login");
    http::add_cookie(
        &mut res,
        &session::set_cookie(SESSION_COOKIE, "", "/", Some(0), ctx.opts.secure_cookies),
    );
    res
}
