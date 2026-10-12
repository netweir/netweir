//! HTML parsing, CSS and XPath queries, and declarative extraction for
//! netweir.
//!
//! Parsing is done by lexbor, a C implementation of the HTML5 parsing spec.
//! The tree stays in lexbor's memory; [`Node`] is a borrowed handle into it.

pub mod date;
mod document;
pub mod extract;
mod ffi;
mod index;
pub mod js;
pub mod metadata;
pub mod price;
mod query;
pub mod track;
mod xpath;

pub use document::{Document, Node, NodeId, NodeKind, ParseTimeout};
pub use query::{Hit, Output, Query, QueryError};
pub use xpath::{XPath, XPathError, XValue};
