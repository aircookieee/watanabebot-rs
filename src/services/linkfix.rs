//! Embed-fixer style link replacement: rewrites social links to embed-friendly mirrors.
//!
//! Two delivery modes:
//! - webhook (default): repost the message as the author through a channel webhook with the
//!   links replaced, then delete the original;
//! - reply: suppress embeds on the original and reply with the fixed links.
//!
//! The URL rewriting (`rewrite_content`) is pure and unit tested below.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail};
use once_cell::sync::Lazy;
use poise::serenity_prelude as serenity;
use regex::Regex;
use serenity::{
    ChannelId, CreateAllowedMentions, CreateAttachment, CreateMessage, CreateWebhook, EditMessage,
    ExecuteWebhook, FullEvent, GuildId, Message, MessageType, Permissions, Webhook,
};
use url::Url;

use crate::bot::{Data, Error};
use crate::db::linkfix::{FixMode, GuildLinkConfig};
use crate::db::Db;

const WEBHOOK_NAME: &str = "watanabebot linkfix";
const MAX_MESSAGE_CHARS: usize = 2000;
/// Largest total attachment size that is re-uploaded with a webhook repost.
const MAX_REUPLOAD_BYTES: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// Site registry
// ---------------------------------------------------------------------------------------------

/// A site whose links can be rewritten.
pub struct Site {
    pub id: &'static str,
    pub name: &'static str,
    pub default_on: bool,
    pub default_domain: &'static str,
    /// Known alternative replacement domains (any other hostname is accepted too).
    pub alternatives: &'static [&'static str],
    /// Source domains. A host matches if it equals one of them or is a subdomain of one.
    roots: &'static [&'static str],
    /// Regexes matched against `host + path` (lowercased host, anchored by the pattern itself).
    patterns: &'static [&'static str],
    /// The only query parameters kept on the rewritten link; everything else (tracking ids,
    /// share tokens, ...) is removed, as embed-fixer does.
    keep_params: &'static [&'static str],
    /// Twitch: `clips.twitch.tv/<id>` becomes `<domain>/clip/<id>`.
    clip_prefix: bool,
}

