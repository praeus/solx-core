//! `solx-surface` — the dependency-light foundation for the solx workspace.
//!
//! Holds the entity DTOs, the shared [`error::SolxError`]/[`error::Result`],
//! list/search wire types, path/name helpers, and the manager traits that form
//! the client/server seam. It deliberately depends only on serde/uuid/chrono/
//! async-trait so it can back both the local impls and a future HTTP
//! client/server.

pub mod entities;
pub mod error;
pub mod managers;
pub mod path;
pub mod query;

pub use error::{Result, SolxError};

pub use entities::{
    Action, ActionExecResult, ActionInput, ActionType, DocLink, Document, DocumentInput, FileRef,
    LinkKind, TypeEntity, TypeInput,
};
pub use managers::{ActionManager, DocManager, FileStore, Solx, TypeManager};
pub use query::{ListOptions, Page, SearchHit, SearchQuery, SearchResults, SortOrder};
