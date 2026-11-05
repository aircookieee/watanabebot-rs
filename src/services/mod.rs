//! Business logic and third-party API clients, independent of how they are invoked
//! (slash command, legacy `!` command, web dashboard, scheduled job).

pub mod anilist;
pub mod authz;
pub mod bracket_sorter;
pub mod facts;
pub mod gemini;
pub mod linkfix;
pub mod spotify;
pub mod tournaments;
pub mod wallets;
pub mod wordnik;
