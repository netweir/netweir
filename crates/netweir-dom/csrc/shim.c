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
