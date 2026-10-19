//! Beautiful Soup's filters for `find` and `find_all`: a string, a list of
//! strings, True/False, a compiled regular expression, or a function.

use netweir_dom::{Node, NodeKind};
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyFrozenSet, PyList, PySet, PyString, PyTuple};

pub(crate) enum Filter {
    /// No filter given: anything matches.
    Any,
    /// True: the value must exist. False: it must not.
    Present(bool),
    /// Equal to one of these.
    OneOf(Vec<String>),
    /// A compiled pattern (anything with `.search`): it must find a match.
    Pattern(Py<PyAny>),
    /// A list mixing strings and patterns: any one of them must match.
    AnyOf(Vec<Filter>),
    /// A function: it must return something true.
    Function(Py<PyAny>),
}

impl Filter {
    pub(crate) fn from_py(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Filter> {
        let Some(obj) = obj.filter(|o| !o.is_none()) else {
            return Ok(Filter::Any);
        };
        if let Ok(b) = obj.cast::<PyBool>() {
            return Ok(Filter::Present(b.is_true()));
        }
        if let Ok(s) = obj.cast::<PyString>() {
            return Ok(Filter::OneOf(vec![s.to_string()]));
        }
        if obj.is_instance_of::<PyList>()
            || obj.is_instance_of::<PyTuple>()
            || obj.is_instance_of::<PySet>()
            || obj.is_instance_of::<PyFrozenSet>()
        {
            let mut strings = Vec::new();
            let mut others = Vec::new();
            for item in obj.try_iter()? {
                let item = item?;
                if let Ok(s) = item.cast::<PyString>() {
                    strings.push(s.to_string());
                } else if item.hasattr("search")? {
                    others.push(Filter::Pattern(item.unbind()));
                } else {
                    return Err(PyTypeError::new_err(format!(
                        "a list filter holds strings and compiled regexes, not {}",
                        type_name(&item)
                    )));
                }
            }
            if others.is_empty() {
                return Ok(Filter::OneOf(strings));
            }
            others.push(Filter::OneOf(strings));
            return Ok(Filter::AnyOf(others));
        }
        if obj.hasattr("search")? {
            return Ok(Filter::Pattern(obj.clone().unbind()));
        }
        if obj.is_callable() {
            return Ok(Filter::Function(obj.clone().unbind()));
        }
        Err(PyTypeError::new_err(format!(
            "a filter is a string, a list of strings, True/False, a compiled regex or a function, not {}",
            type_name(obj)
        )))
    }

    pub(crate) fn is_any(&self) -> bool {
        matches!(self, Filter::Any)
    }

    /// Whether `value` (None when the attribute or string is missing)
    /// passes. Functions are called with the value.
    pub(crate) fn matches_value(&self, py: Python<'_>, value: Option<&str>) -> PyResult<bool> {
        Ok(match self {
            Filter::Any => true,
            Filter::Present(want) => value.is_some() == *want,
            Filter::OneOf(options) => value.is_some_and(|v| options.iter().any(|o| o == v)),
            Filter::Pattern(p) => match value {
                Some(v) => !p.bind(py).call_method1("search", (v,))?.is_none(),
                None => false,
            },
            Filter::Function(f) => f.bind(py).call1((value,))?.is_truthy()?,
            Filter::AnyOf(filters) => {
                for f in filters {
                    if f.matches_value(py, value)? {
                        return Ok(true);
                    }
                }
                false
            }
        })
    }

    /// A multi-valued attribute (class, rel, ...) matches if the filter
    /// matches any one of its values or the whole value, as in Beautiful
    /// Soup.
    fn matches_tokens(&self, py: Python<'_>, value: Option<&str>) -> PyResult<bool> {
        if let (
            Some(v),
            Filter::OneOf(_) | Filter::Pattern(_) | Filter::Function(_) | Filter::AnyOf(_),
        ) = (value, self)
        {
            for class in v.split_ascii_whitespace() {
                if self.matches_value(py, Some(class))? {
                    return Ok(true);
                }
            }
        }
        self.matches_value(py, value)
    }
}

fn type_name(obj: &Bound<'_, PyAny>) -> String {
    obj.get_type()
        .name()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "that".into())
}

/// Attributes Beautiful Soup splits into a list of values.
const MULTI_VALUED: [&str; 7] = [
    "class",
    "rel",
    "rev",
    "accept-charset",
    "headers",
    "accesskey",
    "dropzone",
];

/// Everything a `find*` call filters on.
pub(crate) struct Criteria {
    pub name: Filter,
    pub attrs: Vec<(String, Filter)>,
    pub string: Filter,
}

impl Criteria {
    /// With only `string=` given, Beautiful Soup matches text, not elements.
    pub(crate) fn wants_text(&self) -> bool {
        self.name.is_any() && self.attrs.is_empty() && !self.string.is_any()
    }

    /// Whether `node` passes. `as_python` turns a node into the object a
    /// name function is called with.
    pub(crate) fn matches(
        &self,
        py: Python<'_>,
        node: Node<'_>,
        as_python: &dyn Fn(Node<'_>) -> PyResult<Py<PyAny>>,
    ) -> PyResult<bool> {
        if self.wants_text() {
            return Ok(node.kind() == NodeKind::Text && self.string.matches_value(py, node.data())?);
        }
        if node.kind() != NodeKind::Element {
            return Ok(false);
        }
        let name_ok = match &self.name {
            // A name function is given the element, not its name.
            Filter::Function(f) => f.bind(py).call1((as_python(node)?,))?.is_truthy()?,
            Filter::Present(want) => *want,
            // In XML, as in Beautiful Soup, `dc:creator` finds it too.
            other => {
                other.matches_value(py, node.tag())?
                    || (node.qualified_name() != node.tag().unwrap_or_default()
                        && other.matches_value(py, Some(node.qualified_name()))?)
            }
        };
        if !name_ok {
            return Ok(false);
        }
        for (attr, filter) in &self.attrs {
            let value = node.attr(attr);
            let ok = if MULTI_VALUED.contains(&attr.as_str()) {
                filter.matches_tokens(py, value)?
            } else {
                filter.matches_value(py, value)?
            };
            if !ok {
                return Ok(false);
            }
        }
        if !self.string.is_any() {
            return self
                .string
                .matches_value(py, single_string(node).as_deref());
        }
        Ok(true)
    }
}

/// Beautiful Soup's `.string`: the text of a node whose only child (all the
/// way down) is one piece of text, else None.
pub(crate) fn single_string(node: Node<'_>) -> Option<String> {
    match node.kind() {
        NodeKind::Text | NodeKind::Comment => node.data().map(str::to_string),
        _ => {
            let mut children = node.children();
            let only = children.next()?;
            if children.next().is_some() {
                return None;
            }
            single_string(only)
        }
    }
}
