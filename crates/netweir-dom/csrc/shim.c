/*
 * Accessors over lexbor's DOM and selector engine.
 *
 * The Rust side calls only these functions, so it never depends on the
 * layout of lexbor's structs, which change between lexbor releases.
 */
#include <lexbor/css/css.h>
#include <lexbor/dom/dom.h>
#include <lexbor/html/html.h>
#include <lexbor/selectors/selectors.h>
#include <lexbor/core/sbst.h>
#include <lexbor/html/tokenizer/res.h>
#include <lexbor/tag/res.h>
#include <string.h>

/* ---- documents ---------------------------------------------------------- */

/*
 * Chunked parsing, so the caller can stop between chunks. Returns NULL (begin)
 * or non-zero (chunk, end) when lexbor cannot allocate.
 */
lxb_html_document_t *nw_chunk_begin(void) {
    lxb_html_document_t *doc = lxb_html_document_create();
    if (doc == NULL) {
        return NULL;
    }
    if (lxb_html_document_parse_chunk_begin(doc) != LXB_STATUS_OK) {
        lxb_html_document_destroy(doc);
        return NULL;
    }
    return doc;
}

int nw_chunk(lxb_html_document_t *doc, const char *html, size_t len) {
    return lxb_html_document_parse_chunk(doc, (const lxb_char_t *) html, len) == LXB_STATUS_OK ? 0 : -1;
}

/*
 * lexbor parses a <template>'s contents into a separate fragment, so they are
 * not the template's children and no walk, selector or index sees them. lxml
 * and Beautiful Soup show them as children, which is what scrapers expect, so
 * they are moved under the template once parsing ends. The serializer writes
 * the (now empty) fragment and then the children, so HTML output is unchanged.
 */
static void nw_adopt_templates(lxb_dom_node_t *root) {
    lxb_dom_node_t *n = root;
    while (n != NULL) {
        if (lxb_html_tree_node_is(n, LXB_TAG_TEMPLATE)) {
            lxb_html_template_element_t *t = lxb_html_interface_template(n);
            if (t->content != NULL) {
                lxb_dom_node_t *c;
                while ((c = t->content->node.first_child) != NULL) {
                    lxb_dom_node_remove(c);
                    lxb_dom_node_insert_child(n, c);
                }
            }
        }
        if (n->first_child != NULL) {
            n = n->first_child;
            continue;
        }
        while (n != root && n->next == NULL) {
            n = n->parent;
        }
        n = n == root ? NULL : n->next;
    }
}

/* `templates` is whether the markup has a <template> tag at all: the walk
 * visits every node, which on a large page is a tenth of the parse. */
int nw_chunk_end(lxb_html_document_t *doc, int templates) {
    if (lxb_html_document_parse_chunk_end(doc) != LXB_STATUS_OK) {
        return -1;
    }
    if (templates) {
        nw_adopt_templates(lxb_dom_interface_node(doc));
    }
    return 0;
}

void nw_destroy(lxb_html_document_t *doc) {
    lxb_html_document_destroy(doc);
}

lxb_dom_node_t *nw_root(lxb_html_document_t *doc) {
    return lxb_dom_interface_node(doc);
}

/* ---- nodes -------------------------------------------------------------- */

lxb_dom_node_t *nw_parent(lxb_dom_node_t *n) { return n->parent; }
lxb_dom_node_t *nw_first_child(lxb_dom_node_t *n) { return n->first_child; }
lxb_dom_node_t *nw_last_child(lxb_dom_node_t *n) { return n->last_child; }
lxb_dom_node_t *nw_next(lxb_dom_node_t *n) { return n->next; }
lxb_dom_node_t *nw_prev(lxb_dom_node_t *n) { return n->prev; }
int nw_type(lxb_dom_node_t *n) { return (int) n->type; }

/* Tag ids: equal ids mean equal (lowercased) names. 0 is "no such tag". */
uintptr_t nw_tag_id(lxb_dom_node_t *n) { return n->local_name; }
uintptr_t nw_tag_id_named(lxb_dom_node_t *n, const char *name, size_t len) {
    return lxb_tag_id_by_name(n->owner_document->tags, (const lxb_char_t *) name, len);
}

