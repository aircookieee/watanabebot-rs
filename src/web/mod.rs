//! Dashboard HTTP server (Discord OAuth + JSON API + built React app from `dist-web/`).
//!
//! Port of `src/web/server.ts`. Requests are dispatched by hand in `dispatch` (not with per-route
//! axum handlers) so that unmatched methods and paths behave exactly like the Node server:
//! `401` before anything else under `/api/`, `404 not_found` for unknown API routes, and so on.

mod api;
mod assets;
mod auth;
mod http;
mod names;
mod session;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use axum::Router;
use serenity::all::GuildId;

use crate::bot::Data;
use crate::config::{self, Config};

use names::NameResolver;
use session::{parse_cookies, SessionStore, MAX_SESSION_AGE_SECS, SESSION_COOKIE};

/// Settings the handlers need, copied out of the global config once.
#[derive(Clone)]
pub(crate) struct WebOptions {
    port: u16,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    /// Guild whose membership is required to log in (`DISCORD_GUILD_ID`).
    member_guild: Option<GuildId>,
    /// Guild all dashboard data belongs to (`CURRENCY_GUILD_ID`).
    currency_guild: Option<GuildId>,
    dist_dir: PathBuf,
    favicon_candidates: Vec<PathBuf>,
    /// Add `Secure` to cookies (`WEB_BASE_URL` is https).
    secure_cookies: bool,
}

impl WebOptions {
    fn from_config(cfg: &Config) -> Self {
        let base_url = cfg.web.base_url.trim_end_matches('/').to_string();
        let redirect_uri = if cfg.discord.oauth_redirect_uri.is_empty() {
            format!("{base_url}/auth/callback")
        } else {
            cfg.discord.oauth_redirect_uri.clone()
        };
        let mut favicon_candidates = vec![cfg.web.favicon_path.clone()];
        for name in ["yousoro2.png", "yousoro.png"] {
            favicon_candidates.push(cfg.resource(name));
            favicon_candidates.push(PathBuf::from("resources").join(name));
        }
        WebOptions {
            port: cfg.web.port,
            client_id: cfg.discord.client_id.clone(),
            client_secret: cfg.discord.oauth_client_secret.clone(),
            redirect_uri,
            member_guild: cfg.discord.guild_id,
            currency_guild: cfg.currency.guild_id,
            dist_dir: cfg.paths.web_dist_dir.clone(),
            favicon_candidates,
            secure_cookies: base_url.to_ascii_lowercase().starts_with("https"),
        }
    }
}

pub(crate) struct WebState {
    app: Data,
    opts: WebOptions,
    sessions: SessionStore,
    names: NameResolver,
}

impl WebState {
    fn new(app: Data, opts: WebOptions) -> Self {
        WebState {
            sessions: SessionStore::new(app.db.clone()),
            names: NameResolver::new(app.http.clone(), opts.member_guild),
            app,
            opts,
        }
    }
}

pub(crate) type Ctx = Arc<WebState>;

fn router(ctx: Ctx) -> Router {
    Router::new().fallback(dispatch).with_state(ctx)
}

/// Starts the dashboard on `0.0.0.0:WEB_PORT`. Runs until the listener fails.
pub async fn serve(state: Data) -> anyhow::Result<()> {
    let cfg = config::get();
    let opts = WebOptions::from_config(cfg);
    let port = opts.port;
    if opts.currency_guild.is_none() {
        tracing::warn!("CURRENCY_GUILD_ID is not set: dashboard data requests will fail");
    }
    let ctx: Ctx = Arc::new(WebState::new(state, opts));

    match ctx.app.db.purge_expired_web_sessions(MAX_SESSION_AGE_SECS) {
        Ok(0) => {}
        Ok(n) => tracing::info!("Removed {n} expired web sessions"),
        Err(err) => tracing::warn!("failed to purge expired web sessions: {err:#}"),
    }

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))
        .await
        .with_context(|| format!("binding web server to port {port}"))?;
    tracing::info!("Web server listening on {port}");
    axum::serve(listener, router(ctx))
        .await
        .context("web server error")?;
    Ok(())
}

