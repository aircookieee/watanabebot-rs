//! Embed-fixer style link replacement: rewrites social links to embed-friendly mirrors,
//! deletes the original and reposts it as the author via a channel webhook.
//! OWNER: Link-fixer agent. STUB: replace with the real implementation.

use poise::serenity_prelude as serenity;

use crate::bot::{Data, Error};
use crate::db::Db;

pub struct LinkFixer {
    #[allow(dead_code)]
    db: Db,
}

impl LinkFixer {
    pub fn new(db: Db) -> Self {
        LinkFixer { db }
    }
}

pub async fn on_event(_ctx: &serenity::Context, _event: &serenity::FullEvent, _data: &Data) -> Result<(), Error> {
    Ok(())
}