pub static SITES: &[Site] = &[
    Site {
        id: "twitter",
        name: "Twitter / X",
        default_on: true,
        default_domain: "fxtwitter.com",
        alternatives: &["vxtwitter.com", "fixupx.com", "fixvx.com", "xeezz.com"],
        roots: &["twitter.com", "x.com", "nitter.net"],
        patterns: &[
            r"^(?:www\.|mobile\.)?(?:twitter\.com|x\.com|nitter\.net)/[A-Za-z0-9_]+/status(?:es)?/\d+(?:/(?:photo|video)/\d+)?/?$",
            r"^(?:www\.|mobile\.)?(?:twitter\.com|x\.com)/i/(?:web/)?status/\d+/?$",
        ],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "instagram",
        name: "Instagram",
        default_on: true,
        default_domain: "kkinstagram.com",
        alternatives: &[
            "ddinstagram.com",
            "eeinstagram.com",
            "oginstagram.com",
            "zzinstagram.com",
            "fxig.seria.moe",
        ],
        roots: &["instagram.com"],
        patterns: &[
            r"^(?:www\.)?instagram\.com/(?:share/)?(?:p|reels?|tv)/[\w-]+/?$",
            r"^(?:www\.)?instagram\.com/share/[\w-]+/?$",
            r"^(?:www\.)?instagram\.com/[\w.]+/(?:p|reels?)/[\w-]+/?$",
            r"^(?:www\.)?instagram\.com/stories/[\w.]+/\d+/?$",
        ],
        // Selects the picture in a carousel post.
        keep_params: &["img_index"],
        clip_prefix: false,
    },
    Site {
        id: "tiktok",
        name: "TikTok",
        default_on: true,
        default_domain: "tnktok.com",
        alternatives: &["kktiktok.com", "tiktokez.com"],
        roots: &["tiktok.com"],
        patterns: &[
            r"^(?:www\.)?tiktok\.com/(?:t/\w+|@[\w.]+/(?:video|photo)/\d+)/?$",
            r"^v[mt]\.tiktok\.com/[\w-]+/?$",
        ],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "reddit",
        name: "Reddit",
        default_on: true,
        default_domain: "rxddit.com",
        alternatives: &["vxreddit.com", "fxreddit.seria.moe", "redditez.com"],
        roots: &["reddit.com"],
        patterns: &[
            r"^(?:www\.|old\.|np\.|new\.|m\.)?reddit\.com/(?:r|u|user)/[\w-]+/comments/\w+(?:/[^/]*){0,2}$",
            r"^(?:www\.|old\.|np\.|new\.|m\.)?reddit\.com/r/[\w-]+/s/\w+/?$",
        ],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "bluesky",
        name: "Bluesky",
        default_on: true,
        default_domain: "bskx.app",
        alternatives: &["fxbsky.app"],
        roots: &["bsky.app"],
        patterns: &[r"^(?:www\.)?bsky\.app/profile/[\w.:-]+/post/\w+/?$"],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "pixiv",
        name: "Pixiv",
        default_on: true,
        default_domain: "phixiv.net",
        alternatives: &[],
        roots: &["pixiv.net"],
        patterns: &[r"^(?:www\.)?pixiv\.net/(?:[a-z]{2}/)?(?:artworks|users)/\d+/?$"],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "threads",
        name: "Threads",
        default_on: true,
        default_domain: "fixthreads.net",
        alternatives: &["fixthreads.seria.moe", "vxthreads.net"],
        roots: &["threads.net", "threads.com"],
        patterns: &[
            r"^(?:www\.)?threads\.(?:net|com)/@[\w.]+/post/[\w-]+/?$",
            r"^(?:www\.)?threads\.(?:net|com)/share/[\w-]+/?$",
            r"^(?:www\.)?threads\.(?:net|com)/@[\w.]+/?$",
        ],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "tumblr",
        name: "Tumblr",
        default_on: false,
        default_domain: "tpmblr.com",
        alternatives: &[],
        roots: &["tumblr.com"],
        patterns: &[
            r"^(?:www\.)?tumblr\.com/[\w-]+/\d+(?:/[\w-]+)?/?$",
            r"^[\w-]+\.tumblr\.com/post/\d+(?:/[\w-]+)?/?$",
        ],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "deviantart",
        name: "DeviantArt",
        default_on: false,
        default_domain: "fixdeviantart.com",
        alternatives: &[],
        roots: &["deviantart.com"],
        patterns: &[r"^(?:www\.)?deviantart\.com/[\w.-]+/art/[\w.-]+-\d+/?$"],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "twitch",
        name: "Twitch clips",
        default_on: false,
        default_domain: "fxtwitch.seria.moe",
        alternatives: &[],
        roots: &["twitch.tv"],
        patterns: &[
            r"^clips\.twitch\.tv/[\w-]+/?$",
            r"^(?:www\.|m\.)?twitch\.tv/\w+/clip/[\w-]+/?$",
            r"^m\.twitch\.tv/clip/[\w-]+/?$",
        ],
        keep_params: &[],
        clip_prefix: true,
    },
    Site {
        id: "facebook",
        name: "Facebook",
        default_on: false,
        default_domain: "facebed.seria.moe",
        alternatives: &["fxfb.seria.moe", "facebookez.com"],
        roots: &["facebook.com"],
        patterns: &[
            r"^(?:www\.|web\.|m\.)?facebook\.com/(?:share/[rvp]/[\w-]+|reel/\d+|watch/?|photo/?|permalink\.php|story\.php|[\w.]+/(?:posts|videos|photos)/.+|marketplace/.+)$",
        ],
        // watch/?v=, permalink.php?story_fbid=&id=, photo/?fbid=&set= do not work without them.
        keep_params: &["v", "story_fbid", "id", "fbid", "set"],
        clip_prefix: false,
    },
    Site {
        id: "furaffinity",
        name: "Fur Affinity",
        default_on: false,
        default_domain: "xfuraffinity.net",
        alternatives: &["fxraffinity.net"],
        roots: &["furaffinity.net"],
        patterns: &[r"^(?:www\.)?furaffinity\.net/view/\d+/?$"],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "iwara",
        name: "Iwara",
        default_on: false,
        default_domain: "fxiwara.seria.moe",
        alternatives: &[],
        roots: &["iwara.tv"],
        patterns: &[r"^(?:www\.)?iwara\.tv/video/\w+(?:/[\w-]+)?/?$"],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "bilibili",
        name: "Bilibili",
        default_on: false,
        default_domain: "vxbilibili.com",
        alternatives: &["fxbilibili.seria.moe", "bilibiliez.com"],
        roots: &["bilibili.com"],
        patterns: &[
            r"^(?:www\.|m\.)?bilibili\.com/(?:video/\w+|opus/\d+)/?$",
            r"^t\.bilibili\.com/\d+/?$",
        ],
        // Video part number.
        keep_params: &["p"],
        clip_prefix: false,
    },
    Site {
        id: "pinterest",
        name: "Pinterest",
        default_on: false,
        default_domain: "pinterestez.com",
        alternatives: &[],
        roots: &["pinterest.com"],
        patterns: &[r"^(?:www\.)?pinterest\.com/pin/\d+/?$"],
        keep_params: &[],
        clip_prefix: false,
    },
    Site {
        id: "ptt",
        name: "PTT",
        default_on: false,
        default_domain: "fxptt.seria.moe",
        alternatives: &[],
        roots: &["ptt.cc"],
        patterns: &[r"^(?:www\.)?ptt\.cc/bbs/\w+/M\.\d+\.A\.\w+\.html$"],
        keep_params: &[],
        clip_prefix: false,
    },
];

static COMPILED: Lazy<Vec<Vec<Regex>>> = Lazy::new(|| {
    SITES
        .iter()
        .map(|s| {
            s.patterns
                .iter()
                .map(|p| Regex::new(p).expect("valid linkfix pattern"))
                .collect()
        })
        .collect()
});

/// Subdomains that are dropped when the host is swapped (everything else, such as `vm.` or
/// `clips.`, is kept).
const DROPPED_SUBDOMAINS: &[&str] = &["www", "m", "mobile", "old", "new", "np", "web"];
/// Tracking parameters removed from links rewritten by custom rules. Built-in sites instead keep
/// only their `keep_params`, since their URLs are known; a custom rule's site may need its query.
const TRACKING_PARAMS: &[&str] = &[
    "fbclid", "gclid", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "twclid", "ttclid", "li_fat_id",
    "mc_cid", "mc_eid", "igsh", "igshid", "si", "share_id", "ref_src", "ref_url", "_ga", "_gl", "s_cid",
];

/// Which query parameters survive a rewrite.
#[derive(Clone, Copy)]
enum QueryPolicy<'a> {
    /// Built-in site: keep only these.
    KeepOnly(&'a [&'a str]),
    /// Custom rule: keep everything except known tracking parameters.
    DropTracking,
}

pub fn find_site(query: &str) -> Option<&'static Site> {
    let q = query.trim().to_ascii_lowercase();
    SITES
        .iter()
        .find(|s| s.id == q || s.name.to_ascii_lowercase() == q)
}

/// (enabled, replacement domain) of a site for a guild.
pub fn site_state(cfg: &GuildLinkConfig, site: &Site) -> (bool, String) {
    let ov = cfg.sites.get(site.id);
    let enabled = ov.and_then(|o| o.enabled).unwrap_or(site.default_on);
    let domain = ov
        .and_then(|o| o.domain.clone())
        .unwrap_or_else(|| site.default_domain.to_string());
    (enabled, domain)
}

