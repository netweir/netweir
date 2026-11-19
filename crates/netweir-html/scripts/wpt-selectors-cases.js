#!/usr/bin/env node
// Turns web-platform-tests' selector tests into data for tests/wpt-selectors.rs.
// Run by scripts/fetch-wpt-selectors.sh:
//   node scripts/wpt-selectors-cases.js WPT_CHECKOUT tests/wpt-selectors
//
// The tests are JavaScript, so each test file's inline scripts run here in
// a sandbox whose `document` and elements aren't a DOM: querySelector,
// querySelectorAll, matches and closest return placeholders, and the
// assertions the test makes about those placeholders are recorded as cases
// (selector, root, what the test expects). A test that needs anything a
// static document can't give (computed style, focus, shadow DOM, a live
// NodeList) throws inside the sandbox and is listed as skipped with the
// reason, as is a whole file nothing in which could run.
'use strict';

const fs = require('fs');
const path = require('path');
const vm = require('vm');

const [src, out] = process.argv.slice(2);
if (!src || !out) {
  console.error('usage: wpt-selectors-cases.js WPT_CHECKOUT OUT_DIR');
  process.exit(2);
}

// ---- dom/nodes: selectors.js is a data list -------------------------------

function domNodesData() {
  const code = fs.readFileSync(path.join(src, 'dom/nodes/selectors.js'), 'utf8');
  const lists = vm.runInNewContext(
    code + ';({invalidSelectors, validSelectors, scopedSelectors})',
    {}
  );
  return {
    invalid: lists.invalidSelectors,
    valid: lists.validSelectors,
    scoped: lists.scopedSelectors,
  };
}

// ---- the sandbox ----------------------------------------------------------

class Unsupported extends Error {}
const unsupported = (what) => {
  throw new Unsupported(what);
};

const TOKEN = /^(\d+)$/;

// What a selector query returned: a placeholder the assertions give meaning to.
class Sym {
  constructor(syms, kind, root, selector) {
    if (typeof selector !== 'string') unsupported('a selector that is not a string');
    this.kind = kind; // all | first | matches | closest
    this.root = root;
    this.selector = selector;
    this.n = syms.push(this) - 1;
  }
  get token() {
    return `${this.n}`;
  }
}

function symProxy(sym) {
  return new Proxy(sym, {
    get(target, p) {
      if (p === '__sym') return target;
      if (p === 'id') {
        if (target.kind === 'first' || target.kind === 'closest') return target.token;
        unsupported('.id of a ' + target.kind + ' result');
      }
      if (target.kind === 'all') {
        // Array.from(list).map(e => e.id).sort().join() then yields the token.
        if (p === Symbol.iterator) {
          return function* () {
            yield new Proxy(
              {},
              {
                get(_, q) {
                  if (q === 'id') return target.token;
                  unsupported('an element of a querySelectorAll result (.' + String(q) + ')');
                },
              }
            );
          };
        }
        if (p === 'length') return target.token;
      }
      unsupported(`.${String(p)} of a ${target.kind} result`);
    },
  });
}

