//! HTML parsing, CSS and XPath queries, and declarative extraction for
//! netweir.
//!
//! Parsing is done by lexbor, a C implementation of the HTML5 parsing spec.
//! The tree stays in lexbor's memory; [`Node`] is a borrowed handle into it.

mod cache;
pub mod content;
pub mod date;
mod document;
pub mod extract;
mod ffi;
mod index;
pub mod js;
pub mod markdown;
pub mod metadata;
pub mod price;
mod query;
pub mod similar;
pub mod track;
mod xpath;

pub use cache::{QUERY_CACHE_SIZE, QUERY_CACHE_TEXT};
pub use document::{Content, Document, EditError, Node, NodeId, NodeKind, ParseTimeout, Reading};
pub use query::{Hit, OutOfMemory, Output, Query, QueryError};
pub use xpath::{XPath, XPathError, XValue};
