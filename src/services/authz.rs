//! Role checks shared by slash commands and the web dashboard.

use serde::Serialize;

use crate::config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    User,
    Operator,
    Admin,
}

pub fn user_role(user_id: u64) -> UserRole {
    let cfg = config::get();
    if cfg.admin_user_id.is_some_and(|id| id.get() == user_id) {
        UserRole::Admin
    } else if cfg.tournament_operator_id.is_some_and(|id| id.get() == user_id) {
        UserRole::Operator
    } else {
        UserRole::User
    }
}

pub fn can_manage_tournaments(user_id: u64) -> bool {
    matches!(user_role(user_id), UserRole::Admin | UserRole::Operator)
}

pub fn can_use_admin_tools(user_id: u64) -> bool {
    user_role(user_id) == UserRole::Admin
}