/// Lowercases and validates a hostname given by an admin. Accepts a pasted `https://host/`.
pub fn normalize_domain(input: &str) -> Option<String> {
    let mut s = input.trim().to_ascii_lowercase();
    for prefix in ["https://", "http://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    let s = s.trim_end_matches('/');
    if s.is_empty() || s.len() > 253 {
        return None;
    }
    let labels: Vec<&str> = s.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if !labels.iter().all(label_ok) {
        return None;
    }
    let tld = labels[labels.len() - 1];
    if tld.len() < 2 || tld.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(s.to_string())
}

// ---------------------------------------------------------------------------------------------
// URL rewriting
// ---------------------------------------------------------------------------------------------

/// Returns the subdomain part of `host` relative to `root` (`""` when equal).
fn subdomain_of<'a>(host: &'a str, root: &str) -> Option<&'a str> {
    if host == root {
        return Some("");
    }
    let rest = host.strip_suffix(root)?;
    rest.strip_suffix('.')
}

fn query_param_kept(key: &str, policy: QueryPolicy) -> bool {
    let key = key.to_ascii_lowercase();
    match policy {
        QueryPolicy::KeepOnly(keep) => keep.contains(&key.as_str()),
        QueryPolicy::DropTracking => !(TRACKING_PARAMS.contains(&key.as_str()) || key.starts_with("utm_")),
    }
}

/// Builds the rewritten URL: HTTPS, swapped host, filtered query.
fn build_fixed(
    url: &mut Url,
    sub: &str,
    domain: &str,
    policy: QueryPolicy,
    clip_prefix: bool,
) -> Option<String> {
    let mut sub = sub;
    if DROPPED_SUBDOMAINS.contains(&sub) {
        sub = "";
    }
    let mut clip = false;
    if clip_prefix && sub == "clips" {
        sub = "";
        clip = true;
    }
    let new_host = if sub.is_empty() {
        domain.to_string()
    } else {
        format!("{sub}.{domain}")
    };

    url.set_scheme("https").ok()?;
    url.set_host(Some(&new_host)).ok()?;
    url.set_port(None).ok()?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    if clip {
        let path = format!("/clip{}", url.path());
        url.set_path(&path);
    }
    if let Some(query) = url.query().map(str::to_string) {
        let kept: Vec<&str> = query
            .split('&')
            .filter(|p| !p.is_empty())
            .filter(|p| query_param_kept(p.split('=').next().unwrap_or(""), policy))
            .collect();
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.set_query(Some(&kept.join("&")));
        }
    }
    Some(url.to_string())
}

/// Rewrites a single URL according to the guild config, or returns `None` if it should be
/// left alone (unknown site, site disabled, already a fix domain, ...).
pub fn rewrite_url(raw: &str, cfg: &GuildLinkConfig) -> Option<String> {
    let mut url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();

    // Custom rules take priority over the built-in sites.
    for (from, to) in &cfg.rules {
        if let Some(sub) = subdomain_of(&host, from) {
            if subdomain_of(&host, to).is_some() || url.path().len() <= 1 {
                return None;
            }
            return build_fixed(&mut url, sub, to, QueryPolicy::DropTracking, false);
        }
    }

    let target = format!("{host}{}", url.path());
    for (idx, site) in SITES.iter().enumerate() {
        let Some(sub) = site.roots.iter().find_map(|root| subdomain_of(&host, root)) else {
            continue;
        };
        let (enabled, domain) = site_state(cfg, site);
        if !enabled || subdomain_of(&host, &domain).is_some() {
            continue;
        }
        if !COMPILED[idx].iter().any(|re| re.is_match(&target)) {
            continue;
        }
        return build_fixed(&mut url, sub, &domain, QueryPolicy::KeepOnly(site.keep_params), site.clip_prefix);
    }
    None
}

/// One replaced link.
#[derive(Debug, Clone)]
pub struct FixedLink {
    pub fixed: String,
    /// The original was wrapped in `||spoiler||` markers.
    pub spoiler: bool,
}

#[derive(Debug, Clone)]
pub struct Rewrite {
    /// The message text with every fixable link replaced.
    pub text: String,
    pub links: Vec<FixedLink>,
}

impl Rewrite {
    /// Fixed links in order of appearance, without duplicates.
    pub fn unique_links(&self) -> Vec<&FixedLink> {
        let mut seen: Vec<&str> = Vec::new();
        let mut out = Vec::new();
        for link in &self.links {
            if !seen.contains(&link.fixed.as_str()) {
                seen.push(&link.fixed);
                out.push(link);
            }
        }
        out
    }
}

static URL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"https?://[^\s<>|]+").unwrap());