const char *nw_tag(lxb_dom_node_t *n, size_t *len) {
    return (const char *) lxb_dom_element_local_name(lxb_dom_interface_element(n), len);
}

/* Text and comment nodes. */
const char *nw_char_data(lxb_dom_node_t *n, size_t *len) {
    lxb_dom_character_data_t *cd = lxb_dom_interface_character_data(n);
    *len = cd->data.length;
    return (const char *) cd->data.data;
}

/* NULL when the attribute is absent; "" when present without a value. */
const char *nw_get_attr(lxb_dom_node_t *n, const char *name, size_t name_len, size_t *len) {
    lxb_dom_attr_t *attr = lxb_dom_element_attr_by_name(
        lxb_dom_interface_element(n), (const lxb_char_t *) name, name_len);
    if (attr == NULL) {
        return NULL;
    }
    const lxb_char_t *value = lxb_dom_attr_value(attr, len);
    if (value == NULL) {
        *len = 0;
        return "";
    }
    return (const char *) value;
}

lxb_dom_attr_t *nw_first_attr(lxb_dom_node_t *n) {
    return lxb_dom_element_first_attribute(lxb_dom_interface_element(n));
}

lxb_dom_attr_t *nw_next_attr(lxb_dom_attr_t *a) {
    return lxb_dom_element_next_attribute(a);
}

const char *nw_attr_name(lxb_dom_attr_t *a, size_t *len) {
    return (const char *) lxb_dom_attr_local_name(a, len);
}

const char *nw_attr_value(lxb_dom_attr_t *a, size_t *len) {
    const lxb_char_t *v = lxb_dom_attr_value(a, len);
    if (v == NULL) {
        *len = 0;
        return "";
    }
    return (const char *) v;
}

/* ---- whole-tree walks --------------------------------------------------- */

/* The node after `n` in document order, staying inside `root`. */
static lxb_dom_node_t *nw_step(lxb_dom_node_t *n, lxb_dom_node_t *root) {
    if (n->first_child != NULL) {
        return n->first_child;
    }
    while (n != root) {
        if (n->next != NULL) {
            return n->next;
        }
        n = n->parent;
    }
    return NULL;
}

/* Numbers every node under `root` (root first) in document order, from
 * `start`, keeping the number in the node's `user` field, which lexbor
 * leaves to callers. Returns the number after the last. */
size_t nw_number(lxb_dom_node_t *root, size_t start) {
    uintptr_t i = start;
    for (lxb_dom_node_t *n = root; n != NULL; n = nw_step(n, root)) {
        n->user = (void *) i++;
    }
    return (size_t) i;
}

uintptr_t nw_order(lxb_dom_node_t *n) { return (uintptr_t) n->user; }

/* After nw_number: fills one entry per node, in document order, with the
 * node, its parent's number (UINT32_MAX for the root), its type and its tag
 * id. Each array holds an entry for every node nw_number numbered. */
void nw_index_fill(lxb_dom_node_t *root, lxb_dom_node_t **raw, uint32_t *parent,
                   uint8_t *type, uintptr_t *tag) {
    size_t i = 0;
    for (lxb_dom_node_t *n = root; n != NULL; n = nw_step(n, root), i++) {
        raw[i] = n;
        parent[i] = (n == root || n->parent == NULL) ? UINT32_MAX : (uint32_t) (uintptr_t) n->parent->user;
        type[i] = (uint8_t) n->type;
        tag[i] = n->local_name;
    }
}

/* ---- editing ------------------------------------------------------------ */

/*
 * Tree edits only relink nodes and create new ones; nothing is ever freed
 * before the document is, so a pointer to a removed node stays valid. The
 * _wo_events variants skip lexbor's mutation callbacks, which netweir does
 * not register.
 */
void nw_remove(lxb_dom_node_t *n) { lxb_dom_node_remove_wo_events(n); }
void nw_append(lxb_dom_node_t *parent, lxb_dom_node_t *n) {
    lxb_dom_node_insert_child_wo_events(parent, n);
}
void nw_insert_before(lxb_dom_node_t *at, lxb_dom_node_t *n) {
    lxb_dom_node_insert_before_wo_events(at, n);
}

