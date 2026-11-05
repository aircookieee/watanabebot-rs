//! Slash commands. Each module owns its command tree and (optionally) an `on_event`
//! hook for component interactions it created (buttons, selects, modals).

pub mod anilist;
pub mod linkfix;
pub mod misc;
pub mod mugcoin;
pub mod tournament;
pub mod yousoro;

use super::{Data, Error};

pub fn all() -> Vec<poise::Command<Data, Error>> {
    let mut cmds = vec![
        yousoro::yousoro(),
        misc::define(),
        misc::pick(),
        mugcoin::mugcoin(),
        tournament::tournament(),
        linkfix::linkfix(),
    ];
    cmds.extend(anilist::commands());
    cmds
}