/// Byte ranges that must not be touched: code spans, code blocks and `<...>` links.
fn skip_ranges(s: &str) -> Vec<Range<usize>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'`' => {
                let start = i;
                let mut run = 0;
                while i < b.len() && b[i] == b'`' {
                    run += 1;
                    i += 1;
                }
                // The span closes at the next backtick run of exactly the same length.
                let mut j = i;
                let mut end = None;
                while j < b.len() {
                    if b[j] == b'`' {
                        let mut n = 0;
                        while j < b.len() && b[j] == b'`' {
                            n += 1;
                            j += 1;
                        }
                        if n == run {
                            end = Some(j);
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                if let Some(end) = end {
                    out.push(start..end);
                    i = end;
                }
            }
            b'<' => {
                if s[i..].starts_with("<http") {
                    if let Some(rel) = s[i..].find('>') {
                        if !s[i..i + rel].contains(char::is_whitespace) {
                            out.push(i..i + rel + 1);
                            i += rel + 1;
                            continue;
                        }
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// Removes punctuation that belongs to the surrounding sentence or markdown, not the URL.
fn trim_url(url: &str) -> &str {
    let count = |s: &str, c: char| s.chars().filter(|&x| x == c).count();
    let mut s = url;
    while let Some(c) = s.chars().next_back() {
        let cut = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '*' | '~' => true,
            ')' => count(s, '(') < count(s, ')'),
            ']' => count(s, '[') < count(s, ']'),
            _ => false,
        };
        if !cut {
            break;
        }
        s = &s[..s.len() - c.len_utf8()];
    }
    s
}

/// Replaces every fixable link in `content`. Returns `None` if nothing changed.
pub fn rewrite_content(content: &str, cfg: &GuildLinkConfig) -> Option<Rewrite> {
    let skip = skip_ranges(content);
    // `||` markers outside code pair up in order, like Discord does; URLs never contain `|`.
    let markers: Vec<usize> =
        content.match_indices("||").map(|(i, _)| i).filter(|i| !skip.iter().any(|r| r.contains(i))).collect();
    let spoilers: Vec<Range<usize>> = markers.chunks_exact(2).map(|p| p[0] + 2..p[1]).collect();
    let mut text = String::with_capacity(content.len() + 32);
    let mut links = Vec::new();
    let mut last = 0;
    for m in URL_RE.find_iter(content) {
        if skip.iter().any(|r| r.contains(&m.start())) {
            continue;
        }
        let raw = trim_url(m.as_str());
        let end = m.start() + raw.len();
        let Some(fixed) = rewrite_url(raw, cfg) else {
            continue;
        };
        let spoiler = spoilers.iter().any(|r| r.contains(&m.start()));
        text.push_str(&content[last..m.start()]);
        text.push_str(&fixed);
        last = end;
        links.push(FixedLink { fixed, spoiler });
    }
    if links.is_empty() {
        return None;
    }
    text.push_str(&content[last..]);
    Some(Rewrite { text, links })
}

// ---------------------------------------------------------------------------------------------
// Runtime service
// ---------------------------------------------------------------------------------------------

pub struct LinkFixer {
    db: Db,
    configs: Mutex<HashMap<u64, Arc<GuildLinkConfig>>>,
    /// Bot-owned webhook per (non-thread) channel.
    webhooks: tokio::sync::Mutex<HashMap<ChannelId, Webhook>>,
}

impl LinkFixer {
    pub fn new(db: Db) -> Self {
        LinkFixer {
            db,
            configs: Mutex::new(HashMap::new()),
            webhooks: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Cached per-guild configuration.
    pub fn config(&self, guild_id: u64) -> anyhow::Result<Arc<GuildLinkConfig>> {
        if let Some(cfg) = self
            .configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&guild_id)
        {
            return Ok(cfg.clone());
        }
        let cfg = Arc::new(self.db.get_linkfix_config(guild_id)?);
        self.configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(guild_id, cfg.clone());
        Ok(cfg)
    }

    /// Must be called after any change to a guild's configuration.
    pub fn invalidate(&self, guild_id: u64) {
        self.configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&guild_id);
    }

    async fn forget_webhook(&self, channel: ChannelId) {
        self.webhooks.lock().await.remove(&channel);
    }

    /// Finds or creates the bot's webhook for a channel (cached in memory).
    async fn webhook_for(
        &self,
        ctx: &serenity::Context,
        channel: ChannelId,
    ) -> anyhow::Result<Webhook> {
        let mut cache = self.webhooks.lock().await;
        if let Some(w) = cache.get(&channel) {
            return Ok(w.clone());
        }
        let me = ctx.cache.current_user().id;
        let existing = channel.webhooks(&ctx.http).await?;
        let found = existing.into_iter().find(|w| {
            w.token.is_some()
                && w.name.as_deref() == Some(WEBHOOK_NAME)
                && w.user.as_ref().is_some_and(|u| u.id == me)
        });
        let webhook = match found {
            Some(w) => w,
            None => {
                channel
                    .create_webhook(&ctx.http, CreateWebhook::new(WEBHOOK_NAME))
                    .await?
            }
        };
        cache.insert(channel, webhook.clone());
        Ok(webhook)
    }
}

pub async fn on_event(
    ctx: &serenity::Context,
    event: &FullEvent,
    data: &Data,
) -> Result<(), Error> {
    match event {
        FullEvent::Message { new_message } => handle_message(ctx, data, new_message).await,
        FullEvent::WebhookUpdate {
            belongs_to_channel_id,
            ..
        } => {
            data.linkfix.forget_webhook(*belongs_to_channel_id).await;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Where a message lives: the channel that owns webhooks, and the thread id if it is in one.
struct Place {
    base: ChannelId,
    thread: Option<ChannelId>,
}

async fn resolve_place(ctx: &serenity::Context, guild_id: GuildId, channel_id: ChannelId) -> Place {
    // Some(Some(parent)) = thread, Some(None) = ordinary channel, None = not cached.
    let cached = ctx.cache.guild(guild_id).and_then(|g| {
        if let Some(t) = g.threads.iter().find(|t| t.id == channel_id) {
            Some(t.parent_id)
        } else if g.channels.contains_key(&channel_id) {
            Some(None)
        } else {
            None
        }
    });
    let parent = match cached {
        Some(parent) => parent,
        None => match channel_id.to_channel(ctx).await {
            Ok(serenity::Channel::Guild(c)) if c.thread_metadata.is_some() => c.parent_id,
            _ => None,
        },
    };
    match parent {
        Some(parent) => Place {
            base: parent,
            thread: Some(channel_id),
        },
        None => Place {
            base: channel_id,
            thread: None,
        },
    }
}

/// The bot's permissions in a channel, if the cache knows enough to compute them.
fn bot_permissions(
    ctx: &serenity::Context,
    guild_id: GuildId,
    channel: ChannelId,
) -> Option<Permissions> {
    let me = ctx.cache.current_user().id;
    let guild = ctx.cache.guild(guild_id)?;
    let channel = guild.channels.get(&channel)?;
    let member = guild.members.get(&me)?;
    Some(guild.user_permissions_in(channel, member))
}

async fn handle_message(ctx: &serenity::Context, data: &Data, msg: &Message) -> Result<(), Error> {
    let Some(guild_id) = msg.guild_id else {
        return Ok(());
    };
    if !msg.content.contains("http")
        || !matches!(msg.kind, MessageType::Regular | MessageType::InlineReply)
    {
        return Ok(());
    }
    // Only messages typed by real users: every bot and every webhook (including PluralKit
    // proxies and our own reposts) is left alone.
    if msg.author.bot || msg.webhook_id.is_some() {
        return Ok(());
    }

    let fixer = &data.linkfix;

    let cfg = fixer.config(guild_id.get())?;
    if !cfg.enabled || cfg.ignored.contains(&msg.channel_id.get()) {
        return Ok(());
    }
    let Some(rewrite) = rewrite_content(&msg.content, &cfg) else {
        return Ok(());
    };

    let place = resolve_place(ctx, guild_id, msg.channel_id).await;
    if place.thread.is_some() && cfg.ignored.contains(&place.base.get()) {
        return Ok(());
    }
    // Unknown permissions are assumed sufficient; a failing call falls back to reply mode.
    let perms = bot_permissions(ctx, guild_id, place.base).unwrap_or_else(Permissions::all);

    let can_webhook = perms.contains(Permissions::MANAGE_WEBHOOKS)
        && perms.contains(Permissions::MANAGE_MESSAGES);
    let use_webhook = cfg.mode == FixMode::Webhook
        && can_webhook
        && msg.poll.is_none()
        && msg.sticker_items.is_empty();

    if use_webhook {
        match repost_via_webhook(ctx, data, msg, guild_id, &place, &rewrite).await {
            Ok(()) => return Ok(()),
            Err(err) => tracing::warn!("linkfix webhook repost failed, replying instead: {err:#}"),
        }
    }
    reply_with_links(ctx, data, msg, guild_id, &perms, &rewrite).await
}

/// Display name used for the repost: guild nickname, then global name, then username.
fn display_name(msg: &Message) -> String {
    let name = msg
        .member
        .as_ref()
        .and_then(|m| m.nick.clone())
        .or_else(|| msg.author.global_name.clone())
        .unwrap_or_else(|| msg.author.name.clone());
    sanitize_webhook_username(&name)
}

/// Discord rejects webhook names containing "discord" or "clyde" and requires 2-80 characters.
fn sanitize_webhook_username(name: &str) -> String {
    let mut out: String = name.chars().take(80).collect();
    for banned in ["discord", "clyde"] {
        while let Some(pos) = out.to_ascii_lowercase().find(banned) {
            let split = pos + out[pos..].chars().next().map_or(1, char::len_utf8);
            out.insert(split, '\u{200b}');
            // The zero-width space breaks the match, so the loop ends once all are split.
        }
    }
    if out.trim().chars().count() < 2 {
        out = format!("{out}\u{200b}\u{200b}");
        if out.trim().is_empty() {
            out = "Unknown user".into();
        }
    }
    out
}

fn avatar_url(msg: &Message, guild_id: GuildId) -> String {
    if let Some(hash) = msg.member.as_ref().and_then(|m| m.avatar) {
        let hash = hash.to_string();
        let ext = if hash.starts_with("a_") {
            "gif"
        } else {
            "webp"
        };
        return format!(
            "https://cdn.discordapp.com/guilds/{guild_id}/users/{}/avatars/{hash}.{ext}",
            msg.author.id
        );
    }
    msg.author.face()
}

async fn download(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    Ok(bytes.to_vec())
}

async fn repost_via_webhook(
    ctx: &serenity::Context,
    data: &Data,
    msg: &Message,
    guild_id: GuildId,
    place: &Place,
    rewrite: &Rewrite,
) -> anyhow::Result<()> {
    if msg
        .message_reference
        .as_ref()
        .is_some_and(|r| r.kind != serenity::MessageReferenceKind::Default)
    {
        bail!("forwarded messages are not reposted");
    }

    let mut content = String::new();
    if msg.kind == MessageType::InlineReply {
        if let Some(reference) = &msg.message_reference {
            if let Some(replied_id) = reference.message_id {
                let channel = reference.channel_id;
                content.push_str(&format!(
                    "-# Replying to https://discord.com/channels/{guild_id}/{channel}/{replied_id}\n"
                ));
            }
        }
    }
    content.push_str(&rewrite.text);
    if content.chars().count() > MAX_MESSAGE_CHARS {
        bail!("message too long for a webhook repost");
    }

    // Attachments must be re-uploaded: the originals disappear with the deleted message.
    let mut files = Vec::new();
    if !msg.attachments.is_empty() {
        let total: u64 = msg.attachments.iter().map(|a| u64::from(a.size)).sum();
        if total > MAX_REUPLOAD_BYTES {
            bail!("attachments too large to re-upload");
        }
        for attachment in &msg.attachments {
            let bytes = download(&data.client, &attachment.url).await?;
            let mut file = CreateAttachment::bytes(bytes, attachment.filename.clone());
            if let Some(description) = &attachment.description {
                file = file.description(description.clone());
            }
            files.push(file);
        }
    }

    let mut builder = ExecuteWebhook::new()
        .content(content)
        .username(display_name(msg))
        .avatar_url(avatar_url(msg, guild_id))
        .allowed_mentions(CreateAllowedMentions::new())
        .files(files);
    if let Some(thread) = place.thread {
        builder = builder.in_thread(thread);
    }

    let fixer = &data.linkfix;
    let mut webhook = fixer.webhook_for(ctx, place.base).await?;
    let sent = match webhook.execute(&ctx.http, true, builder.clone()).await {
        Ok(sent) => sent,
        Err(first) => {
            // The cached webhook may have been deleted; look it up again once.
            tracing::debug!("webhook execute failed ({first}), refreshing webhook");
            fixer.forget_webhook(place.base).await;
            webhook = fixer.webhook_for(ctx, place.base).await?;
            webhook.execute(&ctx.http, true, builder).await?
        }
    };
    let sent = sent.ok_or_else(|| anyhow!("webhook returned no message"))?;

    if let Err(err) = msg.delete(ctx).await {
        // Keep exactly one copy: remove the repost and let the caller fall back to a reply.
        let _ = webhook
            .delete_message(&ctx.http, place.thread, sent.id)
            .await;
        bail!("could not delete the original: {err}");
    }
    if let Err(err) = data.db.record_linkfix_repost(
        sent.id.get(),
        msg.author.id.get(),
        msg.channel_id.get(),
        guild_id.get(),
    ) {
        tracing::warn!("could not record linkfix repost: {err:#}");
    }
    Ok(())
}

async fn reply_with_links(
    ctx: &serenity::Context,
    data: &Data,
    msg: &Message,
    guild_id: GuildId,
    perms: &Permissions,
    rewrite: &Rewrite,
) -> Result<(), Error> {
    let mut body = String::new();
    for link in rewrite.unique_links() {
        let line = if link.spoiler {
            format!("||{}||", link.fixed)
        } else {
            link.fixed.clone()
        };
        if body.chars().count() + line.chars().count() + 1 > MAX_MESSAGE_CHARS {
            break;
        }
        body.push_str(&line);
        body.push('\n');
    }
    if body.is_empty() {
        return Ok(());
    }

    if perms.contains(Permissions::MANAGE_MESSAGES) {
        if let Err(err) = msg
            .channel_id
            .edit_message(ctx, msg.id, EditMessage::new().suppress_embeds(true))
            .await
        {
            tracing::debug!("could not suppress embeds: {err}");
        }
    }

    let reply = CreateMessage::new()
        .content(body.trim_end())
        .reference_message(msg)
        .allowed_mentions(CreateAllowedMentions::new().replied_user(false));
    let sent = msg.channel_id.send_message(ctx, reply).await?;
    if let Err(err) = data.db.record_linkfix_repost(
        sent.id.get(),
        msg.author.id.get(),
        msg.channel_id.get(),
        guild_id.get(),
    ) {
        tracing::warn!("could not record linkfix reply: {err:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::linkfix::SiteOverride;

    fn fix(input: &str) -> Option<String> {
        rewrite_content(input, &GuildLinkConfig::default()).map(|r| r.text)
    }

    fn fix_with(input: &str, cfg: &GuildLinkConfig) -> Option<String> {
        rewrite_content(input, cfg).map(|r| r.text)
    }

    #[test]
    fn twitter_variants() {
        for host in [
            "x.com",
            "twitter.com",
            "www.twitter.com",
            "mobile.twitter.com",
            "nitter.net",
        ] {
            assert_eq!(
                fix(&format!("https://{host}/user_1/status/123456")).as_deref(),
                Some("https://fxtwitter.com/user_1/status/123456"),
                "{host}"
            );
        }
        assert_eq!(
            fix("https://x.com/user/status/1/photo/2").as_deref(),
            Some("https://fxtwitter.com/user/status/1/photo/2")
        );
        // Profiles and non-status pages are left alone.
        assert_eq!(fix("https://x.com/user"), None);
        assert_eq!(fix("https://x.com/home"), None);
    }

    #[test]
    fn twitter_query_is_stripped() {
        assert_eq!(
            fix("https://x.com/a/status/9?s=20&t=abc").as_deref(),
            Some("https://fxtwitter.com/a/status/9")
        );
        // Like embed-fixer, built-in sites keep no query parameters unless listed.
        assert_eq!(
            fix("https://x.com/a/status/9?s=20&lang=en").as_deref(),
            Some("https://fxtwitter.com/a/status/9")
        );
        // The fragment is not tracking data and is kept.
        assert_eq!(
            fix("https://x.com/a/status/9?s=20#m").as_deref(),
            Some("https://fxtwitter.com/a/status/9#m")
        );
    }

    #[test]
    fn unknown_parameters_are_stripped_but_needed_ones_kept() {
        assert_eq!(
            fix("https://www.instagram.com/p/Cabc/?img_index=3&igsh=a&hl=en").as_deref(),
            Some("https://kkinstagram.com/p/Cabc/?img_index=3")
        );
        assert_eq!(
            fix("https://bsky.app/profile/a.bsky.social/post/3k?ref_src=x&anything=1").as_deref(),
            Some("https://bskx.app/profile/a.bsky.social/post/3k")
        );
        let mut cfg = GuildLinkConfig::default();
        for site in ["facebook", "bilibili"] {
            cfg.sites.insert(site.into(), SiteOverride { enabled: Some(true), domain: None });
        }
        assert_eq!(
            fix_with("https://www.facebook.com/watch/?v=123&mibextid=abc&rdid=x", &cfg).as_deref(),
            Some("https://facebed.seria.moe/watch/?v=123")
        );
        assert_eq!(
            fix_with("https://www.facebook.com/permalink.php?story_fbid=1&id=2&ref=share", &cfg).as_deref(),
            Some("https://facebed.seria.moe/permalink.php?story_fbid=1&id=2")
        );
        assert_eq!(
            fix_with("https://www.bilibili.com/video/BV1xx?p=2&spm_id_from=333&vd_source=abc", &cfg).as_deref(),
            Some("https://vxbilibili.com/video/BV1xx?p=2")
        );
    }

    #[test]
    fn custom_rules_keep_query_minus_tracking() {
        let mut cfg = GuildLinkConfig::default();
        cfg.rules.push(("example.com".into(), "fixed.example.org".into()));
        assert_eq!(
            fix_with("https://example.com/watch?v=1&si=abc&fbclid=z&utm_medium=y&list=2", &cfg).as_deref(),
            Some("https://fixed.example.org/watch?v=1&list=2")
        );
    }

    #[test]
    fn instagram() {
        assert_eq!(
            fix("https://www.instagram.com/p/Cabc_12/?igsh=xyz").as_deref(),
            Some("https://kkinstagram.com/p/Cabc_12/")
        );
        assert_eq!(
            fix("https://instagram.com/reel/Cabc/?utm_source=ig_web&img_index=2").as_deref(),
            Some("https://kkinstagram.com/reel/Cabc/?img_index=2")
        );
        assert_eq!(
            fix("https://www.instagram.com/share/reel/BAbc123/").as_deref(),
            Some("https://kkinstagram.com/share/reel/BAbc123/")
        );
        assert_eq!(fix("https://www.instagram.com/someuser/"), None);
    }

    #[test]
    fn tiktok() {
        assert_eq!(
            fix("https://www.tiktok.com/@some.user/video/7123456789?_r=1&_t=abc").as_deref(),
            Some("https://tnktok.com/@some.user/video/7123456789")
        );
        assert_eq!(
            fix("https://vm.tiktok.com/ZMabc123/").as_deref(),
            Some("https://vm.tnktok.com/ZMabc123/")
        );
        assert_eq!(
            fix("https://vt.tiktok.com/ZSabc/").as_deref(),
            Some("https://vt.tnktok.com/ZSabc/")
        );
        assert_eq!(
            fix("https://www.tiktok.com/t/ZTabc/").as_deref(),
            Some("https://tnktok.com/t/ZTabc/")
        );
    }

    #[test]
    fn reddit() {
        assert_eq!(
            fix("https://www.reddit.com/r/rust/comments/abc123/some_title/").as_deref(),
            Some("https://rxddit.com/r/rust/comments/abc123/some_title/")
        );
        assert_eq!(
            fix("https://old.reddit.com/r/rust/comments/abc123/some_title/").as_deref(),
            Some("https://rxddit.com/r/rust/comments/abc123/some_title/")
        );
        assert_eq!(
            fix("https://reddit.com/r/rust/s/AbCdEf").as_deref(),
            Some("https://rxddit.com/r/rust/s/AbCdEf")
        );
        assert_eq!(
            fix("https://www.reddit.com/r/rust/comments/abc123/t/?utm_source=share&utm_medium=web")
                .as_deref(),
            Some("https://rxddit.com/r/rust/comments/abc123/t/")
        );
        assert_eq!(fix("https://www.reddit.com/r/rust/"), None);
    }

    #[test]
    fn bluesky_pixiv_threads() {
        assert_eq!(
            fix("https://bsky.app/profile/someone.bsky.social/post/3kabc").as_deref(),
            Some("https://bskx.app/profile/someone.bsky.social/post/3kabc")
        );
        assert_eq!(
            fix("https://www.pixiv.net/en/artworks/12345678").as_deref(),
            Some("https://phixiv.net/en/artworks/12345678")
        );
        assert_eq!(
            fix("https://www.threads.net/@user/post/Cabc123").as_deref(),
            Some("https://fixthreads.net/@user/post/Cabc123")
        );
        assert_eq!(
            fix("https://threads.com/@user/post/Cabc123?xmt=abc").as_deref(),
            Some("https://fixthreads.net/@user/post/Cabc123")
        );
    }

    #[test]
    fn off_by_default_sites_need_enabling() {
        let tumblr = "https://www.tumblr.com/someblog/123456789/post-slug";
        assert_eq!(fix(tumblr), None);
        let mut cfg = GuildLinkConfig::default();
        cfg.sites.insert(
            "tumblr".into(),
            SiteOverride {
                enabled: Some(true),
                domain: None,
            },
        );
        assert_eq!(
            fix_with(tumblr, &cfg).as_deref(),
            Some("https://tpmblr.com/someblog/123456789/post-slug")
        );

        let clip = "https://clips.twitch.tv/FunnyClipName-abc";
        assert_eq!(fix(clip), None);
        cfg.sites.insert(
            "twitch".into(),
            SiteOverride {
                enabled: Some(true),
                domain: None,
            },
        );
        assert_eq!(
            fix_with(clip, &cfg).as_deref(),
            Some("https://fxtwitch.seria.moe/clip/FunnyClipName-abc")
        );
    }

    #[test]
    fn site_can_be_disabled_or_use_other_domain() {
        let mut cfg = GuildLinkConfig::default();
        cfg.sites.insert(
            "twitter".into(),
            SiteOverride {
                enabled: None,
                domain: Some("fixupx.com".into()),
            },
        );
        assert_eq!(
            fix_with("https://x.com/a/status/1", &cfg).as_deref(),
            Some("https://fixupx.com/a/status/1")
        );
        cfg.sites.insert(
            "twitter".into(),
            SiteOverride {
                enabled: Some(false),
                domain: None,
            },
        );
        assert_eq!(fix_with("https://x.com/a/status/1", &cfg), None);
    }

    #[test]
    fn already_fixed_links_are_untouched() {
        assert_eq!(fix("https://fxtwitter.com/a/status/1"), None);
        assert_eq!(fix("https://vxtwitter.com/a/status/1"), None);
        assert_eq!(fix("https://kkinstagram.com/p/abc/"), None);
        assert_eq!(fix("https://rxddit.com/r/rust/comments/abc/x/"), None);
        assert_eq!(fix("https://tnktok.com/@u/video/1"), None);
        assert_eq!(fix("https://bskx.app/profile/a/post/b"), None);
    }

    #[test]
    fn angle_brackets_and_code_are_skipped() {
        assert_eq!(fix("<https://x.com/a/status/1>"), None);
        assert_eq!(fix("`https://x.com/a/status/1`"), None);
        assert_eq!(fix("``https://x.com/a/status/1``"), None);
        assert_eq!(fix("```\nhttps://x.com/a/status/1\n```"), None);
        assert_eq!(
            fix("<https://x.com/a/status/1> https://x.com/b/status/2").as_deref(),
            Some("<https://x.com/a/status/1> https://fxtwitter.com/b/status/2")
        );
        assert_eq!(
            fix("`code` https://x.com/b/status/2").as_deref(),
            Some("`code` https://fxtwitter.com/b/status/2")
        );
        // An unclosed backtick does not hide the link.
        assert_eq!(
            fix("a ` https://x.com/b/status/2").as_deref(),
            Some("a ` https://fxtwitter.com/b/status/2")
        );
    }

    #[test]
    fn spoilers_are_preserved() {
        let rw = rewrite_content(
            "look ||https://x.com/a/status/1|| wow",
            &GuildLinkConfig::default(),
        )
        .unwrap();
        assert_eq!(rw.text, "look ||https://fxtwitter.com/a/status/1|| wow");
        assert!(rw.links[0].spoiler);
        let rw = rewrite_content("https://x.com/a/status/1", &GuildLinkConfig::default()).unwrap();
        assert!(!rw.links[0].spoiler);
        let rw = rewrite_content(
            "|| look https://x.com/a/status/1 ||\n|| `||` || https://x.com/b/status/2",
            &GuildLinkConfig::default(),
        )
        .unwrap();
        assert!(rw.links[0].spoiler);
        assert!(!rw.links[1].spoiler);
    }

    #[test]
    fn surrounding_text_and_punctuation() {
        assert_eq!(
            fix("see https://x.com/a/status/1, and (https://x.com/b/status/2).").as_deref(),
            Some("see https://fxtwitter.com/a/status/1, and (https://fxtwitter.com/b/status/2).")
        );
        assert_eq!(
            fix("[tweet](https://x.com/a/status/1)").as_deref(),
            Some("[tweet](https://fxtwitter.com/a/status/1)")
        );
        assert_eq!(
            fix("**https://x.com/a/status/1**").as_deref(),
            Some("**https://fxtwitter.com/a/status/1**")
        );
        assert_eq!(fix("no links here"), None);
        assert_eq!(fix("https://example.com/page"), None);
    }

    #[test]
    fn unique_links_are_deduplicated() {
        let rw = rewrite_content(
            "https://x.com/a/status/1 https://twitter.com/a/status/1 https://x.com/b/status/2",
            &GuildLinkConfig::default(),
        )
        .unwrap();
        assert_eq!(rw.links.len(), 3);
        let unique: Vec<&str> = rw.unique_links().iter().map(|l| l.fixed.as_str()).collect();
        assert_eq!(
            unique,
            [
                "https://fxtwitter.com/a/status/1",
                "https://fxtwitter.com/b/status/2"
            ]
        );
    }

    #[test]
    fn custom_rules() {
        let mut cfg = GuildLinkConfig::default();
        cfg.rules
            .push(("example.com".into(), "fixed.example.org".into()));
        assert_eq!(
            fix_with("https://www.example.com/path/1?utm_source=x&id=5", &cfg).as_deref(),
            Some("https://fixed.example.org/path/1?id=5")
        );
        assert_eq!(
            fix_with("https://sub.example.com/a", &cfg).as_deref(),
            Some("https://sub.fixed.example.org/a")
        );
        // Bare homepages and already fixed hosts are skipped.
        assert_eq!(fix_with("https://example.com/", &cfg), None);
        assert_eq!(fix_with("https://fixed.example.org/a", &cfg), None);
        assert_eq!(fix_with("https://notexample.com/a", &cfg), None);
    }

    #[test]
    fn custom_rule_overrides_builtin_site() {
        let mut cfg = GuildLinkConfig::default();
        cfg.rules.push(("x.com".into(), "my.mirror.net".into()));
        assert_eq!(
            fix_with("https://x.com/a/status/1", &cfg).as_deref(),
            Some("https://my.mirror.net/a/status/1")
        );
    }

    #[test]
    fn domain_validation() {
        assert_eq!(
            normalize_domain(" FxTwitter.COM ").as_deref(),
            Some("fxtwitter.com")
        );
        assert_eq!(
            normalize_domain("https://fixupx.com/").as_deref(),
            Some("fixupx.com")
        );
        assert_eq!(
            normalize_domain("fixthreads.seria.moe").as_deref(),
            Some("fixthreads.seria.moe")
        );
        assert_eq!(normalize_domain("localhost"), None);
        assert_eq!(normalize_domain("exa mple.com"), None);
        assert_eq!(normalize_domain("example.com/path"), None);
        assert_eq!(normalize_domain("-bad.com"), None);
        assert_eq!(normalize_domain("1.2.3.4"), None);
        assert_eq!(normalize_domain(""), None);
    }

    #[test]
    fn webhook_username_sanitizing() {
        assert_eq!(sanitize_webhook_username("Alice"), "Alice");
        let s = sanitize_webhook_username("MyDiscordBot");
        assert!(!s.to_ascii_lowercase().contains("discord"));
        assert!(s.chars().count() >= 2);
        assert!(sanitize_webhook_username("a").chars().count() >= 2);
    }

    #[test]
    fn site_lookup() {
        assert_eq!(find_site("Twitter").map(|s| s.id), Some("twitter"));
        assert_eq!(find_site("twitter / x").map(|s| s.id), Some("twitter"));
        assert!(find_site("nope").is_none());
        // Every pattern compiles and ids are unique.
        assert_eq!(COMPILED.len(), SITES.len());
        for (i, a) in SITES.iter().enumerate() {
            assert!(SITES.iter().skip(i + 1).all(|b| b.id != a.id));
        }
    }
}