/* A new element with no parent, or NULL when lexbor cannot allocate. */
lxb_dom_node_t *nw_create_element(lxb_html_document_t *doc, const char *name, size_t len) {
    lxb_dom_element_t *el = lxb_dom_document_create_element(
        lxb_dom_interface_document(doc), (const lxb_char_t *) name, len, NULL);
    return el == NULL ? NULL : lxb_dom_interface_node(el);
}

/* Returns 0 on success. */
int nw_set_attr(lxb_dom_node_t *n, const char *name, size_t name_len, const char *value,
                size_t value_len) {
    lxb_dom_attr_t *attr = lxb_dom_element_set_attribute(
        lxb_dom_interface_element(n), (const lxb_char_t *) name, name_len,
        (const lxb_char_t *) value, value_len);
    return attr == NULL ? -1 : 0;
}

/* A new text node with no parent, or NULL when lexbor cannot allocate. */
lxb_dom_node_t *nw_create_text(lxb_html_document_t *doc, const char *data, size_t len) {
    lxb_dom_text_t *t = lxb_dom_document_create_text_node(
        lxb_dom_interface_document(doc), (const lxb_char_t *) data, len);
    return t == NULL ? NULL : lxb_dom_interface_node(t);
}

/* ---- XML ---------------------------------------------------------------- */

/*
 * An XML document is built node by node from the Rust side (xml.rs), which
 * does the parsing. It is an ordinary lexbor document marked as XML, so
 * lexbor compares attribute names as written rather than lowercased.
 *
 * Names keep their case in each node's qualified name. The local name, the
 * tag id the index and CSS match on, is lowercased as lexbor does it, so CSS
 * type selectors on XML ignore case. Namespaces are kept on the Rust side:
 * lexbor lowercases namespace URIs, which XML compares as written.
 */
lxb_html_document_t *nw_xml_document(void) {
    lxb_html_document_t *doc = lxb_html_document_create();
    if (doc != NULL) {
        doc->dom_document.type = LXB_DOM_DOCUMENT_DTYPE_XML;
    }
    return doc;
}

/* Defined in lexbor (tag.c, attr.c) without being in its headers. */
const lxb_tag_data_t *lxb_tag_append(lexbor_hash_t *hash, lxb_tag_id_t tag_id,
                                     const lxb_char_t *name, size_t length);
const lxb_tag_data_t *lxb_tag_append_lower(lexbor_hash_t *hash, const lxb_char_t *name,
                                           size_t length);
lxb_dom_attr_data_t *lxb_dom_attr_local_name_append(lexbor_hash_t *hash, const lxb_char_t *name,
                                                    size_t length);
lxb_dom_attr_data_t *lxb_dom_attr_qualified_name_append(lexbor_hash_t *hash,
                                                        const lxb_char_t *name, size_t length);

/* A new element with no parent, named `qname` (`prefix:local` or `local`,
 * the local part starting at `local_at`), or NULL when lexbor cannot
 * allocate. */
lxb_dom_node_t *nw_xml_element(lxb_html_document_t *doc, const char *qname, size_t qlen,
                               size_t local_at) {
    lxb_dom_document_t *d = lxb_dom_interface_document(doc);
    const lxb_tag_data_t *tag =
        lxb_tag_append_lower(d->tags, (const lxb_char_t *) qname + local_at, qlen - local_at);
    if (tag == NULL) {
        return NULL;
    }
    lxb_dom_element_t *el = lxb_dom_element_interface_create(d);
    if (el == NULL) {
        return NULL;
    }
    el->node.local_name = tag->tag_id;
    el->node.ns = LXB_NS__UNDEF;
    const lxb_tag_data_t *q = lxb_tag_append(d->tags, tag->tag_id, (const lxb_char_t *) qname, qlen);
    if (q == NULL) {
        return NULL;
    }
    el->qualified_name = (lxb_tag_id_t) q;
    return lxb_dom_interface_node(el);
}

/* An XML attribute's local name id: the lowercased name's, as lexbor's
 * HTML attributes have, which its CSS attribute selectors look up, so they
 * ignore case on XML too (`[id]` finds `ID`). Returns 1 for a name that
 * lowercases to `id` or `class` without being written so (`ID`, `Class`):
 * lexbor keeps one `id` and one `class` attribute per element, displacing
 * another, and XML may have `id` and `ID` side by side, so such a one must
 * be linked in without lexbor's bookkeeping (nw_xml_attr_link). 0 for any
 * other name, -1 when lexbor cannot allocate. */