async fn dispatch(State(ctx): State<Ctx>, req: Request) -> Response {
    let get = req.method() == Method::GET;
    let post = req.method() == Method::POST;
    let path = req.uri().path().to_string();

    if get && path == "/favicon.ico" {
        return assets::serve_favicon(&ctx.opts.favicon_candidates).await;
    }
    if get && path == "/auth/discord" {
        return auth::login(&ctx);
    }
    if get && path == "/auth/callback" {
        let query = http::parse_query(req.uri().query());
        return auth::callback(&ctx, req.headers(), &query).await;
    }
    if post && path == "/auth/logout" {
        return auth::logout(&ctx, req.headers());
    }

    if path.starts_with("/api/") {
        let session = current_session(&ctx, &req);
        return api::handle(&ctx, req, session).await;
    }

    if path == "/" {
        let target = if current_session(&ctx, &req).is_some() {
            "/dashboard"
        } else {
            "/login"
        };
        return http::redirect(target);
    }

    if get && !assets::is_asset_request(&path) {
        return assets::serve_app_shell(&ctx.opts.dist_dir).await;
    }
    if get {
        if let Some(res) = assets::serve_static(&ctx.opts.dist_dir, &path).await {
            return res;
        }
    }
    http::send(StatusCode::NOT_FOUND, "Not found")
}

