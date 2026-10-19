//! XML documents. quick-xml reads the markup and this module builds the
//! tree in lexbor, as an HTML page's is built, so CSS, XPath, `get()` and
//! the rest of the API work on a feed or a sitemap as they do on a page.
//!
//! XML is a trust boundary, and parsing treats it as one:
//!
//! - Nothing is ever fetched. A DOCTYPE's external DTD is ignored, and a
//!   reference to an external entity is an error, never a request or a
//!   file read.
//! - Entities the document declares in its own DTD are expanded, but no
//!   more than [`MAX_ENTITY_DEPTH`] deep, and to no more than a few times
//!   the document's own size in all: a "billion laughs" document is
//!   refused before it can fill memory.
//! - Elements nest at most [`MAX_DEPTH`] deep.
//! - Anything that isn't well-formed is an [`XmlError`] saying what is
//!   wrong and where, by line and column. Unlike lxml in Scrapy's
//!   `recover` mode, a broken document is never quietly cut short.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::time::{Duration, Instant};

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};

use crate::document::{Document, Node, NodeKind, ParseTimeout};
use crate::ffi::{self, RawNode};

/// How deep elements may nest. lxml's limit with `huge_tree`, which
/// Scrapy's selectors turn on.
pub const MAX_DEPTH: usize = 2048;
/// How deep entity references may nest inside the entities they refer to.
pub const MAX_ENTITY_DEPTH: usize = 16;
/// Entity expansion may add this many bytes to any document, plus
/// [`ENTITY_FACTOR`] bytes for each byte of the document itself.
pub const ENTITY_ALLOWANCE: usize = 1 << 20;
pub const ENTITY_FACTOR: usize = 8;

/// The namespace the `xml` prefix is always bound to.
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Why a document isn't well-formed XML, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlError {
    /// From 1.
    pub line: usize,
    /// From 1, in characters.
    pub column: usize,
    pub reason: String,
}

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "not well-formed XML at line {}, column {}: {}",
            self.line, self.column, self.reason
        )
    }
}

impl std::error::Error for XmlError {}

/// Returned by [`Document::parse_xml_within`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseXmlError {
    Malformed(XmlError),
    Timeout(ParseTimeout),
}

impl fmt::Display for ParseXmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseXmlError::Malformed(e) => e.fmt(f),
            ParseXmlError::Timeout(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ParseXmlError {}

/// What an XML document knows that lexbor doesn't keep: namespaces.
/// lexbor folds namespace URIs to lowercase, and XML compares them as
/// written, so they live here, keyed by node.
#[derive(Default)]
pub(crate) struct XmlNames {
    /// Namespace URIs; id `i` is `uris[i - 1]`, and 0 is no namespace.
    uris: Vec<Box<str>>,
    ids: HashMap<Box<str>, u32>,
    /// The namespace of each element or attribute that has one.
    ns: HashMap<usize, u32>,
    /// The `xmlns` declarations each element makes, in order: a prefix
    /// ("" for the default namespace) and a URI id (0 for `xmlns=""`).
    decls: HashMap<usize, Vec<(Box<str>, u32)>>,
    /// For an element read alone ([`XmlNodes`]): the declarations of the
    /// elements it was inside, nearest first.
    outer: Vec<(Box<str>, u32)>,
}

impl XmlNames {
    fn intern(&mut self, uri: &str) -> u32 {
        if uri.is_empty() {
            return 0;
        }
        if let Some(&id) = self.ids.get(uri) {
            return id;
        }
        self.uris.push(uri.into());
        let id = self.uris.len() as u32;
        self.ids.insert(uri.into(), id);
        id
    }

    /// The URI with id `id`; "" for 0.
    pub(crate) fn uri(&self, id: u32) -> &str {
        match id {
            0 => "",
            n => self.uris.get(n as usize - 1).map_or("", |u| u),
        }
    }

    /// The id of `uri` in this document, None if nothing in it has that
    /// namespace. "" is 0.
    pub(crate) fn id_of(&self, uri: &str) -> Option<u32> {
        if uri.is_empty() {
            Some(0)
        } else {
            self.ids.get(uri).copied()
        }
    }

    /// The namespace id of the element or attribute at `raw`.
    pub(crate) fn ns_of(&self, raw: usize) -> u32 {
        self.ns.get(&raw).copied().unwrap_or(0)
    }

    fn decls_of(&self, raw: usize) -> &[(Box<str>, u32)] {
        self.decls.get(&raw).map_or(&[], |d| d)
    }

    /// Forgets every node's namespace and declarations. The URIs stay:
    /// strings borrowed from them before must still be valid after.
    pub(crate) fn clear(&mut self) {
        self.ns.clear();
        self.decls.clear();
        self.outer.clear();
    }
}

// --- names (XML 1.0, fifth edition, section 2.3) --------------------------

fn name_start(c: char) -> bool {
    matches!(c,
        'A'..='Z' | '_' | 'a'..='z' | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}'
        | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}')
}