static int nw_xml_attr_set_local(lxb_dom_document_t *d, lxb_dom_attr_t *attr, const char *qname,
                                 size_t qlen) {
    lxb_dom_attr_data_t *local =
        lxb_dom_attr_local_name_append(d->attrs, (const lxb_char_t *) qname, qlen);
    if (local == NULL) {
        return -1;
    }
    attr->node.local_name = local->attr_id;
    return (local->attr_id == LXB_DOM_ATTR_ID && !(qlen == 2 && memcmp(qname, "id", 2) == 0))
           || (local->attr_id == LXB_DOM_ATTR_CLASS
               && !(qlen == 5 && memcmp(qname, "class", 5) == 0));
}

/* 1 if `attr` is named `name` (lowercase) in another case: `ID` for "id". */
static int nw_xml_attr_variant(lxb_dom_attr_t *attr, const char *name, size_t len) {
    size_t qlen;
    const lxb_char_t *q = lxb_dom_attr_qualified_name(attr, &qlen);
    if (qlen != len || memcmp(q, name, len) == 0) {
        return 0;
    }
    for (size_t i = 0; i < len; i++) {
        lxb_char_t c = q[i] >= 'A' && q[i] <= 'Z' ? (lxb_char_t) (q[i] + 32) : q[i];
        if (c != (lxb_char_t) name[i]) {
            return 0;
        }
    }
    return 1;
}

/* The local name id of an `ID` or `Class`: lexbor's `id` or `class`, so
 * `[id]` finds it, unless the element also has an attribute written
 * exactly `id` (or `class`). Then it's its own written name's, as in
 * parsel, so `[id=x]` reads the `id` and not whichever came first. */
static void nw_xml_attr_settle(lxb_dom_element_t *el, lxb_dom_attr_t *attr) {
    if (nw_xml_attr_variant(attr, "id", 2)) {
        attr->node.local_name = el->attr_id != NULL ? attr->qualified_name : LXB_DOM_ATTR_ID;
    } else if (nw_xml_attr_variant(attr, "class", 5)) {
        attr->node.local_name =
            el->attr_class != NULL ? attr->qualified_name : LXB_DOM_ATTR_CLASS;
    }
}

/* nw_xml_attr_settle for each of `el`'s attributes, once its `id` or
 * `class` has come or gone. */
static void nw_xml_attrs_settle(lxb_dom_element_t *el) {
    for (lxb_dom_attr_t *a = el->first_attr; a != NULL; a = a->next) {
        nw_xml_attr_settle(el, a);
    }
}

/* Puts `attr` last among `el`'s attributes as lxb_dom_element_attr_append
 * does, but without taking it for the element's `id` or `class` (and
 * dropping the one there). */
static void nw_xml_attr_link(lxb_dom_element_t *el, lxb_dom_attr_t *attr) {
    if (el->first_attr == NULL) {
        el->first_attr = attr;
    } else {
        attr->prev = el->last_attr;
        el->last_attr->next = attr;
    }
    el->last_attr = attr;
    attr->owner = el;
}

/* Adds an attribute named `qname`, exactly as written, to element `n`.
 * Returns the attribute, or NULL when lexbor cannot allocate. The caller
 * makes sure no two attributes of an element share a name. */
lxb_dom_attr_t *nw_xml_attr(lxb_dom_node_t *n, const char *qname, size_t qlen, const char *value,
                            size_t vlen) {
    lxb_dom_document_t *d = n->owner_document;
    lxb_dom_attr_t *attr = lxb_dom_attr_interface_create(d);
    if (attr == NULL) {
        return NULL;
    }
    lxb_dom_attr_data_t *q =
        lxb_dom_attr_qualified_name_append(d->attrs, (const lxb_char_t *) qname, qlen);
    if (q == NULL) {
        return NULL;
    }
    attr->qualified_name = q->attr_id;
    int special = nw_xml_attr_set_local(d, attr, qname, qlen);
    if (special < 0) {
        return NULL;
    }
    if (lxb_dom_attr_set_value(attr, (const lxb_char_t *) value, vlen) != LXB_STATUS_OK) {
        return NULL;
    }
    lxb_dom_element_t *el = lxb_dom_interface_element(n);
    if (special) {
        nw_xml_attr_link(el, attr);
        nw_xml_attr_settle(el, attr);
    } else {
        lxb_dom_element_attr_append(el, attr);
        /* An element has one `id` and one `class` at most, so this runs
         * at most twice for it. */
        if (el->attr_id == attr || el->attr_class == attr) {
            nw_xml_attrs_settle(el);
        }
    }
    return attr;
}

