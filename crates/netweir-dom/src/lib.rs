//! HTML and XML parsing, CSS and XPath queries, and declarative extraction
//! for netweir.
//!
//! HTML parsing is done by lexbor, a C implementation of the HTML5 parsing
//! spec. XML is read with quick-xml and built into a lexbor tree too (see
//! [`xml`]).
//! The tree stays in lexbor's memory; [`Node`] is a borrowed handle into it.

pub mod auto;
mod cache;
pub mod content;
pub mod css_path;
pub mod date;
mod document;
pub mod extract;
mod ffi;
pub mod gtin;
mod index;
pub mod js;
pub mod markdown;
pub mod metadata;
pub mod price;
mod query;
pub mod records;
pub mod similar;
pub mod track;
pub mod xml;
mod xpath;

/// The version of lexbor compiled in.
pub const LEXBOR_VERSION: &str = env!("LEXBOR_VERSION");

pub use cache::{QUERY_CACHE_SIZE, QUERY_CACHE_TEXT};
pub use document::{Content, Document, EditError, Node, NodeId, NodeKind, ParseTimeout, Reading};
pub use query::{Hit, OutOfMemory, Output, Query, QueryError};
pub use xml::{ParseXmlError, XmlError, XmlNodes};
pub use xpath::{XPath, XPathError, XValue};