function makeSandbox(elements) {
  const syms = [];
  const refs = new WeakMap(); // element proxy -> "#id" | "html" | "body"
  const state = { test: null, cases: [], skipped: [], throwing: null };

  function element(ref) {
    const proxy = new Proxy(
      {},
      {
        get(_, p) {
          const known = elements.get(ref);
          switch (p) {
            case 'querySelector':
              return (s) => symProxy(new Sym(syms, 'first', ref, s));
            case 'querySelectorAll':
              return (s) => symProxy(new Sym(syms, 'all', ref, s));
            case 'matches':
            case 'webkitMatchesSelector':
              return (s) => symProxy(new Sym(syms, 'matches', ref, s));
            case 'closest':
              return (s) => symProxy(new Sym(syms, 'closest', ref, s));
            case 'id':
              return ref.startsWith('#') ? ref.slice(1) : '';
            // Read off the element's start tag in the HTML.
            case 'tagName':
              if (known) return known.tag.toUpperCase();
              break;
            case 'localName':
              if (known) return known.tag;
              break;
            case 'getAttribute':
              if (known) return (name) => known.attrs.get(name.toLowerCase()) ?? null;
              break;
            case 'hasAttribute':
              if (known) return (name) => known.attrs.has(name.toLowerCase());
              break;
            case 'dataset':
              if (known) {
                return new Proxy(
                  {},
                  {
                    get: (_, q) =>
                      known.attrs.get('data-' + String(q).replace(/[A-Z]/g, (c) => '-' + c.toLowerCase())),
                  }
                );
              }
              break;
            // Only ever a test's label in these files.
            case 'textContent':
              return ref;
          }
          unsupported(`element.${String(p)}`);
        },
      }
    );
    refs.set(proxy, ref);
    return proxy;
  }

  const document = new Proxy(
    {},
    {
      get(_, p) {
        switch (p) {
          case 'querySelector':
            // Looking an element up by id is setup, not the thing under test.
            return (s) =>
              /^#[\w-]+$/.test(s) ? element(s) : symProxy(new Sym(syms, 'first', 'document', s));
          case 'querySelectorAll':
            return (s) => symProxy(new Sym(syms, 'all', 'document', s));
          case 'getElementById':
            return (id) => element('#' + id);
          case 'documentElement':
            return element('html');
          case 'body':
            return element('body');
          default:
            unsupported(`document.${String(p)}`);
        }
      },
    }
  );

  // The Sym an assertion's argument stands for, if any.
  function symOf(x) {
    if (x && typeof x === 'object' && x.__sym instanceof Sym) return x.__sym;
    if (typeof x === 'string') {
      const m = TOKEN.exec(x);
      if (m) return syms[+m[1]];
    }
    if (Array.isArray(x) && x.length === 1) return symOf(x[0]);
    return null;
  }

  // An expected element: "#id", "html", "body" or null.
  function expectedRef(x) {
    if (x === null || x === undefined || x === '') return null;
    if (typeof x === 'string') return '#' + x;
    if (refs.has(x)) return refs.get(x);
    unsupported('an expected value that is not an element or an id');
  }

  function record(c) {
    if (!state.test) unsupported('an assertion outside test()');
    state.cases.push(Object.assign({ test: state.test }, c));
  }

  function assertEquals(a, b) {
    let sym = symOf(a);
    let other = b;
    if (!sym) {
      sym = symOf(b);
      other = a;
    }
    if (!sym) return; // nothing of ours in it
    const base = { kind: sym.kind, root: sym.root, selector: sym.selector };
    if (sym.kind === 'all') {
      if (typeof other === 'number') {
        return record(Object.assign(base, { kind: 'count', expect: other }));
      }
      if (typeof other === 'string') {
        if (//.test(other)) unsupported('comparing two query results');
        // formatElements(): ids sorted and joined with commas.
        const ids = other === '' ? [] : other.split(',');
        return record(Object.assign(base, { expect: ids.map((i) => '#' + i), ordered: false }));
      }
      if (Array.isArray(other)) {
        return record(Object.assign(base, { expect: other.map(expectedRef), ordered: true }));
      }
      unsupported('comparing a querySelectorAll result with ' + typeof other);
    }
    if (sym.kind === 'matches') {
      if (typeof other !== 'boolean') unsupported('comparing matches() with ' + typeof other);
      return record(Object.assign(base, { expect: other }));
    }
    record(Object.assign(base, { expect: expectedRef(other) }));
  }

  function assertBool(x, expect) {
    const sym = symOf(x);
    if (!sym) return;
    if (sym.kind !== 'matches') unsupported(`assert_${expect} on a ${sym.kind} result`);
    record({ kind: 'matches', root: sym.root, selector: sym.selector, expect });
  }

  function assertThrows(...args) {
    const fn = args.find((a) => typeof a === 'function');
    if (!fn) unsupported('assert_throws without a function');
    const before = syms.length;
    state.throwing = true;
    try {
      fn();
    } catch (e) {
      if (e instanceof Unsupported) throw e;
      // Setup inside the function failing before the query is fine: the
      // queries made so far are the cases.
    } finally {
      state.throwing = false;
    }
    if (syms.length === before) unsupported('assert_throws without a selector query');
    for (const s of syms.slice(before)) {
      record({ kind: 'parse', selector: s.selector, expect: false });
    }
  }

  function test(fn, name) {
    if (typeof fn !== 'function') [fn, name] = [name, fn];
    const label = typeof name === 'string' ? name : '(unnamed)';
    state.test = label;
    const before = state.cases.length;
    try {
      fn({ add_cleanup() {}, step_func: (f) => f, done() {} });
      if (state.cases.length === before) {
        state.skipped.push({ test: label, reason: 'asserts nothing about a selector query' });
      }
    } catch (e) {
      state.cases.length = before;
      const reason =
        e instanceof Unsupported
          ? `needs ${e.message}`
          : `${e && e.constructor ? e.constructor.name : 'error'}: ${e && e.message}`;
      state.skipped.push({ test: label, reason });
    }
    state.test = null;
  }

  function skipTest(reason) {
    return (fn, name) => {
      if (typeof fn !== 'function') [fn, name] = [name, fn];
      state.skipped.push({ test: typeof name === 'string' ? name : '(unnamed)', reason });
    };
  }

  // /css/support/parsing-testcommon.js: serialisation isn't checked, only
  // whether the selector parses.
  function parseCase(valid) {
    return (selector) => {
      state.test = JSON.stringify(selector) + (valid ? ' should be a valid selector' : ' should be an invalid selector');
      state.cases.push({ test: state.test, kind: 'parse', selector, expect: valid });
      state.test = null;
    };
  }

  // /css/support/query-testcommon.js
  function testQuerySelector(root, selector, expected) {
    if (!Array.isArray(expected)) expected = [expected];
    const ref = root === sandbox.document ? 'document' : refs.get(root);
    if (!ref) unsupported('a query root that is not an element');
    state.test = `Selector '${selector}" should find the expected elements`;
    state.cases.push({
      test: state.test,
      kind: 'all',
      root: ref,
      selector,
      expect: expected.map(expectedRef),
      ordered: true,
    });
    state.test = null;
  }

  const sandbox = {
    document,
    test,
    setup() {},
    done() {},
    promise_test: skipTest('runs asynchronously'),
    async_test: skipTest('runs asynchronously'),
    assert_equals: assertEquals,
    assert_not_equals: (a, b) => {
      if (symOf(a) || symOf(b)) unsupported('assert_not_equals on a query result');
    },
    assert_array_equals: assertEquals,
    assert_true: (x) => assertBool(x, true),
    assert_false: (x) => assertBool(x, false),
    assert_throws_dom: assertThrows,
    assert_throws_js: assertThrows,
    assert_unreached: (m) => {
      throw new Error('assert_unreached: ' + m);
    },
    test_valid_selector: parseCase(true),
    test_valid_forgiving_selector: parseCase(true),
    test_invalid_selector: parseCase(false),
    test_query_selector: testQuerySelector,
    console: { log() {} },
    _loadHandlers: [],
    addEventListener(type, fn) {
      if (type === 'load') sandbox._loadHandlers.push(fn);
    },
  };
  sandbox.window = sandbox;
  sandbox.self = sandbox;
  for (const ref of elements.keys()) {
    const id = ref.slice(1);
    if (/^[A-Za-z_$][\w$]*$/.test(id) && !(id in sandbox)) sandbox[id] = element(ref);
  }
  return { sandbox, state };
}

// ---- one test file --------------------------------------------------------

const SKIP_PATTERNS = [
  [/getComputedStyle|\.style\b|cssRules|CSS\.supports/, 'computed style or CSSOM'],
  [/test_driver|testdriver|\.focus\(|\.click\(|hover|active|accesskey/i, 'user interaction'],
  [/attachShadow|shadowRoot|<slot|::part|::slotted/, 'shadow DOM'],
  [/innerHTML|appendChild|insertAdjacent|createElement|\.remove\(\)|setAttribute|removeAttribute|classList|className/,
    'a live DOM changed from script'],
  [/promise_test|async_test|requestAnimationFrame|setTimeout|<iframe/, 'running asynchronously in a browser'],
];

function reasonFor(html, state) {
  const m = SKIP_PATTERNS.find(([re]) => re.test(html));
  if (m) return `needs ${m[1]}`;
  const first = state && state.skipped[0];
  return first ? first.reason : 'reads the DOM from script';
}

function extract(rel) {
  const html = fs.readFileSync(path.join(src, rel), 'utf8');
  const entry = { file: rel };
  if (/\.xht(ml)?$/.test(rel)) return Object.assign(entry, { skipped_because: 'an XHTML document' });
  if (/\/crashtests\//.test(rel)) return Object.assign(entry, { skipped_because: 'a crash test: needs a browser' });
  if (/\/invalidation\//.test(rel)) return Object.assign(entry, { skipped_because: 'invalidation: needs a live DOM' });
  if (/-manual\.html$/.test(rel)) return Object.assign(entry, { skipped_because: 'a manual test' });
  if (!/testharness\.js/.test(html)) return Object.assign(entry, { skipped_because: 'a reftest: needs rendering' });

  // Every element with an id, by "#id": its tag and attributes.
  const elements = new Map();
  for (const [, tag, rest] of html.matchAll(/<([a-zA-Z][\w-]*)(\s[^>]*)?>/g)) {
    const attrs = new Map();
    for (const m of (rest ?? '').matchAll(/([^\s"'=<>\/]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+)))?/g)) {
      attrs.set(m[1].toLowerCase(), m[2] ?? m[3] ?? m[4] ?? '');
    }
    if (attrs.has('id') && !elements.has('#' + attrs.get('id'))) {
      elements.set('#' + attrs.get('id'), { tag: tag.toLowerCase(), attrs });
    }
  }
  const { sandbox, state } = makeSandbox(elements);
  const context = vm.createContext(sandbox);
  const scripts = [...html.matchAll(/<script(\s[^>]*)?>([\s\S]*?)<\/script>/gi)];
  try {
    for (const [, attrs, code] of scripts) {
      if (attrs && /\bsrc\s*=/.test(attrs)) {
        const srcAttr = /src\s*=\s*["']?([^"'\s>]+)/.exec(attrs)[1];
        if (!/testharness|testharnessreport|parsing-testcommon|query-testcommon/.test(srcAttr)) {
          unsupported(`the script ${srcAttr}`);
        }
        continue;
      }
      if (attrs && /type\s*=\s*["']?(module|text\/plain)/.test(attrs)) unsupported('a module script');
      vm.runInContext(code, context, { timeout: 5000, filename: rel });
    }
    for (const fn of [sandbox.onload, ...sandbox._loadHandlers]) {
      if (typeof fn === 'function') fn();
    }
  } catch (e) {
    if (state.cases.length === 0) {
      const reason = e instanceof Unsupported ? `needs ${e.message}` : reasonFor(html, state);
      return Object.assign(entry, { skipped_because: reason });
    }
    state.skipped.push({ test: '(rest of the file)', reason: e instanceof Unsupported ? `needs ${e.message}` : String(e) });
  }
  if (state.cases.length === 0) return Object.assign(entry, { skipped_because: reasonFor(html, state) });
  return Object.assign(entry, { cases: state.cases, skipped: state.skipped });
}

function walk(dir, rel = '') {
  const out = [];
  for (const name of fs.readdirSync(path.join(dir, rel)).sort()) {
    const r = path.join(rel, name);
    if (fs.statSync(path.join(dir, r)).isDirectory()) out.push(...walk(dir, r));
    else out.push(r);
  }
  return out;
}

const files = walk(src, 'css/selectors')
  .filter((f) => /\.(html|xht|xhtml)$/.test(f))
  .filter((f) => !/-ref\.|\/reference\/|\/resources\//.test(f))
  .filter((f) => !/-notref\.|-noref\./.test(f));
files.unshift('dom/nodes/Element-closest.html');

const result = {
  commit: fs.readFileSync(path.join(src, 'COMMIT'), 'utf8').trim(),
  dom: domNodesData(),
  files: files.map(extract),
};

fs.mkdirSync(out, { recursive: true });
fs.writeFileSync(path.join(out, 'cases.json'), JSON.stringify(result, null, 1) + '\n');
const keep = ['dom/nodes/ParentNode-querySelector-All-content.html'];
for (const f of result.files) if (f.cases) keep.push(f.file);
for (const f of keep) {
  fs.mkdirSync(path.join(out, path.dirname(f)), { recursive: true });
  fs.copyFileSync(path.join(src, f), path.join(out, f));
}
const ran = result.files.filter((f) => f.cases);
console.error(
  `${ran.length} files with ${ran.reduce((n, f) => n + f.cases.length, 0)} cases, ` +
    `${result.files.length - ran.length} files skipped`
);