fn name_char(c: char) -> bool {
    name_start(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// A name without a colon, as namespaces allow in each part of a name.
pub fn is_ncname(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(name_start) && chars.all(name_char)
}

/// `local` or `prefix:local`.
pub(crate) fn is_qname(s: &str) -> bool {
    match s.split_once(':') {
        Some((p, l)) => is_ncname(p) && is_ncname(l),
        None => is_ncname(s),
    }
}

/// A Name as the DTD uses them (entity names, PI targets): colons allowed.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c == ':' || name_start(c))
        && chars.all(|c| c == ':' || name_char(c))
}

/// Characters XML allows in a document.
fn legal(c: char, v11: bool) -> bool {
    match c {
        '\t' | '\n' | '\r' => true,
        '\u{1}'..='\u{1F}' => v11,
        '\u{0}' => false,
        '\u{FFFE}' | '\u{FFFF}' => false,
        _ => true,
    }
}

/// Checks text as written in the document. XML 1.1 allows the control
/// characters only as character references, so both versions check alike.
fn check_chars(s: &str) -> Result<(), String> {
    // Most text is plain: only look closer at what could be illegal.
    if s.bytes().all(|b| b >= 0x20 && b != 0xEF) {
        return Ok(());
    }
    match s.chars().find(|&c| !legal(c, false)) {
        Some(c) => Err(format!(
            "the character U+{:04X} isn't allowed in XML",
            c as u32
        )),
        None => Ok(()),
    }
}

fn prefix_part(qname: &str) -> &str {
    qname.split_once(':').map_or("", |(prefix, _)| prefix)
}

fn is_blank(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
}

fn predefined(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return None,
    })
}

/// `&#...;` (given without `&` and `;`) as a character.
fn char_ref(body: &str, v11: bool) -> Result<char, String> {
    let digits = body.strip_prefix('#').unwrap_or(body);
    let n = match digits.strip_prefix('x') {
        Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
            u32::from_str_radix(hex, 16).ok()
        }
        None if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
            digits.parse().ok()
        }
        _ => return Err(format!("&{body}; isn't a valid character reference")),
    };
    match n.and_then(char::from_u32) {
        Some(c) if legal(c, v11) => Ok(c),
        _ => Err(format!("&{body}; refers to a character XML doesn't allow")),
    }
}

// --- the DTD's internal subset ---------------------------------------------

#[derive(Clone)]
enum Entity {
    /// Declared with its replacement text.
    Internal(Rc<str>),
    /// Declared with SYSTEM or PUBLIC: it would have to be fetched.
    External,
}

/// The general entities declared in a DOCTYPE's internal subset. `doctype`
/// is what follows `<!DOCTYPE`. Parameter entities are not expanded; as
/// XML asks of a processor that doesn't read them, declarations after a
/// parameter entity reference are not read either.
fn declared_entities(doctype: &str, v11: bool) -> Result<HashMap<String, Entity>, String> {
    let mut out = HashMap::new();
    let bytes = doctype.as_bytes();
    // Find the internal subset's `[`, skipping quoted literals.
    let mut i = 0;
    let mut quote = None;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (None, b'"' | b'\'') => quote = Some(bytes[i]),
            (Some(q), b) if b == q => quote = None,
            (None, b'[') => break,
            _ => {}
        }
        i += 1;
    }
    let mut s = &doctype[(i + 1).min(doctype.len())..];
    loop {
        s = s.trim_start_matches([' ', '\t', '\r', '\n']);
        if s.is_empty() || s.starts_with(']') {
            return Ok(out);
        }
        if let Some(rest) = s.strip_prefix("<!--") {
            let end = rest.find("-->").ok_or("a comment in the DTD never ends")?;
            s = &rest[end + 3..];
        } else if let Some(rest) = s.strip_prefix("<?") {
            let end = rest
                .find("?>")
                .ok_or("a processing instruction in the DTD never ends")?;
            s = &rest[end + 2..];
        } else if s.starts_with('%') {
            // A parameter entity reference: what follows can't be trusted
            // to mean what it says without reading it.
            return Ok(out);
        } else if let Some(rest) = s.strip_prefix("<!ENTITY") {
            let (decl, after) = split_decl(rest)?;
            s = after;
            if let Some((name, entity)) = entity_decl(decl, v11)? {
                // The first declaration of a name is the one that counts.
                out.entry(name).or_insert(entity);
            }
        } else if s.starts_with("<!") {
            let (_, after) = split_decl(&s[2..])?;
            s = after;
        } else {
            let shown: String = s.chars().take(20).collect();
            return Err(format!(
                "the DTD has {shown:?} where a declaration was expected"
            ));
        }
    }
}

/// A markup declaration's body up to its closing `>`, and what follows.
fn split_decl(s: &str) -> Result<(&str, &str), String> {
    let mut quote = None;
    for (i, b) in s.bytes().enumerate() {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(q), b) if b == q => quote = None,
            (None, b'>') => return Ok((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    Err("a declaration in the DTD never ends".into())
}

/// `name "value"`, `name SYSTEM "uri"`, `name PUBLIC "id" "uri"`, or a
/// parameter entity (`% name ...`), which gives None.
fn entity_decl(decl: &str, v11: bool) -> Result<Option<(String, Entity)>, String> {
    let ws = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let decl = decl.trim_matches(ws);
    if decl.starts_with('%') {
        return Ok(None);
    }
    let end = decl.find(ws).ok_or("an ENTITY declaration has no value")?;
    let (name, rest) = decl.split_at(end);
    if !is_name(name) || name.contains(':') {
        return Err(format!("{name:?} isn't a valid entity name"));
    }
    let rest = rest.trim_start_matches(ws);
    let entity = match rest.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let body = &rest[1..];
            let close = body
                .find(q)
                .ok_or("an entity's value has no closing quote")?;
            Entity::Internal(entity_value(&body[..close], v11)?.into())
        }
        _ if rest.starts_with("SYSTEM") || rest.starts_with("PUBLIC") => Entity::External,
        _ => return Err(format!("the ENTITY declaration of {name} has no value")),
    };
    Ok(Some((name.to_string(), entity)))
}

