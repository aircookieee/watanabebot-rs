//! Per-guild link replacement configuration.
//! OWNER: Link-fixer agent. Define the schema and `impl Db` methods here.

use rusqlite::Connection;

#[allow(unused_imports)]
use super::Db;

pub(super) fn migrate(_conn: &Connection) -> anyhow::Result<()> {
    Ok(())
}
