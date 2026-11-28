//! HTML and XML parsing, CSS and XPath queries, and declarative extraction
//! for netweir.
//!
//! HTML is parsed by Parsec's parser for the HTML Standard
//! (parsec-web-core, named netweir-html here). XML is read with quick-xml
//! and built into the same kind of tree (see [`xml`]). [`Node`] is a
//! borrowed handle into the tree.

pub mod auto;
mod cache;
pub mod chunk;
pub mod content;
pub mod css_path;
pub mod date;
mod document;
pub mod extract;
pub mod gtin;
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

/// The HTML parser's name and version, as `netweir version` reports it.
/// tests/parser_version.rs holds it to the version in Cargo.lock.
pub const PARSER: &str = "parsec-web-core 0.3.0";

pub use cache::{QUERY_CACHE_SIZE, QUERY_CACHE_TEXT};
pub use document::{Content, Document, EditError, Node, NodeId, NodeKind, ParseTimeout, Reading};
pub use query::{Hit, OutOfMemory, Output, Query, QueryError};
pub use xml::{ParseXmlError, XmlError, XmlNodes};
pub use xpath::{XPath, XPathError, XValue};