/// An entity's literal value with its character references replaced, as
/// XML does when the entity is declared. General entity references stay,
/// to be expanded where the entity is used.
fn entity_value(raw: &str, v11: bool) -> Result<String, String> {
    if raw.contains('%') {
        return Err("parameter entity references can't be used inside an entity's value".into());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find("&#") {
        out.push_str(&rest[..at]);
        let end = rest[at..]
            .find(';')
            .ok_or("a character reference has no `;`")?;
        out.push(char_ref(&rest[at + 1..at + end], v11)?);
        rest = &rest[at + end + 1..];
    }
    out.push_str(rest);
    check_chars(&out)?;
    Ok(out)
}

// --- building ---------------------------------------------------------------

enum Stop {
    Bad(String),
    Late,
}

impl From<String> for Stop {
    fn from(s: String) -> Stop {
        Stop::Bad(s)
    }
}

impl From<&str> for Stop {
    fn from(s: &str) -> Stop {
        Stop::Bad(s.to_string())
    }
}

type Step<T = ()> = Result<T, Stop>;

/// An element that has started and not ended.
struct Open {
    qname: String,
    /// How many namespace bindings were in scope before it.
    mark: usize,
    /// Its node, when it's part of what's being built.
    node: *mut RawNode,
}

/// Which elements become documents: all of it, or each element with this
/// namespace and local name, alone.
enum Want {
    Whole,
    Nodes { ns: String, local: String },
}

/// The document being built, and how deep in the source it starts.
struct Out {
    doc: Document,
    depth: usize,
}

struct Builder {
    want: Want,
    out: Option<Out>,
    /// A finished document, in Nodes mode.
    ready: Option<Document>,
    /// Namespace bindings in scope: prefix ("" for the default) and URI.
    bindings: Vec<(String, String)>,
    open: Vec<Open>,
    entities: HashMap<String, Entity>,
    /// Entities being expanded, outermost first.
    expanding: Vec<String>,
    /// How many elements were open when the innermost expansion began:
    /// an entity's content can't close more than it opened.
    floor: usize,
    expanded: usize,
    allowance: usize,
    /// Text not yet put in a node, so text, CDATA and references next to
    /// each other become one text node, as in lxml.
    pending: String,
    root_seen: bool,
    root_done: bool,
    doctype_seen: bool,
    /// Whether an XML declaration may come next: only at the very start.
    decl_allowed: bool,
    v11: bool,
    events: u64,
    deadline: Option<Instant>,
}

impl Builder {
    fn new(want: Want, size: usize, deadline: Option<Instant>) -> Builder {
        let out = matches!(want, Want::Whole).then(|| Out {
            doc: Document::new_xml(),
            depth: 0,
        });
        Builder {
            want,
            out,
            ready: None,
            bindings: Vec::new(),
            open: Vec::new(),
            entities: HashMap::new(),
            expanding: Vec::new(),
            floor: 0,
            expanded: 0,
            allowance: ENTITY_ALLOWANCE.saturating_add(size.saturating_mul(ENTITY_FACTOR)),
            pending: String::new(),
            root_seen: false,
            root_done: false,
            doctype_seen: false,
            decl_allowed: true,
            v11: false,
            events: 0,
            deadline,
        }
    }

    fn version(&self) -> XmlVersion {
        if self.v11 {
            XmlVersion::Explicit1_1
        } else {
            XmlVersion::Explicit1_0
        }
    }

    /// The node new content goes into, if what's being read is being built.
    fn parent(&self) -> Option<*mut RawNode> {
        let out = self.out.as_ref()?;
        if self.open.len() > out.depth {
            self.open.last().map(|o| o.node)
        } else if matches!(self.want, Want::Whole) {
            Some(unsafe { ffi::nw_root(out.doc.raw) })
        } else {
            None
        }
    }

    fn append(&mut self, node: *mut RawNode) {
        assert!(!node.is_null(), "lexbor failed to allocate an XML node");
        if let Some(parent) = self.parent() {
            unsafe { ffi::nw_append(parent, node) };
        }
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if let (Some(_), Some(out)) = (self.parent(), self.out.as_ref()) {
            let t = &self.pending;
            let n = unsafe { ffi::nw_create_text(out.doc.raw, t.as_ptr().cast(), t.len()) };
            self.append(n);
        }
        self.pending.clear();
    }

    fn event(&mut self, ev: Event<'_>) -> Step {
        self.events += 1;
        if self.events.is_multiple_of(1024) && self.deadline.is_some_and(|d| Instant::now() > d) {
            return Err(Stop::Late);
        }
        let decl_allowed = std::mem::replace(&mut self.decl_allowed, false);
        match ev {
            Event::Decl(d) => {
                if !decl_allowed {
                    return Err("an XML declaration can only come at the very start".into());
                }
                match d.version().map_err(|e| e.to_string())?.as_ref() {
                    "1.0" => {}
                    "1.1" => self.v11 = true,
                    v => return Err(format!("XML version {v:?} isn't 1.0 or 1.1").into()),
                }
            }
            Event::DocType(t) => {
                if self.root_seen {
                    return Err("a DOCTYPE can only come before the root element".into());
                }
                if self.doctype_seen {
                    return Err("a document can only have one DOCTYPE".into());
                }
                self.doctype_seen = true;
                self.entities = declared_entities(&t.into_inner(), self.v11)?;
            }
            Event::PI(p) => {
                let target = p.target();
                if target.eq_ignore_ascii_case("xml") {
                    return Err("an XML declaration can only come at the very start".into());
                }
                if !is_name(target) {
                    return Err(
                        format!("{target:?} isn't a valid processing instruction target").into(),
                    );
                }
                let data = p.content().trim_start_matches([' ', '\t', '\r', '\n']);
                check_chars(data)?;
                self.flush();
                if let Some(out) = self.parent().and(self.out.as_ref()) {
                    let n = unsafe {
                        ffi::nw_create_pi(
                            out.doc.raw,
                            target.as_ptr().cast(),
                            target.len(),
                            data.as_ptr().cast(),
                            data.len(),
                        )
                    };
                    self.append(n);
                }
            }
            Event::Comment(c) => {
                let text = c.xml_content(self.version());
                check_chars(&text)?;
                self.flush();
                if let Some(out) = self.parent().and(self.out.as_ref()) {
                    let n = unsafe {
                        ffi::nw_create_comment(out.doc.raw, text.as_ptr().cast(), text.len())
                    };
                    self.append(n);
                }
            }
            Event::Start(e) => self.start(&e, false)?,
            Event::Empty(e) => self.start(&e, true)?,
            Event::End(e) => self.end(e.name().as_ref())?,
            Event::Text(t) => {
                let text = t.xml_content(self.version());
                if text.contains("]]>") {
                    return Err("`]]>` can't appear in text (write ]]&gt;)".into());
                }
                self.text(&text)?;
            }
            Event::CData(c) => {
                if self.open.is_empty() {
                    return Err("CDATA can only come inside the root element".into());
                }
                let text = c.xml_content(self.version());
                self.text(&text)?;
            }
            Event::GeneralRef(r) => {
                if self.open.is_empty() {
                    return Err("an entity reference can only come inside the root element".into());
                }
                let body = r.into_inner();
                if body.starts_with('#') {
                    let c = char_ref(&body, self.v11)?;
                    if self.capturing() {
                        self.pending.push(c);
                    }
                } else if let Some(c) = predefined(&body) {
                    if self.capturing() {
                        self.pending.push(c);
                    }
                } else {
                    self.expand(&body)?;
                }
            }
            Event::Eof => {}
        }
        Ok(())
    }

    /// Whether text read now ends up in the tree.
    fn capturing(&self) -> bool {
        self.out
            .as_ref()
            .is_some_and(|out| self.open.len() > out.depth)
    }

    fn text(&mut self, text: &str) -> Step {
        if self.open.is_empty() {
            if !is_blank(text) {
                let shown: String = text.trim().chars().take(30).collect();
                return Err(format!("text outside the root element: {shown:?}").into());
            }
            return Ok(());
        }
        check_chars(text)?;
        if self.capturing() {
            self.pending.push_str(text);
        }
        Ok(())
    }

    /// Counts an expansion of `name`, refusing one too deep, one that
    /// refers to itself, and one past the document's allowance.
    fn enter(&mut self, name: &str, size: usize) -> Step {
        if self.expanding.iter().any(|n| n == name) {
            return Err(format!("the entity &{name}; refers to itself").into());
        }
        if self.expanding.len() >= MAX_ENTITY_DEPTH {
            return Err(
                format!("entities nest more than {MAX_ENTITY_DEPTH} deep (at &{name};)").into(),
            );
        }
        self.expanded = self.expanded.saturating_add(size);
        if self.expanded > self.allowance {
            return Err(format!(
                "expanding entities would take more than {} bytes, which this document's size allows (a \"billion laughs\" document?)",
                self.allowance
            )
            .into());
        }
        self.expanding.push(name.to_string());
        Ok(())
    }

    fn entity(&self, name: &str) -> Result<Rc<str>, String> {
        match self.entities.get(name) {
            Some(Entity::Internal(v)) => Ok(v.clone()),
            Some(Entity::External) => Err(format!(
                "&{name}; is an external entity, and external entities are never fetched"
            )),
            None => Err(format!("the entity &{name}; isn't declared")),
        }
    }

    /// Reads a declared entity's replacement text where it's referred to.
    fn expand(&mut self, name: &str) -> Step {
        let value = self.entity(name)?;
        self.enter(name, value.len())?;
        let floor = std::mem::replace(&mut self.floor, self.open.len());
        let mut result = self.read_entity(&value);
        if result.is_ok() && self.open.len() != self.floor {
            let left = &self.open[self.open.len() - 1].qname;
            result = Err(format!("the entity &{name}; leaves <{left}> open").into());
        }
        self.floor = floor;
        self.expanding.pop();
        result.map_err(|e| match e {
            Stop::Bad(why) if !why.starts_with("in &") => Stop::Bad(format!("in &{name};: {why}")),
            other => other,
        })
    }

    fn read_entity(&mut self, value: &str) -> Step {
        let mut reader = reader(value);
        loop {
            match reader.read_event().map_err(|e| e.to_string())? {
                Event::Eof => return Ok(()),
                Event::Decl(_) | Event::DocType(_) => {
                    return Err("an entity can't contain a declaration".into());
                }
                ev => self.event(ev)?,
            }
        }
    }

    /// An attribute's value as XML normalises it: references replaced,
    /// and each line break or tab a space.
    fn attr_value(&mut self, raw: &str, out: &mut String) -> Step {
        let mut rest = raw;
        while let Some(at) = rest.find(['&', '<', '\r', '\n', '\t']) {
            let head = &rest[..at];
            check_chars(head)?;
            out.push_str(head);
            let c = rest.as_bytes()[at];
            rest = &rest[at + 1..];
            match c {
                b'<' => return Err("`<` isn't allowed in an attribute value".into()),
                b'\r' => {
                    rest = rest.strip_prefix('\n').unwrap_or(rest);
                    out.push(' ');
                }
                b'\n' | b'\t' => out.push(' '),
                _ => {
                    let end = rest
                        .find(';')
                        .ok_or("`&` in an attribute value starts no reference (write &amp;)")?;
                    let body = &rest[..end];
                    rest = &rest[end + 1..];
                    if body.starts_with('#') {
                        out.push(char_ref(body, self.v11)?);
                    } else if let Some(c) = predefined(body) {
                        out.push(c);
                    } else {
                        let value = self.entity(body)?;
                        if value.contains('<') {
                            return Err(format!(
                                "the entity &{body}; contains `<`, so it can't be used in an attribute value"
                            )
                            .into());
                        }
                        self.enter(body, value.len())?;
                        let result = self.attr_value(&value, out);
                        self.expanding.pop();
                        result?;
                    }
                }
            }
        }
        check_chars(rest)?;
        out.push_str(rest);
        Ok(())
    }

    /// The URI `prefix` is bound to; None for no namespace.
    fn resolve(&self, prefix: &str) -> Option<&str> {
        if prefix == "xml" {
            return Some(XML_NS);
        }
        self.bindings
            .iter()
            .rev()
            .find(|(p, _)| p == prefix)
            .map(|(_, u)| u.as_str())
            .filter(|u| !u.is_empty())
    }

    fn bind(&mut self, prefix: &str, uri: String) -> Step {
        if prefix == "xmlns" {
            return Err("the xmlns prefix can't be declared".into());
        }
        if (prefix == "xml") != (uri == XML_NS) {
            return Err(format!(
                "the xml prefix is only ever bound to {XML_NS}, and nothing else can be"
            )
            .into());
        }
        if uri == XMLNS_NS {
            return Err(format!("{XMLNS_NS} can't be declared as a namespace").into());
        }
        if !prefix.is_empty() && uri.is_empty() && !self.v11 {
            return Err(format!("xmlns:{prefix} can't be empty in XML 1.0").into());
        }
        if !prefix.is_empty() && !is_ncname(prefix) {
            return Err(format!("{prefix:?} isn't a valid namespace prefix").into());
        }
        self.bindings.push((prefix.to_string(), uri));
        Ok(())
    }

    fn start(&mut self, e: &BytesStart<'_>, empty: bool) -> Step {
        self.flush();
        let qname = e.name().as_ref().to_string();
        if !is_qname(&qname) {
            return Err(format!("<{qname}> isn't a valid element name").into());
        }
        if self.open.is_empty() {
            if self.root_done {
                return Err(format!(
                    "<{qname}> comes after the root element has ended; a document has one root"
                )
                .into());
            }
            self.root_seen = true;
        }
        if self.open.len() >= MAX_DEPTH {
            return Err(format!("elements nest more than {MAX_DEPTH} deep").into());
        }
        let mark = self.bindings.len();
        let mut attrs: Vec<(String, String)> = Vec::new();
        for a in e.attributes() {
            let a = a.map_err(|e| format!("in <{qname}>: {e}"))?;
            let key = a.key.as_ref();
            let mut value = String::new();
            self.attr_value(&a.value, &mut value)?;
            if key == "xmlns" {
                self.bind("", value)?;
            } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                if !is_ncname(prefix) {
                    return Err(format!("{prefix:?} isn't a valid namespace prefix").into());
                }
                self.bind(prefix, value)?;
            } else if !is_qname(key) {
                return Err(format!("{key:?} isn't a valid attribute name").into());
            } else {
                attrs.push((key.to_string(), value));
            }
        }
        let (prefix, local) = qname.split_once(':').unwrap_or(("", &qname));
        let ns = match self.resolve(prefix) {
            Some(u) => u.to_string(),
            None if prefix.is_empty() => String::new(),
            None => {
                return Err(
                    format!("the namespace prefix {prefix} of <{qname}> isn't declared").into(),
                );
            }
        };
        let mut attr_ns: Vec<String> = Vec::with_capacity(attrs.len());
        for (key, _) in &attrs {
            let uri = match key.split_once(':') {
                Some((p, _)) => self
                    .resolve(p)
                    .ok_or_else(|| {
                        format!("the namespace prefix {p} of the attribute {key} isn't declared")
                    })?
                    .to_string(),
                None => String::new(),
            };
            attr_ns.push(uri);
        }
        // Two prefixes for one namespace still name one attribute.
        for i in 0..attrs.len() {
            for j in 0..i {
                let local = |k: &str| k.split_once(':').map_or(k, |(_, l)| l).to_string();
                if !attr_ns[i].is_empty()
                    && attr_ns[i] == attr_ns[j]
                    && local(&attrs[i].0) == local(&attrs[j].0)
                {
                    return Err(format!(
                        "<{qname}> has {} and {}, the same attribute twice",
                        attrs[j].0, attrs[i].0
                    )
                    .into());
                }
            }
        }
        let wanted = match &self.want {
            Want::Whole => false,
            Want::Nodes {
                ns: want_ns,
                local: want_local,
            } => *want_ns == ns && want_local == local,
        };
        if self.out.is_none() && wanted {
            self.out = Some(Out {
                doc: Document::new_xml(),
                depth: self.open.len(),
            });
        }
        let mut node = std::ptr::null_mut();
        let top = self
            .out
            .as_ref()
            .is_some_and(|o| o.depth == self.open.len());
        if let Some(out) = self.out.as_mut().filter(|o| self.open.len() >= o.depth) {
            let raw = out.doc.raw;
            node = unsafe {
                ffi::nw_xml_element(
                    raw,
                    qname.as_ptr().cast(),
                    qname.len(),
                    qname.len() - local.len(),
                )
            };
            assert!(!node.is_null(), "lexbor failed to allocate an XML element");
            let names = out.doc.xml_names_mut();
            if !ns.is_empty() {
                let id = names.intern(&ns);
                names.ns.insert(node as usize, id);
            }
            for ((key, value), uri) in attrs.iter().zip(&attr_ns) {
                let a = unsafe {
                    ffi::nw_xml_attr(
                        node,
                        key.as_ptr().cast(),
                        key.len(),
                        value.as_ptr().cast(),
                        value.len(),
                    )
                };
                assert!(!a.is_null(), "lexbor failed to allocate an XML attribute");
                if !uri.is_empty() {
                    let id = names.intern(uri);
                    names.ns.insert(a as usize, id);
                }
            }
            // A node read alone keeps what the elements it was cut from
            // declared, nearest first, each in the order written, for
            // writing it out as it would be written from the whole.
            if top && !matches!(self.want, Want::Whole) {
                let mut end = mark;
                for open in self.open.iter().rev() {
                    for (p, u) in &self.bindings[open.mark..end] {
                        let id = names.intern(u);
                        names.outer.push((Box::from(p.as_str()), id));
                    }
                    end = open.mark;
                }
            }
            let declared = &self.bindings[mark..];
            if !declared.is_empty() {
                let decls = declared
                    .iter()
                    .map(|(p, u)| (Box::from(p.as_str()), names.intern(u)))
                    .collect();
                names.decls.insert(node as usize, decls);
            }
            if top {
                // The node of its own document, or the root of the whole.
                let parent = unsafe { ffi::nw_root(raw) };
                unsafe { ffi::nw_append(parent, node) };
            } else if let Some(parent) = self.open.last().map(|o| o.node) {
                unsafe { ffi::nw_append(parent, node) };
            }
        }
        self.open.push(Open { qname, mark, node });
        if empty {
            let name = self
                .open
                .last()
                .map(|o| o.qname.clone())
                .unwrap_or_default();
            self.end(&name)?;
        }
        Ok(())
    }

    fn end(&mut self, name: &str) -> Step {
        self.flush();
        let Some(top) = self.open.last() else {
            return Err(format!("</{name}> closes an element that never started").into());
        };
        if self.open.len() <= self.floor {
            return Err(format!("</{name}> would close an element the entity didn't start").into());
        }
        if top.qname != name {
            return Err(format!("</{name}> doesn't close <{}>", top.qname).into());
        }
        let top = self.open.pop().expect("checked above");
        self.bindings.truncate(top.mark);
        if self.open.is_empty() {
            self.root_done = true;
        }
        if !matches!(self.want, Want::Whole)
            && self
                .out
                .as_ref()
                .is_some_and(|o| o.depth == self.open.len())
        {
            self.ready = self.out.take().map(|o| o.doc);
        }
        Ok(())
    }

    /// Reads `src` from byte `start` until its end, or in Nodes mode until
    /// a node is ready. Returns where it stopped, or why and where it
    /// failed.
    fn pump(&mut self, src: &str, start: usize) -> Result<usize, (Stop, usize)> {
        let mut reader = reader(&src[start..]);
        loop {
            let before = start + reader.buffer_position() as usize;
            let ev = match reader.read_event() {
                Ok(ev) => ev,
                Err(e) => {
                    let at = start + reader.error_position() as usize;
                    return Err((Stop::Bad(e.to_string()), at));
                }
            };
            if matches!(ev, Event::Eof) {
                return Ok(src.len());
            }
            if before != 0 {
                self.decl_allowed = false;
            }
            self.event(ev).map_err(|s| (s, before))?;
            if self.ready.is_some() {
                return Ok(start + reader.buffer_position() as usize);
            }
        }
    }

    /// The checks at the end of the document.
    fn finish(&mut self, src: &str) -> Result<(), String> {
        self.flush();
        if !self.open.is_empty() {
            let names: Vec<&str> = self.open.iter().map(|o| o.qname.as_str()).collect();
            return Err(format!("the document ends inside <{}>", names.join("><")));
        }
        if !self.root_seen && !is_blank(src) {
            return Err("the document has no root element".into());
        }
        Ok(())
    }
}

