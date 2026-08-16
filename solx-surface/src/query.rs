//! Wire types for list & search operations (pagination, filters, facets),
//! plus the shared SQL rendering for [`ListOptions`].

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::path::normalize_path;

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

// ---------- ListSchema / ListSql ----------

/// Declares which columns a store exposes to the [`ListOptions`] knobs.
///
/// `filter_field` and `sort_by` are caller-supplied strings, so nothing from
/// the request is ever interpolated into SQL. A field is looked *up* in these
/// tables and the matched `&'static str` is what reaches the query; anything
/// unrecognized is ignored. That keeps the whitelist and the SQL in one place
/// instead of one hand-rolled `match` per store.
#[derive(Debug, Clone, Copy)]
pub struct ListSchema<'a> {
    /// Columns `filter_field` may target. Matching is a case-insensitive
    /// substring (`LIKE %value%`) on the named column.
    pub filterable: &'a [&'a str],
    /// (`sort_by` key, `ORDER BY` expression) pairs. The expression may name
    /// several comma-separated columns.
    pub sortable: &'a [(&'a str, &'a str)],
    /// `ORDER BY` expression used when `sort_by` is absent or unrecognized.
    pub default_sort: &'a str,
    /// Column bounded by `date_after`/`date_before`; `None` if unsupported.
    pub date_column: Option<&'a str>,
}

/// A rendered [`ListOptions`]: SQL fragments plus their positional binds.
///
/// Every bind is a string, which is what keeps this renderer free of any
/// database dependency — callers convert to their driver's value type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListSql {
    /// Empty, or `" WHERE …"`.
    pub where_clause: String,
    /// `" ORDER BY … ASC|DESC"`, always non-empty.
    pub order_clause: String,
    /// Positional binds for `where_clause`, in `?1..?n` order.
    pub binds: Vec<String>,
}

impl ListOptions {
    /// Render the filter and sort knobs against a store's [`ListSchema`].
    ///
    /// Fails only if `path_prefix` is not a valid path.
    pub fn to_sql(&self, schema: ListSchema<'_>) -> Result<ListSql> {
        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();

        // Path prefix: the directory itself plus everything beneath it.
        if let Some(prefix) = &self.path_prefix {
            let p = normalize_path(prefix)?;
            if p == "/" {
                binds.push("/%".to_string());
                conditions.push(format!("path LIKE ?{}", binds.len()));
            } else {
                binds.push(p.clone());
                let exact = binds.len();
                binds.push(format!("{p}/%"));
                let under = binds.len();
                conditions.push(format!("(path=?{exact} OR path LIKE ?{under})"));
            }
        }

        // Date range, for stores that declare a column for it.
        if let Some(col) = schema.date_column {
            if let Some(after) = &self.date_after {
                binds.push(after.clone());
                conditions.push(format!("{col} >= ?{}", binds.len()));
            }
            if let Some(before) = &self.date_before {
                binds.push(before.clone());
                conditions.push(format!("{col} <= ?{}", binds.len()));
            }
        }

        // Substring filter on a whitelisted column.
        if let (Some(field), Some(value)) = (&self.filter_field, &self.filter_value) {
            if !value.is_empty() {
                if let Some(col) = schema.filterable.iter().find(|c| **c == field.as_str()) {
                    binds.push(format!("%{value}%"));
                    conditions.push(format!(
                        "lower(COALESCE({col},'')) LIKE lower(?{})",
                        binds.len()
                    ));
                }
            }
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        let sort_expr = self
            .sort_by
            .as_deref()
            .and_then(|key| {
                schema
                    .sortable
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, sql)| *sql)
            })
            .unwrap_or(schema.default_sort);

        let dir = match self.sort_order {
            SortOrder::Desc => "DESC",
            SortOrder::Asc => "ASC",
        };

        // The direction applies to *every* column in the expression. Writing
        // `ORDER BY path,name DESC` would sort only `name` descending and
        // silently leave `path` ascending.
        let mut parts: Vec<String> = sort_expr
            .split(',')
            .map(|c| format!("{} {dir}", c.trim()))
            .collect();

        // Deterministic pagination: LIMIT/OFFSET over a non-unique sort key
        // (updated_at, category, …) can otherwise repeat or drop rows between
        // pages, so fall back to the unique (path, name) pair.
        for tie in ["path", "name"] {
            if !sort_expr.split(',').any(|c| c.trim() == tie) {
                parts.push(format!("{tie} ASC"));
            }
        }