/* A new comment or processing instruction with no parent, or NULL when
 * lexbor cannot allocate. */
lxb_dom_node_t *nw_create_comment(lxb_html_document_t *doc, const char *data, size_t len) {
    lxb_dom_comment_t *c = lxb_dom_document_create_comment(
        lxb_dom_interface_document(doc), (const lxb_char_t *) data, len);
    return c == NULL ? NULL : lxb_dom_interface_node(c);
}

/* lexbor's lxb_dom_document_create_processing_instruction refuses data
 * with `?>` in it, which the parser never gives but recover mode can make
 * by dropping a character from between `?` and `>`; this makes the node
 * the same way, without refusing. */
lxb_dom_node_t *nw_create_pi(lxb_html_document_t *doc, const char *target, size_t tlen,
                             const char *data, size_t dlen) {
    lxb_dom_document_t *d = lxb_dom_interface_document(doc);
    lxb_dom_processing_instruction_t *pi = lxb_dom_processing_instruction_interface_create(d);
    if (pi == NULL) {
        return NULL;
    }
    if (lexbor_str_init(&pi->char_data.data, d->text, dlen) == NULL
        || lexbor_str_init(&pi->target, d->text, tlen) == NULL) {
        return NULL;
    }
    lexbor_str_append(&pi->char_data.data, d->text, (const lxb_char_t *) data, dlen);
    lexbor_str_append(&pi->target, d->text, (const lxb_char_t *) target, tlen);
    return lxb_dom_interface_node(pi);
}

const char *nw_pi_target(lxb_dom_node_t *n, size_t *len) {
    lxb_dom_processing_instruction_t *pi = lxb_dom_interface_processing_instruction(n);
    *len = pi->target.length;
    return (const char *) pi->target.data;
}

/* An element's name as written, with its prefix. */
const char *nw_qname(lxb_dom_node_t *n, size_t *len) {
    return (const char *) lxb_dom_element_qualified_name(lxb_dom_interface_element(n), len);
}

const char *nw_attr_qname(lxb_dom_attr_t *a, size_t *len) {
    return (const char *) lxb_dom_attr_qualified_name(a, len);
}

/* Gives an element a new qualified name with the same local part (taking
 * its prefix off). Returns 0 on success. */
int nw_xml_rename(lxb_dom_node_t *n, const char *qname, size_t qlen) {
    lxb_dom_element_t *el = lxb_dom_interface_element(n);
    const lxb_tag_data_t *q =
        lxb_tag_append(n->owner_document->tags, n->local_name, (const lxb_char_t *) qname, qlen);
    if (q == NULL) {
        return -1;
    }
    el->qualified_name = (lxb_tag_id_t) q;
    return 0;
}

/* Takes an attribute off its element, leaving it allocated: a string
 * borrowed from it stays valid until the document goes. */
void nw_attr_remove(lxb_dom_node_t *n, lxb_dom_attr_t *a) {
    lxb_dom_element_t *el = lxb_dom_interface_element(n);
    int settle = el->attr_id == a || el->attr_class == a;
    lxb_dom_element_attr_remove(el, a);
    if (settle) {
        nw_xml_attrs_settle(el);
    }
}

/* The same for an attribute, whose new name must not be another of its
 * element's. Returns 0 on success. */