fn reader(src: &str) -> Reader<&[u8]> {
    let mut reader = Reader::from_str(src);
    let config = reader.config_mut();
    // Ends are checked here, against elements opened in other readers too
    // (an entity's, or the one before a node was handed out).
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    config.check_comments = true;
    config.allow_dangling_amp = false;
    config.expand_empty_elements = false;
    config.trim_text(false);
    reader
}

/// The line and column (both from 1, the column in characters) of byte
/// `at` of `src`.
fn position(src: &str, at: usize) -> (usize, usize) {
    let mut at = at.min(src.len());
    while !src.is_char_boundary(at) {
        at -= 1;
    }
    let head = &src[..at];
    let line = head.bytes().filter(|&b| b == b'\n').count() + 1;
    let column = head.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

fn error_at(src: &str, at: usize, reason: String) -> XmlError {
    let (line, column) = position(src, at);
    XmlError {
        line,
        column,
        reason,
    }
}

impl Document {
    /// Parses well-formed XML (see the module's documentation for what
    /// that means here, and what is refused).
    pub fn parse_xml(text: &str) -> Result<Document, XmlError> {
        Document::parse_xml_within(text, Duration::MAX).map_err(|e| match e {
            ParseXmlError::Malformed(e) => e,
            ParseXmlError::Timeout(_) => unreachable!("an unlimited budget cannot run out"),
        })
    }

    /// Like [`Document::parse_xml`], but gives up once `budget` has passed.
    pub fn parse_xml_within(text: &str, budget: Duration) -> Result<Document, ParseXmlError> {
        let src = text.strip_prefix('\u{feff}').unwrap_or(text);
        let deadline = Instant::now().checked_add(budget);
        let mut b = Builder::new(Want::Whole, src.len(), deadline);
        let malformed = |at: usize, why: String| ParseXmlError::Malformed(error_at(src, at, why));
        match b.pump(src, 0) {
            Ok(_) => {}
            Err((Stop::Bad(why), at)) => return Err(malformed(at, why)),
            Err((Stop::Late, at)) => {
                return Err(ParseXmlError::Timeout(ParseTimeout { budget, parsed: at }));
            }
        }
        b.finish(src).map_err(|why| malformed(src.len(), why))?;
        Ok(b.out.take().expect("a whole document is always built").doc)
    }
}

/// The elements of an XML document with one name, each read on its own
/// as a document of its own, as the document is read: the whole document
/// is never in memory, which suits a large feed. Scrapy's `iternodes`.
///
/// Elements inside a matching one are part of it, not matches of their
/// own. The document is still checked from start to end: a node is only
/// given out once it's well-formed, and an error ends the iteration.
pub struct XmlNodes {
    src: String,
    at: usize,
    builder: Builder,
    done: bool,
}

// SAFETY: the raw pointers inside are to nodes of the document being
// built, which the builder owns and nothing else can reach until it is
// handed out; moving the lot to another thread moves them together.
unsafe impl Send for XmlNodes {}

impl XmlNodes {
    /// The elements named `tag` (`local`, or `prefix:local` with the
    /// prefix bound in `namespaces`) in `text`. A name without a prefix
    /// means one in no namespace, as in XPath.
    pub fn new(
        text: String,
        tag: &str,
        namespaces: &[(String, String)],
    ) -> Result<XmlNodes, String> {
        let (ns, local) = match tag.split_once(':') {
            Some((prefix, local)) => {
                let uri = namespaces
                    .iter()
                    .rev()
                    .find(|(p, _)| p == prefix)
                    .map(|(_, u)| u.clone())
                    .or_else(|| (prefix == "xml").then(|| XML_NS.to_string()))
                    .ok_or_else(|| format!("the namespace prefix {prefix:?} of {tag:?} isn't in the namespaces given"))?;
                (uri, local)
            }
            None => (String::new(), tag),
        };
        if !is_ncname(local) {
            return Err(format!("{tag:?} isn't a valid element name"));
        }
        Ok(XmlNodes::reading(
            text,
            Want::Nodes {
                ns,
                local: local.to_string(),
            },
        ))
    }

    fn reading(text: String, want: Want) -> XmlNodes {
        let src = match text.strip_prefix('\u{feff}') {
            Some(rest) => rest.to_string(),
            None => text,
        };
        let size = src.len();
        XmlNodes {
            src,
            at: 0,
            builder: Builder::new(want, size, None),
            done: false,
        }
    }
}

impl Iterator for XmlNodes {
    type Item = Result<Document, XmlError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.builder.pump(&self.src, self.at) {
            Ok(at) => {
                self.at = at;
                if let Some(doc) = self.builder.ready.take() {
                    return Some(Ok(doc));
                }
                self.done = true;
                match self.builder.finish(&self.src) {
                    Ok(()) => None,
                    Err(why) => Some(Err(error_at(&self.src, self.src.len(), why))),
                }
            }
            Err((stop, at)) => {
                self.done = true;
                let why = match stop {
                    Stop::Bad(why) => why,
                    Stop::Late => "ran out of time".to_string(),
                };
                Some(Err(error_at(&self.src, at, why)))
            }
        }
    }
}

