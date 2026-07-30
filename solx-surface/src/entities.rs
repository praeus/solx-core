//! Entity data-transfer objects shared by the local impls and any future
//! client/server. These carry no backend types — only serde-friendly fields —
//! so they can cross a network boundary unchanged.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

fn default_now() -> DateTime<Utc> {
    Utc::now()
}

// ── Types ───────────────────────────────────────────────────────────────────

/// A registered type: a named JSON-schema at a path, optionally tagged with
/// type groups for faceted organization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeEntity {
    pub id: Uuid,
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: Value,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default = "default_now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_now")]
    pub updated_at: DateTime<Utc>,
}

/// Payload for `post type` (create-or-replace).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypeInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The JSON schema. Required on create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(default)]
    pub groups: Vec<String>,
}

// ── Files ─────────────────────────────────────────────────────────────────--

/// A reference to a file stored under the files folder, attached to a doc or
/// action. The bytes live on disk; only this row-side metadata is persisted in
/// the owning entity's database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRef {
    /// Display name of the file (e.g. `diagram.png`).
    pub name: String,
    /// Path relative to the files root, e.g. `files/docs/<id>/diagram.png`.
    pub rel_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

// ── Documents ────────────────────────────────────────────────────────────---

/// A link from a document either to another document (by full reference) or to
/// an external URL. (Actions are intentionally not linkable — see the design.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// `target` is another document's full reference (`/path/name`).
    DocRef,
    /// `target` is an external URL.
    Url,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocLink {
    pub kind: LinkKind,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: Uuid,
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Full reference of this document's type (`/types/.../Name`).
    pub type_ref: String,
    #[serde(default)]
    pub contents: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pub_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub links: Vec<DocLink>,
    #[serde(default)]
    pub files: Vec<FileRef>,
    #[serde(default = "default_now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_now")]
    pub updated_at: DateTime<Utc>,
}

/// Payload for `post doc` (create-or-replace).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocumentInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Full reference of the type. Required on create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_ref: Option<String>,
    #[serde(default)]
    pub contents: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pub_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub links: Vec<DocLink>,
    #[serde(default)]
    pub files: Vec<FileRef>,
}

// ── Actions ─────────────────────────────────────────────────────────────────

/// How an action is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    /// A built-in WASM component function (`bin_name` = artifact, `fn_name` = export).
    Wasm,
    /// An HTTP call — `fn_name` is the literal URL to POST to.
    Webhook,
    /// A shell command — `fn_name` is the literal command to execute.
    Command,
    /// Internal dispatcher handler — `fn_name` selects the operation
    /// (`oauth_start`, `oauth_await`, `oauth_stop`, etc.).
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub id: Uuid,
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub phrases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_type_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_type_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_type: Option<ActionType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fn_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_config: Option<Value>,
    #[serde(default)]
    pub files: Vec<FileRef>,
    /// Whether this action's WASM component runs in the full `backend-action`
    /// world (trusted — full entity/file/secret access) or the restricted
    /// `custom-action` world (untrusted — only action-exec/artifact-read).
    /// Only meaningful for `ActionType::Wasm`. Defaults to `false`; built-in
    /// seeded actions are the exception.
    #[serde(default)]
    pub trusted: bool,
    #[serde(default = "default_now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_now")]
    pub updated_at: DateTime<Utc>,
}

/// Payload for `post action` (create-or-replace).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub phrases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_type_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_type_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_type: Option<ActionType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fn_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_config: Option<Value>,
    #[serde(default)]
    pub files: Vec<FileRef>,
    /// `None` means "leave unchanged" on update / defaults to `false` on
    /// create — see [`Action::trusted`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted: Option<bool>,
}

/// Result of executing an action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionExecResult {
    pub action: String,
    pub result: Value,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
