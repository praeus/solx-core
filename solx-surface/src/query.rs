//! Wire types for list & search operations (pagination, filters, facets).

use serde::{Deserialize, Serialize};

/// Sort direction for list operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

/// Options for a paginated `list` over an entity database.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListOptions {
    /// Restrict to entities whose path is (or is under) this prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// LIKE filter on a column (e.g. `type_ref`, `author`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_value: Option<String>,
    /// Column to sort by (e.g. `name`, `created_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_order: SortOrder,
    /// Only rows with created_at >= this RFC 3339 timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_after: Option<String>,
    /// Only rows with created_at <= this RFC 3339 timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_before: Option<String>,
}

impl ListOptions {
    pub fn limit_or(&self, default: usize) -> usize {
        self.limit.unwrap_or(default)
    }
    pub fn offset_or_zero(&self) -> usize {
        self.offset.unwrap_or(0)
    }
}

/// A page of results plus the unpaginated total.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, total: usize, limit: usize, offset: usize) -> Self {
        Page { items, total, limit, offset }
    }
}

/// A full-text + faceted search query (documents).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Free-text query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Facet: restrict to a path prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Facet: restrict to a type reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_ref: Option<String>,
    /// Facet: restrict to documents whose contents contain a `DocRef` to this
    /// target, given as the target's full reference (`/path/name`, as
    /// returned by `solx get doc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

/// A single search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: String,
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub type_ref: String,
    pub score: f32,
}

/// Search results with the total match count for pagination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}