int nw_xml_attr_rename(lxb_dom_attr_t *a, const char *qname, size_t qlen) {
    lxb_dom_document_t *d = a->node.owner_document;
    lxb_dom_attr_data_t *q =
        lxb_dom_attr_qualified_name_append(d->attrs, (const lxb_char_t *) qname, qlen);
    if (q == NULL) {
        return -1;
    }
    a->qualified_name = q->attr_id;
    /* Only a prefixed name is renamed, and it's already linked in; one
     * renamed to `id` or `class` becomes the element's own. */
    int special = nw_xml_attr_set_local(d, a, qname, qlen);
    if (special < 0) {
        return -1;
    }
    lxb_dom_element_t *el = a->owner;
    if (special) {
        nw_xml_attr_settle(el, a);
    } else if (a->node.local_name == LXB_DOM_ATTR_ID || a->node.local_name == LXB_DOM_ATTR_CLASS) {
        if (a->node.local_name == LXB_DOM_ATTR_ID) {
            el->attr_id = a;
        } else {
            el->attr_class = a;
        }
        nw_xml_attrs_settle(el);
    }
    return 0;
}

/* HTML's named character `name` (given with its `;`), as UTF-8, or NULL
 * if there's none of that name. */
const char *nw_html_entity(const char *name, size_t len, size_t *out_len) {
    const lexbor_sbst_entry_static_t *strt = lxb_html_tokenizer_res_entities_sbst;
    const lexbor_sbst_entry_static_t *entry = &strt[1];
    for (size_t i = 0; i < len; i++) {
        entry = lexbor_sbst_entry_static_find(strt, entry, (lxb_char_t) name[i]);
        if (entry == NULL) {
            return NULL;
        }
        if (i + 1 == len) {
            break;
        }
        entry = &strt[entry->next];
    }
    if (len == 0 || entry->value == NULL) {
        return NULL;
    }
    *out_len = entry->value_len;
    return (const char *) entry->value;
}

/* Whether `name` is an element HTML knows, by lexbor's tag table. */
int nw_is_html_tag(const char *name, size_t len) {
    const lexbor_shs_entry_t *entry =
        lexbor_shs_entry_get_lower_static(lxb_tag_res_shs_data_default, (const lxb_char_t *) name, len);
    return entry != NULL;
}

/* ---- serialising ------------------------------------------------------- */

typedef void (*nw_chunk_fn)(const char *data, size_t len, void *ctx);

typedef struct {
    nw_chunk_fn chunk;
    void *ctx;
} nw_sink_t;

static lxb_status_t nw_on_chunk(const lxb_char_t *data, size_t len, void *ctx) {
    nw_sink_t *sink = ctx;
    sink->chunk((const char *) data, len, sink->ctx);
    return LXB_STATUS_OK;
}

/* Serialises `n` and everything below it as HTML, in pieces. Returns 0 on
 * success. Reads the tree only. */
int nw_serialize(lxb_dom_node_t *n, nw_chunk_fn chunk, void *ctx) {
    nw_sink_t sink = {chunk, ctx};
    return lxb_html_serialize_tree_cb(n, nw_on_chunk, &sink) == LXB_STATUS_OK ? 0 : -1;
}

/* Serialises `n` alone: an element's start tag, a text node escaped (or
 * raw inside <script> and the like), a comment or a doctype. */
int nw_serialize_one(lxb_dom_node_t *n, nw_chunk_fn chunk, void *ctx) {
    nw_sink_t sink = {chunk, ctx};
    return lxb_html_serialize_cb(n, nw_on_chunk, &sink) == LXB_STATUS_OK ? 0 : -1;
}

/* ---- CSS ---------------------------------------------------------------- */

/* An mraw whose free-block index starts small. lexbor_mraw_init always
 * gives that index (a binary tree in its own pools) room for 512 entries,
 * about 28 KB, before anything is freed into it. A compiled selector list
 * frees next to nothing, and the index grows if it does. */
static lexbor_mraw_t *nw_small_mraw(size_t chunk) {
    lexbor_mraw_t *mraw = lexbor_mraw_create();
    if (lexbor_mraw_init(mraw, chunk) != LXB_STATUS_OK) {
        return lexbor_mraw_destroy(mraw, true);
    }
    lexbor_bst_destroy(mraw->cache, true);
    mraw->cache = lexbor_bst_create();
    if (lexbor_bst_init(mraw->cache, 8) != LXB_STATUS_OK) {
        return lexbor_mraw_destroy(mraw, true);
    }
    return mraw;
}

