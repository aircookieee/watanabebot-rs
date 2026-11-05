# Mugcord's you-chan! (Rust)

![You-chan](https://files.catbox.moe/5fvr0c.png)

A cute Discord bot built for one very specific server, rewritten from TypeScript to Rust.
It reacts with emojis of You Watanabe from Love Live! Sunshine!! when someone accidentally
summons her, and runs the server's AniList lookups, link fixing, MugCoin economy and anime
tournament betting.

## Features

- **Reactions**: reacts to "You", "you-chan" and "(yes Watanabe)", and to direct `@you-chan`
  pings. `@everyone`, `@here`, role pings and reply pings never trigger a reaction.
  `/yousoro here` limits keyword reactions to one channel, `/yousoro everywhere` undoes it.
- **Link fixing** (embed-fixer style): Twitter/X, Instagram, TikTok, Reddit, Bluesky, Pixiv and
  Threads links are rewritten to embed-friendly mirrors by default, with more sites available.
  The original message is deleted and reposted through a webhook under the author's name and
  avatar, with pings disabled. When that is not possible (missing permissions, PluralKit
  messages, polls, very long messages) the bot replies with the fixed links instead.
  Configure with `/linkfix` (sites, replacement domains, webhook/reply mode, ignored channels,
  custom rules). Authors can remove a repost with the "Delete fixed message" context menu.
- **AniList**: `/anime`, `/manga`, `/anilist register|unregister|update`, plus `!al`, `!alm`,
  `((anime))` and `[[manga]]` inline lookups showing everyone's scores and progress.
- **MugCoins**: chat rewards, `/mugcoin balance|pay|leaderboard|daily|stats`, admin
  set/give/take. Daily claims build a streak bonus (UTC days). The leaderboard can rank by
  balance or betting profit.
- **Tournaments**: bracket upload, parimutuel betting on matches, tournament-winner and
  runner-up futures, round advancement.
  - `/tournament poll` posts a Discord poll per match and resolves the match automatically
    when the poll ends (ties are left for an operator). Voters earn a one-time reward.
  - `/tournament resolve-from-poll` resolves a match from any poll message link.
- **Daily posts**: a Love Live song from a Spotify playlist and a random fact.
- **Translation**: Japanese tweets from the music/art webhook are translated with Gemini
  (`gemini-3.8-flash`, falling back to `gemini-3.5-flash-lite`; both work on the free tier).
- **Web dashboard**: Discord login, betting, wallet, leaderboard, operator and admin panels.

## Running

### Docker (recommended)

```bash
cp .env.example .env   # then fill it in
docker compose up -d --build
```

The SQLite database lives in `./data`. On first boot in production with an empty `./data`,
the bot refuses to create a new database unless `ALLOW_DATABASE_BOOTSTRAP=true` is set once;
this protects wallets and bets if the volume is not mounted. A backup is written to
`data/backups/` on every start.

### Locally

Requires Rust (stable) and, to rebuild the dashboard, Node.js.

```bash
npm install && npm run build   # optional: rebuilds dist-web/ from web/
cargo run --release
```

### Migrating from the TypeScript bot

Point `DATA_DIR` (or mount the volume) at the old `data/` directory. The existing
`watanabebot.db` is used in place: the schema is the same and new features only add tables.
The `.env` variable names are unchanged (`SESSION_SECRET` is no longer used).

The Rust bot runs SQLite in WAL mode, so the live `watanabebot.db` may not contain the latest
writes on its own. To roll back to the TypeScript bot, use the newest file in `data/backups/`
(each one is a complete, self-contained copy taken at startup), not the live file.

### Discord setup

- Privileged intent: Message Content.
- Bot permissions: View Channels, Send Messages, Send Messages in Threads, Embed Links,
  Attach Files, Read Message History, Add Reactions, Manage Messages and Manage Webhooks
  (the last two for link-fix reposts), Send Polls.

## Credits

[README art](https://x.com/i/web/status/1150801061872930816) by wasteman800.
Thanks to stuff#6454 for the small You neso picture and feature inspiration.
Link fixing is modelled on [embed-fixer](https://github.com/seriaati/embed-fixer).
