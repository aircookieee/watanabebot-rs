//! Environment-driven configuration. Mirrors the env vars used by the TypeScript bot
//! so an existing `.env` keeps working unchanged.

use std::path::PathBuf;

use once_cell::sync::OnceCell;
use serenity::all::{EmojiId, GuildId, UserId};

static CONFIG: OnceCell<Config> = OnceCell::new();

#[derive(Debug, Clone)]
pub struct Config {
    pub discord: DiscordConfig,
    pub spotify: SpotifyConfig,
    pub wordnik_api_key: String,
    pub gemini_api_key: String,
    /// Primary translation model (free tier). Override with GEMINI_MODEL.
    pub gemini_model: String,
    /// Used when the primary errors or is rate limited. Override with GEMINI_FALLBACK_MODEL.
    pub gemini_fallback_model: String,
    pub channels: ChannelConfig,
    pub paths: PathConfig,
    pub reactions: ReactionConfig,
    /// PluralKit's bot user id. Messages proxied by PluralKit arrive as webhooks.
    pub pluralkit_uid: u64,
    pub currency: CurrencyConfig,
    pub admin_user_id: Option<UserId>,
    pub tournament_operator_id: Option<UserId>,
    pub web: WebConfig,
    pub production: bool,
    pub allow_database_bootstrap: bool,
    pub legacy_anilist_map_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct DiscordConfig {
    pub token: String,
    pub client_id: String,
    /// Main guild: slash commands are registered here, web-login membership is checked here.
    pub guild_id: Option<GuildId>,
    pub oauth_client_secret: String,
    pub oauth_redirect_uri: String,
}

#[derive(Debug, Clone)]
pub struct SpotifyConfig {
    pub client_id: String,
    pub client_secret: String,
    pub playlist_id: String,
}

#[derive(Debug, Clone)]
pub struct ChannelConfig {
    pub musicart_webhook_id: Option<u64>,
    pub lovelive_music_channel_id: Option<u64>,
    pub daily_facts_channel_id: Option<u64>,
    pub tournament_updates_channel_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PathConfig {
    pub data_dir: PathBuf,
    pub database_path: PathBuf,
    pub resources_dir: PathBuf,
    pub web_dist_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ReactionConfig {
    pub watashi: EmojiId,
    pub smol_watashi: EmojiId,
    pub yes_watanabe: EmojiId,
}

#[derive(Debug, Clone)]
pub struct CurrencyConfig {
    /// The only guild where MugCoins / tournaments are enabled.
    pub guild_id: Option<GuildId>,
    pub starting_balance: i64,
    /// Coins granted for chatting, at most once per `message_reward_cooldown_secs`.
    pub message_reward: i64,
    pub message_reward_cooldown_secs: u64,
    pub daily_base: i64,
    pub daily_streak_bonus: i64,
    pub daily_streak_cap: i64,
    pub poll_vote_reward: i64,
}

#[derive(Debug, Clone)]
pub struct WebConfig {
    pub port: u16,
    pub base_url: String,
    pub session_secret: String,
    pub favicon_path: PathBuf,
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_u64(key: &str) -> Option<u64> {
    env_opt(key).and_then(|v| v.trim().parse().ok())
}

fn env_i64(key: &str, default: i64) -> i64 {
    env_opt(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        let data_dir = env_opt("DATA_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("data"));
        let resources_dir = env_opt("RESOURCES_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("resources"));
        let web_dist_dir = env_opt("WEB_DIST_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("dist-web"));
        let database_path = env_opt("DATABASE_PATH").map(PathBuf::from).unwrap_or_else(|| data_dir.join("watanabebot.db"));
        let port: u16 = env_opt("WEB_PORT").and_then(|p| p.parse().ok()).unwrap_or(3000);

        Config {
            discord: DiscordConfig {
                token: env("DISCORD_TOKEN"),
                client_id: env("DISCORD_CLIENT_ID"),
                guild_id: env_u64("DISCORD_GUILD_ID").map(GuildId::new),
                oauth_client_secret: env("DISCORD_OAUTH_CLIENT_SECRET"),
                oauth_redirect_uri: env("DISCORD_OAUTH_REDIRECT_URI"),
            },
            spotify: SpotifyConfig {
                client_id: env("SPOTIFY_CLIENT_ID"),
                client_secret: env("SPOTIFY_CLIENT_SECRET"),
                playlist_id: env("SPOTIFY_PLAYLIST_ID"),
            },
            wordnik_api_key: env("WORDNIK_API_KEY"),
            gemini_api_key: env("GEMINI_API_KEY"),
            gemini_model: env_opt("GEMINI_MODEL").unwrap_or_else(|| "gemini-3.8-flash".into()),
            gemini_fallback_model: env_opt("GEMINI_FALLBACK_MODEL").unwrap_or_else(|| "gemini-3.5-flash-lite".into()),
            channels: ChannelConfig {
                musicart_webhook_id: env_u64("CHANNEL_MUSICART_WEBHOOK_ID"),
                lovelive_music_channel_id: env_u64("CHANNEL_LOVELIVE_MUSIC_ID"),
                daily_facts_channel_id: env_u64("CHANNEL_DAILY_FACTS_ID"),
                tournament_updates_channel_id: env_u64("CHANNEL_TOURNAMENT_UPDATES_ID"),
            },
            paths: PathConfig {
                data_dir,
                database_path,
                resources_dir: resources_dir.clone(),
                web_dist_dir,
            },
            reactions: ReactionConfig {
                watashi: EmojiId::new(env_u64("EMOJI_WATASHI_ID").unwrap_or(1247638024372621442)),
                smol_watashi: EmojiId::new(env_u64("EMOJI_SMOL_WATASHI_ID").unwrap_or(1247638021482872944)),
                yes_watanabe: EmojiId::new(env_u64("EMOJI_YES_WATANABE_ID").unwrap_or(1247638018949644298)),
            },
            pluralkit_uid: 466378653216014359,
            currency: CurrencyConfig {
                guild_id: env_u64("CURRENCY_GUILD_ID").map(GuildId::new),
                starting_balance: env_i64("CURRENCY_STARTING_BALANCE", 500),
                message_reward: env_i64("CURRENCY_MESSAGE_REWARD", 1),
                message_reward_cooldown_secs: env_i64("CURRENCY_MESSAGE_REWARD_COOLDOWN_SECS", 60) as u64,
                daily_base: env_i64("CURRENCY_DAILY_BASE", 50),
                daily_streak_bonus: env_i64("CURRENCY_DAILY_STREAK_BONUS", 10),
                daily_streak_cap: env_i64("CURRENCY_DAILY_STREAK_CAP", 7),
                poll_vote_reward: env_i64("CURRENCY_POLL_VOTE_REWARD", 5),
            },
            admin_user_id: env_u64("ADMIN_USER_ID").map(UserId::new),
            tournament_operator_id: env_u64("TOURNAMENT_OPERATOR_ID").map(UserId::new),
            web: WebConfig {
                port,
                base_url: env_opt("WEB_BASE_URL").unwrap_or_else(|| format!("http://localhost:{port}")),
                session_secret: env_opt("SESSION_SECRET").unwrap_or_else(|| "dev-session-secret".into()),
                favicon_path: env_opt("WEB_FAVICON_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| resources_dir.join("yousoro2.png")),
            },
            production: env("NODE_ENV") == "production" || env("APP_ENV") == "production",
            allow_database_bootstrap: env("ALLOW_DATABASE_BOOTSTRAP") == "true",
            legacy_anilist_map_path: env_opt("LEGACY_ANILIST_MAP_PATH").map(PathBuf::from),
        }
    }

    pub fn resource(&self, name: &str) -> PathBuf {
        self.paths.resources_dir.join(name)
    }

    /// True if `guild` is the guild where MugCoins/tournaments are enabled.
    pub fn is_currency_guild(&self, guild: Option<GuildId>) -> bool {
        matches!((guild, self.currency.guild_id), (Some(g), Some(c)) if g == c)
    }
}

pub fn init() -> &'static Config {
    CONFIG.get_or_init(Config::from_env)
}

/// Global config accessor. Panics if `init()` was not called first (done in `main`).
pub fn get() -> &'static Config {
    CONFIG.get().expect("config::init() must be called first")
}
