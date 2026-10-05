//! Mapping between `actions` rows and [`Action`].

use chrono::{DateTime, Utc};
use libsql::Connection;
use serde_json::Value;
use solx_surface::entities::{Action, ActionType, FileRef};
use solx_surface::error::{Result, SolxError};
use uuid::Uuid;

use crate::db::map_db;

pub(crate) const SELECT: &str = "SELECT id,path,name,caption,description,capabilities,phrases,category,param_type_ref,result_type_ref,action_type,fn_name,bin_name,action_config,files,trusted,created_at,updated_at FROM actions";

pub(crate) fn opt(s: String) -> Option<String> {
    Some(s).filter(|v| !v.is_empty())
}

pub(crate) fn action_type_to_str(t: Option<ActionType>) -> String {
    match t {
        Some(ActionType::Wasm) => "wasm",
        Some(ActionType::Webhook) => "webhook",
        Some(ActionType::Command) => "command",
        Some(ActionType::Internal) => "internal",
        Some(ActionType::Script) => "script",
        None => "",
    }
    .to_string()
}

fn action_type_from_str(s: &str) -> Option<ActionType> {
    match s {
        "wasm" => Some(ActionType::Wasm),
        "webhook" => Some(ActionType::Webhook),
        "command" => Some(ActionType::Command),
        "internal" => Some(ActionType::Internal),
        "script" => Some(ActionType::Script),
        _ => None,
    }
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.into())
        .map_err(|e| SolxError::Db(e.to_string()))
}

pub(crate) fn row_to_action(row: &libsql::Row) -> Result<Action> {
    let id = Uuid::parse_str(&row.get::<String>(0).map_err(map_db)?)
        .map_err(|e| SolxError::Db(e.to_string()))?;
    let path: String = row.get(1).map_err(map_db)?;
    let name: String = row.get(2).map_err(map_db)?;
    let caption = opt(row.get::<String>(3).map_err(map_db)?);
    let description = opt(row.get::<String>(4).map_err(map_db)?);
    let capabilities: Vec<String> =
        serde_json::from_str(&row.get::<String>(5).map_err(map_db)?).unwrap_or_default();
    let phrases: Vec<String> =
        serde_json::from_str(&row.get::<String>(6).map_err(map_db)?).unwrap_or_default();
    let category = opt(row.get::<String>(7).map_err(map_db)?);
    let param_type_ref = opt(row.get::<String>(8).map_err(map_db)?);
    let result_type_ref = opt(row.get::<String>(9).map_err(map_db)?);
    let action_type = action_type_from_str(&row.get::<String>(10).map_err(map_db)?);
    let fn_name = opt(row.get::<String>(11).map_err(map_db)?);
    let bin_name = opt(row.get::<String>(12).map_err(map_db)?);
    let action_config: Option<Value> =
        serde_json::from_str(&row.get::<String>(13).map_err(map_db)?).unwrap_or(None);
    let files: Vec<FileRef> =
        serde_json::from_str(&row.get::<String>(14).map_err(map_db)?).unwrap_or_default();
    let trusted: bool = row.get::<i64>(15).map_err(map_db)? != 0;
    let created_at = parse_dt(&row.get::<String>(16).map_err(map_db)?)?;
    let updated_at = parse_dt(&row.get::<String>(17).map_err(map_db)?)?;
    Ok(Action {
        id,
        path,
        name,
        caption,
        description,
        capabilities,
        phrases,
        category,
        param_type_ref,
        result_type_ref,
        action_type,
        fn_name,
        bin_name,
        action_config,
        files,
        trusted,
        created_at,
        updated_at,
    })
}

pub(crate) async fn get_row(conn: &Connection, path: &str, name: &str) -> Result<Option<Action>> {
    let mut rows = conn
        .query(
            &format!("{SELECT} WHERE path=?1 AND name=?2"),
            libsql::params![path.to_string(), name.to_string()],
        )
        .await
        .map_err(map_db)?;
    match rows.next().await.map_err(map_db)? {
        Some(row) => Ok(Some(row_to_action(&row)?)),
        None => Ok(None),
    }
}