// --- writing XML -------------------------------------------------------------

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
}

fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
}

/// `node`'s start tag, without its `>`. The outermost element written
/// (`top`) also declares the namespaces it inherits, so the XML written
/// stands on its own, as lxml writes an element taken from a document:
/// its own declarations, then those of the namespaces its name and
/// attributes are in, then the rest in scope, nearest element first, each
/// prefix once.
fn start_tag(node: Node<'_>, names: &XmlNames, top: bool, out: &mut String) {
    out.push('<');
    out.push_str(node.qualified_name());
    let mut decls: Vec<(&str, u32)> = names
        .decls_of(node.raw as usize)
        .iter()
        .map(|(p, u)| (&**p, *u))
        .collect();
    if top {
        let ns = node.ns_id();
        if ns != 0 {
            decls.push((prefix_part(node.qualified_name()), ns));
        }
        for (name, ns, _) in node.xml_attrs() {
            let prefix = prefix_part(name);
            if ns != 0 && prefix != "xml" {
                decls.push((prefix, ns));
            }
        }
        let mut up = node.parent();
        while let Some(a) = up {
            decls.extend(
                names
                    .decls_of(a.raw as usize)
                    .iter()
                    .map(|(p, u)| (&**p, *u)),
            );
            up = a.parent();
        }
        decls.extend(names.outer.iter().map(|(p, u)| (&**p, *u)));
    }
    for (i, &(prefix, uri)) in decls.iter().enumerate() {
        if decls[..i].iter().any(|(p, _)| *p == prefix) {
            continue;
        }
        if prefix.is_empty() {
            out.push_str(" xmlns=\"");
        } else {
            out.push_str(" xmlns:");
            out.push_str(prefix);
            out.push_str("=\"");
        }
        escape_attr(names.uri(uri), out);
        out.push('"');
    }
    for (name, value) in node.attrs() {
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        escape_attr(value, out);
        out.push('"');
    }
}