        Ok(ListSql {
            where_clause,
            order_clause: format!(" ORDER BY {}", parts.join(", ")),
            binds,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: ListSchema<'static> = ListSchema {
        filterable: &["name", "title", "type_ref"],
        sortable: &[
            ("name", "path,name"),
            ("path", "path,name"),
            ("updated_at", "updated_at"),
        ],
        default_sort: "path,name",
        date_column: Some("created_at"),
    };

    fn opts() -> ListOptions {
        ListOptions::default()
    }

    #[test]
    fn empty_options_render_only_a_sort() {
        let sql = opts().to_sql(SCHEMA).unwrap();
        assert_eq!(sql.where_clause, "");
        assert_eq!(sql.order_clause, " ORDER BY path ASC, name ASC");
        assert!(sql.binds.is_empty());
    }

    #[test]
    fn path_prefix_matches_the_dir_and_its_descendants() {
        let sql = ListOptions { path_prefix: Some("/blogs".into()), ..opts() }
            .to_sql(SCHEMA)
            .unwrap();
        assert_eq!(sql.where_clause, " WHERE (path=?1 OR path LIKE ?2)");
        assert_eq!(sql.binds, vec!["/blogs".to_string(), "/blogs/%".to_string()]);
    }

    #[test]
    fn root_prefix_matches_everything() {
        let sql = ListOptions { path_prefix: Some("/".into()), ..opts() }
            .to_sql(SCHEMA)
            .unwrap();
        assert_eq!(sql.where_clause, " WHERE path LIKE ?1");
        assert_eq!(sql.binds, vec!["/%".to_string()]);
    }

    #[test]
    fn invalid_path_prefix_is_rejected() {
        assert!(ListOptions { path_prefix: Some("/a/../b".into()), ..opts() }
            .to_sql(SCHEMA)
            .is_err());
    }

    #[test]
    fn filter_targets_a_whitelisted_column() {
        let sql = ListOptions {
            filter_field: Some("title".into()),
            filter_value: Some("michigan".into()),
            ..opts()
        }
        .to_sql(SCHEMA)
        .unwrap();
        assert_eq!(
            sql.where_clause,
            " WHERE lower(COALESCE(title,'')) LIKE lower(?1)"
        );
        assert_eq!(sql.binds, vec!["%michigan%".to_string()]);
    }

    #[test]
    fn unknown_filter_field_is_ignored_not_interpolated() {
        // The guard that keeps caller-supplied identifiers out of the SQL.
        let sql = ListOptions {
            filter_field: Some("password) OR 1=1 --".into()),
            filter_value: Some("x".into()),
            ..opts()
        }
        .to_sql(SCHEMA)
        .unwrap();
        assert_eq!(sql.where_clause, "");
        assert!(sql.binds.is_empty());
    }

    #[test]
    fn filter_value_is_bound_never_inlined() {
        let sql = ListOptions {
            filter_field: Some("name".into()),
            filter_value: Some("'; DROP TABLE docs; --".into()),
            ..opts()
        }
        .to_sql(SCHEMA)
        .unwrap();
        assert!(!sql.where_clause.contains("DROP"));
        assert_eq!(sql.binds, vec!["%'; DROP TABLE docs; --%".to_string()]);
    }

    #[test]
    fn empty_filter_value_is_a_no_op() {
        let sql = ListOptions {
            filter_field: Some("name".into()),
            filter_value: Some(String::new()),
            ..opts()
        }
        .to_sql(SCHEMA)
        .unwrap();
        assert_eq!(sql.where_clause, "");
    }

    #[test]
    fn direction_applies_to_every_sort_column() {
        // `ORDER BY path,name DESC` would sort only `name` descending.
        let sql = ListOptions {
            sort_by: Some("name".into()),
            sort_order: SortOrder::Desc,
            ..opts()
        }
        .to_sql(SCHEMA)
        .unwrap();
        assert_eq!(sql.order_clause, " ORDER BY path DESC, name DESC");
    }

    #[test]
    fn non_unique_sort_gets_a_stable_tiebreak() {
        let sql = ListOptions {
            sort_by: Some("updated_at".into()),
            sort_order: SortOrder::Desc,
            ..opts()
        }
        .to_sql(SCHEMA)
        .unwrap();
        assert_eq!(
            sql.order_clause,
            " ORDER BY updated_at DESC, path ASC, name ASC"
        );
    }

    #[test]
    fn unknown_sort_key_falls_back_to_the_default() {
        let sql = ListOptions { sort_by: Some("; DROP TABLE".into()), ..opts() }
            .to_sql(SCHEMA)
            .unwrap();
        assert_eq!(sql.order_clause, " ORDER BY path ASC, name ASC");
    }

    #[test]
    fn date_bounds_apply_when_the_store_declares_a_column() {
        let sql = ListOptions {
            date_after: Some("2024-01-01".into()),
            date_before: Some("2024-12-31".into()),
            ..opts()
        }
        .to_sql(SCHEMA)
        .unwrap();
        assert_eq!(
            sql.where_clause,
            " WHERE created_at >= ?1 AND created_at <= ?2"
        );
    }

    #[test]
    fn date_bounds_are_dropped_when_unsupported() {
        let schema = ListSchema { date_column: None, ..SCHEMA };
        let sql = ListOptions { date_after: Some("2024-01-01".into()), ..opts() }
            .to_sql(schema)
            .unwrap();
        assert_eq!(sql.where_clause, "");
    }

    #[test]
    fn every_filter_composes_with_sequential_binds() {
        let sql = ListOptions {
            path_prefix: Some("/blogs".into()),
            date_after: Some("2024-01-01".into()),
            filter_field: Some("type_ref".into()),
            filter_value: Some("Post".into()),
            sort_by: Some("updated_at".into()),
            ..opts()
        }
        .to_sql(SCHEMA)
        .unwrap();
        assert_eq!(
            sql.where_clause,
            " WHERE (path=?1 OR path LIKE ?2) AND created_at >= ?3 \
             AND lower(COALESCE(type_ref,'')) LIKE lower(?4)"
        );
        assert_eq!(sql.binds.len(), 4);
    }
}