/* Memory for one query: every selector list it compiles lives here, and
 * is freed with it by nw_css_memory_free. lexbor's own
 * (lxb_css_memory_init) is sized for a stylesheet, about 80 KB; this one
 * starts near 5 KB and grows if long selectors need it.
 *
 * This mirrors lxb_css_memory_init (lexbor/css/css.c) field by field, with
 * smaller sizes. Recheck it against that function when lexbor is upgraded:
 * a field added there would be missing here. */
lxb_css_memory_t *nw_css_memory_new(void) {
    static const size_t obj_size = sizeof(lxb_css_selector_t) > sizeof(lxb_css_selector_list_t)
                                       ? sizeof(lxb_css_selector_t)
                                       : sizeof(lxb_css_selector_list_t);
    lxb_css_memory_t *memory = lxb_css_memory_create();
    if (memory == NULL) {
        return NULL;
    }
    memory->objs = lexbor_dobject_create();
    if (lexbor_dobject_init(memory->objs, 16, obj_size) != LXB_STATUS_OK) {
        return lxb_css_memory_destroy(memory, true);
    }
    memory->tree = nw_small_mraw(1024);
    memory->mraw = nw_small_mraw(1024);
    if (memory->tree == NULL || memory->mraw == NULL) {
        return lxb_css_memory_destroy(memory, true);
    }
    memory->ref_count = 1;
    return memory;
}

void nw_css_memory_free(lxb_css_memory_t *memory) {
    lxb_css_memory_destroy(memory, true);
}

/* Compiles one selector list into `memory`, which owns it from then on: it
 * outlives the parser, and is freed only with the memory. On failure
 * whatever was half-built stays in `memory` until then; the caller drops
 * the whole query. */
lxb_css_selector_list_t *nw_css_compile(lxb_css_memory_t *memory, const char *css, size_t len) {
    lxb_css_parser_t *parser = lxb_css_parser_create();
    if (lxb_css_parser_init(parser, NULL) != LXB_STATUS_OK) {
        lxb_css_parser_destroy(parser, true);
        return NULL;
    }
    /* lexbor's parser doesn't free memory it was handed. */
    lxb_css_parser_memory_set(parser, memory);
    lxb_css_selector_list_t *list =
        lxb_css_selectors_parse(parser, (const lxb_char_t *) css, len);
    if (parser->status != LXB_STATUS_OK) {
        list = NULL;
    }
    lxb_css_parser_destroy(parser, true);
    return list;
}

typedef void (*nw_found_fn)(lxb_dom_node_t *node, void *ctx);

typedef struct {
    nw_found_fn found;
    void *ctx;
} nw_found_t;

static lxb_status_t nw_on_match(lxb_dom_node_t *node, lxb_css_selector_specificity_t spec,
                                void *ctx) {
    nw_found_t *f = ctx;
    f->found(node, f->ctx);
    return LXB_STATUS_OK;
}

/* A matcher for nw_select, kept and reused by one thread: making one costs
 * more than matching a selector inside a small element. NULL if lexbor
 * cannot allocate. */
lxb_selectors_t *nw_matcher_new(void) {
    lxb_selectors_t *sel = lxb_selectors_create();
    if (lxb_selectors_init(sel) != LXB_STATUS_OK) {
        lxb_selectors_destroy(sel, true);
        return NULL;
    }
    lxb_selectors_opt_set(sel, LXB_SELECTORS_OPT_MATCH_FIRST);
    return sel;
}

void nw_matcher_free(lxb_selectors_t *sel) {
    lxb_selectors_destroy(sel, true);
}

/*
 * Calls `found` once per element below `root` (not `root` itself) that matches
 * any selector in `list`, in document order. Returns 0 on success. `sel` is
 * left clean for the next call, keeping only its first block of memory.
 */
int nw_select(lxb_selectors_t *sel, lxb_dom_node_t *root, lxb_css_selector_list_t *list,
              nw_found_fn found, void *ctx) {
    nw_found_t f = {found, ctx};
    lxb_status_t status = lxb_selectors_find(sel, root, list, nw_on_match, &f);
    lxb_selectors_clean(sel);
    return status == LXB_STATUS_OK ? 0 : -1;
}
