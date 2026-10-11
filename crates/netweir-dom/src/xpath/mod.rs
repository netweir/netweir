//! XPath 1.0 over a parsed document, written from the W3C recommendation,
//! with the extensions Scrapy users rely on: `has-class()` and the EXSLT
//! regular-expression functions `re:test()` and `re:replace()`.

mod eval;
mod lexer;
mod parser;

use std::fmt;

use crate::document::Node;
use crate::query::Hit;

pub use eval::XValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XPathError {
    pub query: String,
    pub reason: String,
}

impl fmt::Display for XPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid XPath {:?}: {}", self.query, self.reason)
    }
}

impl std::error::Error for XPathError {}

/// A compiled XPath expression. Unlike [`crate::Query`] it holds no lexbor
/// state, so one can be shared by many threads.
#[derive(Debug, Clone)]
pub struct XPath {
    src: String,
    expr: parser::Expr,
}

impl XPath {
    pub fn new(src: &str) -> Result<XPath, XPathError> {
        let tokens = lexer::tokenize(src)?;
        let mut expr = parser::parse(src, tokens)?;
        parser::memoize(&mut expr);
        Ok(XPath {
            src: src.to_string(),
            expr,
        })
    }

    /// Evaluates with `scope` as the context node. Node-set results come
    /// back in document order: elements, text and comment nodes as
    /// [`Hit::Node`], attributes as their value. A number, string or
    /// boolean result is one [`Hit::Value`], formatted as Scrapy's parsel
    /// formats it (`3.0`, `1`/`0` for true/false).
    pub fn run<'a>(&self, scope: Node<'a>) -> Result<Vec<Hit<'a>>, XPathError> {
        self.run_with(scope, &[])
    }

    /// `run`, with values for `$variables` in the expression.
    pub fn run_with<'a>(
        &self,
        scope: Node<'a>,
        vars: &[(String, XValue)],
    ) -> Result<Vec<Hit<'a>>, XPathError> {
        let mut used = Vec::new();
        self.expr.variables(&mut used);
        if let Some(missing) = used.iter().find(|v| !vars.iter().any(|(k, _)| k == *v)) {
            return Err(XPathError {
                query: self.src.clone(),
                reason: format!("no value given for ${missing}"),
            });
        }
        eval::run(&self.expr, scope, vars).map_err(|reason| XPathError {
            query: self.src.clone(),
            reason,
        })
    }
}