fn current_session(ctx: &Ctx, req: &Request) -> Option<session::Session> {
    let cookies = parse_cookies(req.headers());
    cookies
        .get(SESSION_COOKIE)
        .and_then(|sid| ctx.sessions.get(sid))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{header, Request as HttpRequest};
    use tower::ServiceExt;

    use super::*;
    use crate::bot::AppState;
    use crate::db::Db;

    /// A web state on a fresh temp database and a temp web root containing `index.html`.
    fn test_ctx(name: &str) -> (Ctx, PathBuf) {
        config::init();
        let dir =
            std::env::temp_dir().join(format!("watanabebot-web-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dist").join("assets")).unwrap();
        std::fs::write(
            dir.join("dist").join("index.html"),
            "<html>spa shell</html>",
        )
        .unwrap();
        std::fs::write(
            dir.join("dist").join("assets").join("app.js"),
            "console.log(1)",
        )
        .unwrap();

        let mut cfg = Config::from_env();
        cfg.paths.database_path = dir.join("test.db");
        cfg.production = false;
        let db = Db::open(&cfg).unwrap();
        let http = Arc::new(serenity::all::Http::new("test"));
        let app: Data = Arc::new(AppState::new(db, http));

        let mut opts = WebOptions::from_config(&cfg);
        opts.dist_dir = dir.join("dist");
        opts.currency_guild = Some(GuildId::new(1));
        opts.favicon_candidates = vec![dir.join("missing.png")];
        (Arc::new(WebState::new(app, opts)), dir)
    }

    async fn call(ctx: &Ctx, req: HttpRequest<Body>) -> Response {
        router(ctx.clone()).oneshot(req).await.unwrap()
    }

    fn get(uri: &str) -> HttpRequest<Body> {
        HttpRequest::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn with_cookie(mut req: HttpRequest<Body>, cookie: &str) -> HttpRequest<Body> {
        req.headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        req
    }

    async fn body_text(res: Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn unauthenticated_api_is_401() {
        let (ctx, _dir) = test_ctx("unauth");
        let res = call(&ctx, get("/api/me")).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_text(res).await, r#"{"error":"unauthorized"}"#);

        // Also for unknown routes: the session check comes first.
        let res = call(&ctx, get("/api/does-not-exist")).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_api_route_with_a_session_is_404() {
        let (ctx, _dir) = test_ctx("notfound");
        let (sid, _) = ctx.sessions.create("42", "tester").unwrap();
        let cookie = format!("sid={sid}");
        let res = call(&ctx, with_cookie(get("/api/x"), &cookie)).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_text(res).await, r#"{"error":"not_found"}"#);

        // Wrong method on a known route is also not_found, like the TS server.
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/api/me")
            .body(Body::empty())
            .unwrap();
        let res = call(&ctx, with_cookie(req, &cookie)).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_tournament_is_405() {
        let (ctx, _dir) = test_ctx("405");
        let (sid, _) = ctx.sessions.create("42", "tester").unwrap();
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/api/tournament")
            .body(Body::empty())
            .unwrap();
        let res = call(&ctx, with_cookie(req, &format!("sid={sid}"))).await;
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body_text(res).await, r#"{"error":"method_not_allowed"}"#);
    }

    #[tokio::test]
    async fn api_me_and_session_return_the_session() {
        let (ctx, _dir) = test_ctx("me");
        let (sid, session) = ctx.sessions.create("42", "tester").unwrap();
        let cookie = format!("sid={sid}");

        let res = call(&ctx, with_cookie(get("/api/me"), &cookie)).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_text(res).await).unwrap();
        assert_eq!(body["userId"], "42");
        assert_eq!(body["username"], "tester");
        assert_eq!(body["role"], "user");
        assert_eq!(body["csrfToken"], session.csrf_token);
        assert!(body["balance"].is_number());

        let res = call(&ctx, with_cookie(get("/api/session"), &cookie)).await;
        let body: serde_json::Value = serde_json::from_str(&body_text(res).await).unwrap();
        assert_eq!(body["authenticated"], true);
        assert_eq!(body["user"]["userId"], "42");
    }

    #[tokio::test]
    async fn post_routes_require_the_csrf_header_and_a_role() {
        let (ctx, _dir) = test_ctx("csrf");
        let (sid, session) = ctx.sessions.create("42", "tester").unwrap();
        let cookie = format!("sid={sid}");

        let post = |uri: &str, csrf: Option<&str>| {
            let mut b = HttpRequest::builder()
                .method("POST")
                .uri(uri)
                .header(header::COOKIE, cookie.clone());
            if let Some(token) = csrf {
                b = b.header("x-csrf-token", token);
            }
            b.body(Body::from(r#"{"amount":"abc","userId":"7"}"#))
                .unwrap()
        };

        let res = call(&ctx, post("/api/wallet/pay", None)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_text(res).await, r#"{"error":"Invalid CSRF token."}"#);

        // A valid token with a bad number is a 400 with a message, not a panic.
        let res = call(&ctx, post("/api/wallet/pay", Some(&session.csrf_token))).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_text(res).await,
            r#"{"error":"amount must be a whole number."}"#
        );

        // A plain user is not an operator or admin.
        let res = call(
            &ctx,
            post("/api/operator/close-all", Some(&session.csrf_token)),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = call(&ctx, post("/api/admin/give", Some(&session.csrf_token))).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn oversized_bodies_are_rejected() {
        let (ctx, _dir) = test_ctx("big");
        let (sid, session) = ctx.sessions.create("42", "tester").unwrap();
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/api/bets")
            .header(header::COOKIE, format!("sid={sid}"))
            .header("x-csrf-token", session.csrf_token.clone())
            .body(Body::from(vec![b'a'; http::MAX_BODY_BYTES + 1]))
            .unwrap();
        let res = call(&ctx, req).await;
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn spa_routes_serve_index_html() {
        let (ctx, _dir) = test_ctx("spa");
        for path in [
            "/login",
            "/dashboard",
            "/operator/bracket-sorter",
            "/some/unknown/route",
        ] {
            let res = call(&ctx, get(path)).await;
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            assert_eq!(
                res.headers()[header::CONTENT_TYPE],
                "text/html; charset=utf-8"
            );
            assert_eq!(body_text(res).await, "<html>spa shell</html>");
        }
    }

    #[tokio::test]
    async fn static_files_and_missing_assets() {
        let (ctx, _dir) = test_ctx("static");
        let res = call(&ctx, get("/assets/app.js")).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=3600");

        let res = call(&ctx, get("/assets/missing.js")).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = call(&ctx, get("/%2e%2e/test.db")).await;
        assert_ne!(res.status(), StatusCode::OK);
        let res = call(&ctx, get("/..%5Ctest.db")).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn favicon_falls_back_to_svg() {
        let (ctx, _dir) = test_ctx("favicon");
        let res = call(&ctx, get("/favicon.ico")).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "image/svg+xml");
    }

    #[tokio::test]
    async fn root_redirects_by_session() {
        let (ctx, _dir) = test_ctx("root");
        let res = call(&ctx, get("/")).await;
        assert_eq!(res.status(), StatusCode::FOUND);
        assert_eq!(res.headers()[header::LOCATION], "/login");

        let (sid, _) = ctx.sessions.create("42", "tester").unwrap();
        let res = call(&ctx, with_cookie(get("/"), &format!("sid={sid}"))).await;
        assert_eq!(res.headers()[header::LOCATION], "/dashboard");
    }

    #[tokio::test]
    async fn oauth_login_sets_a_state_cookie() {
        let (ctx, _dir) = test_ctx("login");
        let res = call(&ctx, get("/auth/discord")).await;
        assert_eq!(res.status(), StatusCode::FOUND);
        let location = res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.starts_with("https://discord.com/oauth2/authorize?"));
        let cookie = res.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.starts_with("oauth_state="));
        assert!(cookie.contains("HttpOnly"));
        let state = cookie
            .trim_start_matches("oauth_state=")
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert!(location.contains(&format!("state={state}")));
    }

    #[tokio::test]
    async fn oauth_state_mismatch_is_400() {
        let (ctx, _dir) = test_ctx("state");
        // Wrong state.
        let res = call(
            &ctx,
            with_cookie(get("/auth/callback?code=abc&state=bad"), "oauth_state=good"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        // No state cookie at all.
        let res = call(&ctx, get("/auth/callback?code=abc&state=bad")).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        // No state in the query.
        let res = call(
            &ctx,
            with_cookie(get("/auth/callback?code=abc"), "oauth_state=good"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        // The state cookie is cleared either way.
        let res = call(
            &ctx,
            with_cookie(get("/auth/callback?code=abc&state=bad"), "oauth_state=good"),
        )
        .await;
        assert!(res.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0"));
    }

    #[tokio::test]
    async fn matching_state_without_a_code_is_400() {
        let (ctx, _dir) = test_ctx("nocode");
        let res = call(
            &ctx,
            with_cookie(get("/auth/callback?state=good"), "oauth_state=good"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_text(res).await, "Missing OAuth code");
    }

    #[tokio::test]
    async fn logout_clears_the_cookie_and_the_session() {
        let (ctx, _dir) = test_ctx("logout");
        let (sid, _) = ctx.sessions.create("42", "tester").unwrap();
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/auth/logout")
            .header(header::COOKIE, format!("sid={sid}"))
            .body(Body::empty())
            .unwrap();
        let res = call(&ctx, req).await;
        assert_eq!(res.status(), StatusCode::FOUND);
        assert_eq!(res.headers()[header::LOCATION], "/login");
        assert!(res.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0"));
        assert!(ctx.sessions.get(&sid).is_none());
        assert!(ctx.app.db.get_web_session(&sid).unwrap().is_none());
    }

    #[tokio::test]
    async fn sessions_survive_a_cache_miss() {
        let (ctx, _dir) = test_ctx("persist");
        let (sid, session) = ctx.sessions.create("42", "tester").unwrap();
        // A fresh store over the same database models a restart.
        let fresh = SessionStore::new(ctx.app.db.clone());
        let loaded = fresh.get(&sid).expect("session is persisted");
        assert_eq!(loaded.user_id, "42");
        assert_eq!(loaded.csrf_token, session.csrf_token);
    }
}