fn leaf(node: Node<'_>, out: &mut String) {
    match node.kind() {
        NodeKind::Text => escape_text(node.data().unwrap_or(""), out),
        NodeKind::Comment => {
            out.push_str("<!--");
            out.push_str(node.data().unwrap_or(""));
            out.push_str("-->");
        }
        NodeKind::ProcessingInstruction => {
            out.push_str("<?");
            out.push_str(node.pi_target().unwrap_or(""));
            let data = node.data().unwrap_or("");
            if !data.is_empty() {
                out.push(' ');
                out.push_str(data);
            }
            out.push_str("?>");
        }
        _ => {}
    }
}

/// `node` as XML. A document is its top-level nodes, one to a line.
pub(crate) fn serialize(node: Node<'_>, names: &XmlNames) -> String {
    let mut out = String::new();
    if node.kind() == NodeKind::Document {
        for (i, child) in node.children().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            write(child, names, &mut out);
        }
        return out;
    }
    write(node, names, &mut out);
    out
}

/// Without recursion, so the deepest document allowed can't overflow the
/// stack.
fn write(node: Node<'_>, names: &XmlNames, out: &mut String) {
    enum Step<'a> {
        Enter(Node<'a>),
        Leave(Node<'a>),
    }
    let mut stack = vec![Step::Enter(node)];
    while let Some(step) = stack.pop() {
        match step {
            Step::Enter(n) if n.kind() == NodeKind::Element => {
                start_tag(n, names, n == node, out);
                if n.first_child().is_none() {
                    out.push_str("/>");
                    continue;
                }
                out.push('>');
                stack.push(Step::Leave(n));
                let children: Vec<Node<'_>> = n.children().collect();
                stack.extend(children.into_iter().rev().map(Step::Enter));
            }
            Step::Enter(n) => leaf(n, out),
            Step::Leave(n) => {
                out.push_str("</");
                out.push_str(n.qualified_name());
                out.push('>');
            }
        }
    }
}

/// `node` as indented XML: one tag, piece of text, comment or processing
/// instruction to a line, one space of indent a level. Text is trimmed and
/// whitespace-only text left out.
pub(crate) fn pretty(node: Node<'_>, names: &XmlNames) -> String {
    let mut out = String::new();
    let line = |out: &mut String, depth: usize, text: &str| {
        out.extend(std::iter::repeat_n(' ', depth));
        out.push_str(text);
        out.push('\n');
    };
    enum Step<'a> {
        Enter(Node<'a>, usize),
        Leave(Node<'a>, usize),
    }
    let mut stack = vec![];
    if node.kind() == NodeKind::Document {
        let children: Vec<Node<'_>> = node.children().collect();
        stack.extend(children.into_iter().rev().map(|c| Step::Enter(c, 0)));
    } else {
        stack.push(Step::Enter(node, 0));
    }
    let top = |n: Node<'_>| n == node || n.parent().is_some_and(|p| p.kind() == NodeKind::Document);
    while let Some(step) = stack.pop() {
        match step {
            Step::Enter(n, depth) => match n.kind() {
                NodeKind::Element => {
                    let mut tag = String::new();
                    start_tag(n, names, top(n), &mut tag);
                    if n.first_child().is_none() {
                        tag.push_str("/>");
                        line(&mut out, depth, &tag);
                        continue;
                    }
                    tag.push('>');
                    line(&mut out, depth, &tag);
                    stack.push(Step::Leave(n, depth));
                    let children: Vec<Node<'_>> = n.children().collect();
                    stack.extend(
                        children
                            .into_iter()
                            .rev()
                            .map(|c| Step::Enter(c, depth + 1)),
                    );
                }
                NodeKind::Text => {
                    let text = n.data().unwrap_or("").trim();
                    if !text.is_empty() {
                        let mut escaped = String::new();
                        escape_text(text, &mut escaped);
                        line(&mut out, depth, &escaped);
                    }
                }
                _ => {
                    let mut s = String::new();
                    leaf(n, &mut s);
                    line(&mut out, depth, &s);
                }
            },
            Step::Leave(n, depth) => line(&mut out, depth, &format!("</{}>", n.qualified_name())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_xml_grammar() {
        for good in ["a", "_x", "a-b.c1", "é", "日本"] {
            assert!(is_ncname(good), "{good}");
        }
        for bad in ["", "1a", "-a", "a b", "a:b", ".a"] {
            assert!(!is_ncname(bad), "{bad}");
        }
        assert!(is_qname("atom:entry"));
        assert!(!is_qname("a:b:c"));
        assert!(!is_qname(":a"));
        assert!(!is_qname("a:"));
    }

    #[test]
    fn character_references_are_checked() {
        assert_eq!(char_ref("#65", false), Ok('A'));
        assert_eq!(char_ref("#x263A", false), Ok('☺'));
        assert!(char_ref("#0", false).is_err());
        assert!(char_ref("#1", false).is_err());
        assert_eq!(char_ref("#1", true), Ok('\u{1}'));
        assert!(char_ref("#xD800", false).is_err());
        assert!(char_ref("#x", false).is_err());
        assert!(char_ref("#12a", false).is_err());
    }

    #[test]
    fn positions_count_lines_and_characters() {
        let src = "ab\ncdé\nfg";
        assert_eq!(position(src, 0), (1, 1));
        assert_eq!(position(src, 3), (2, 1));
        assert_eq!(position(src, 7), (2, 4));
        assert_eq!(position(src, src.len()), (3, 3));
    }

    #[test]
    fn the_internal_subset_declares_entities() {
        let e = declared_entities(
            r#"x SYSTEM "x.dtd" [
                <!-- a comment with an <!ENTITY inside -->
                <!ELEMENT x (#PCDATA)>
                <!ATTLIST x a CDATA "with > inside">
                <!ENTITY a "one &#38;#38; two">
                <!ENTITY a "ignored: the first counts">
                <!ENTITY b SYSTEM "file:///etc/passwd">
                <!ENTITY % p "parameter">
                <?pi with ]> inside?>
            ]"#,
            false,
        )
        .unwrap();
        assert!(matches!(e.get("a"), Some(Entity::Internal(v)) if &**v == "one &#38; two"));
        assert!(matches!(e.get("b"), Some(Entity::External)));
        assert!(!e.contains_key("p"));
    }

    #[test]
    fn declarations_after_a_parameter_entity_are_not_read() {
        let e = declared_entities(r#"x [ %ext; <!ENTITY a "x"> ]"#, false).unwrap();
        assert!(e.is_empty());
    }
}
