//! Tournaments, rounds, matches, bets, special markets.
//! OWNER: Economy/tournament agent. Add `impl Db` query methods here.

use rusqlite::Connection;

#[allow(unused_imports)]
use super::Db;

pub(super) fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS tournaments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            guild_id TEXT NOT NULL,
            name TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS tournament_rounds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tournament_id INTEGER NOT NULL,
            round_number INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(tournament_id, round_number),
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id)
        );
        CREATE TABLE IF NOT EXISTS tournament_matches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tournament_id INTEGER NOT NULL,
            match_number INTEGER NOT NULL,
            round_number INTEGER NOT NULL DEFAULT 1,
            contestant_a TEXT NOT NULL,
            contestant_b TEXT NOT NULL,
            winner TEXT,
            betting_open INTEGER NOT NULL DEFAULT 1,
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id)
        );
        CREATE TABLE IF NOT EXISTS bets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tournament_id INTEGER NOT NULL,
            match_id INTEGER NOT NULL,
            user_id TEXT NOT NULL,
            guild_id TEXT NOT NULL,
            picked TEXT NOT NULL,
            amount INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            payout INTEGER NOT NULL DEFAULT 0,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(tournament_id, match_id, user_id),
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id),
            FOREIGN KEY (match_id) REFERENCES tournament_matches(id)
        );
        CREATE TABLE IF NOT EXISTS tournament_special_markets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tournament_id INTEGER NOT NULL,
            type TEXT NOT NULL,
            round_number INTEGER,
            match_id INTEGER,
            status TEXT NOT NULL DEFAULT 'open',
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(tournament_id, type),
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id),
            FOREIGN KEY (match_id) REFERENCES tournament_matches(id)
        );
        CREATE TABLE IF NOT EXISTS special_bets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id INTEGER NOT NULL,
            tournament_id INTEGER NOT NULL,
            user_id TEXT NOT NULL,
            guild_id TEXT NOT NULL,
            picked TEXT NOT NULL,
            amount INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            payout INTEGER NOT NULL DEFAULT 0,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(market_id, user_id),
            FOREIGN KEY (market_id) REFERENCES tournament_special_markets(id),
            FOREIGN KEY (tournament_id) REFERENCES tournaments(id)
        );
        CREATE TABLE IF NOT EXISTS tournament_special_market_options (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id INTEGER NOT NULL,
            option_name TEXT NOT NULL,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(market_id, option_name),
            FOREIGN KEY (market_id) REFERENCES tournament_special_markets(id)
        );
        ",
    )?;
    // Very old databases predate rounds.
    super::add_column_if_missing(conn, "tournament_matches", "round_number", "INTEGER NOT NULL DEFAULT 1")?;
    Ok(())
}
