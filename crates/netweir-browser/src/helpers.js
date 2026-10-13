// netweir's helpers. They run in an isolated world: they see the page's
// DOM, but the page can't see them, and nothing the page does to its own
// globals reaches them.
(() => {
  // The next animation frame, or 100 ms if frames aren't running.
  const frame = () =>
    new Promise((resolve) => {
      requestAnimationFrame(() => resolve());
      setTimeout(resolve, 100);
    });

  const box = (el) => {
    const r = el.getBoundingClientRect();
    return [r.x, r.y, r.width, r.height];
  };

  const visible = (el) => {
    const [, , w, h] = box(el);
    return w > 0 && h > 0 && getComputedStyle(el).visibility !== "hidden";
  };

  // The boxes between `el` and the document that scroll by themselves.
  const scrollers = (el) => {
    const found = [];
    for (let p = el; p && p !== document.body && p !== document.documentElement; p = p.parentElement) {
      const s = getComputedStyle(p);
      const scrolls = /(auto|scroll)/.test(s.overflowX + s.overflowY);
      if (scrolls && (p.scrollHeight > p.clientHeight || p.scrollWidth > p.clientWidth)) found.push(p);
    }
    return found;
  };

  const enabled = (el) => !el.matches(":disabled") && !el.closest('[aria-disabled="true"]');

  globalThis.__netweir = {
    // Whether the element `selector` finds passes `checks`, in order. The
    // first that fails is named; otherwise its centre is returned.
    async check(selector, checks, scroll) {
      const el = document.querySelector(selector);
      if (!el) return { failed: "attached" };
      if (scroll) el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
      let b = box(el);
      for (const check of checks) {
        if (check === "visible" && !visible(el)) return { failed: "visible" };
        if (check === "enabled" && !enabled(el)) return { failed: "enabled" };
        if (check === "editable") {
          // A <select> isn't typed into; fill would do nothing to it.
          const field = el.matches("input, textarea") || el.isContentEditable;
          const readOnly = el.readOnly || el.getAttribute("aria-readonly") === "true";
          if (!field || readOnly || !enabled(el)) return { failed: "editable" };
        }
        if (check === "stable") {
          await frame();
          const before = box(el);
          await frame();
          b = box(el);
          if (before.some((v, i) => v !== b[i])) return { failed: "stable" };
        }
        if (check === "receives events") {
          const hit = document.elementFromPoint(b[0] + b[2] / 2, b[1] + b[3] / 2);
          if (!hit || (hit !== el && !el.contains(hit))) return { failed: "receives events" };
        }
      }
      return { x: b[0] + b[2] / 2, y: b[1] + b[3] / 2 };
    },

    // "detached", "hidden" or "visible".
    state(selector) {
      const el = document.querySelector(selector);
      if (!el) return "detached";
      return visible(el) ? "visible" : "hidden";
    },

    // Focuses the element and selects what's in it, so typing replaces it.
    focus(selector) {
      const el = document.querySelector(selector);
      if (!el) return false;
      el.focus();
      if (typeof el.select === "function") {
        el.select();
      } else if (el.isContentEditable) {
        const range = document.createRange();
        range.selectNodeContents(el);
        const selection = getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
      }
      return document.activeElement === el;
    },

    content() {
      const doctype = document.doctype ? new XMLSerializer().serializeToString(document.doctype) : "";
      return doctype + (document.documentElement ? document.documentElement.outerHTML : "");
    },

    // Where a point (x, y) of the document in the <iframe> `el` is in this
    // document. The page is scrolled to bring it into view first, if it's
    // out of it, since the mouse can't reach it there. With `hit`, the
    // frame must be what's at that point.
    offset(el, x, y, hit) {
      const at = () => {
        const r = el.getBoundingClientRect();
        const s = getComputedStyle(el);
        return [
          r.left + el.clientLeft + parseFloat(s.paddingLeft) + x,
          r.top + el.clientTop + parseFloat(s.paddingTop) + y,
        ];
      };
      let [px, py] = at();
      if (px < 0 || py < 0 || px >= innerWidth || py >= innerHeight) {
        scrollBy({ left: px - innerWidth / 2, top: py - innerHeight / 2, behavior: "instant" });
        [px, py] = at();
      }
      if (hit && document.elementFromPoint(px, py) !== el) return { failed: "receives events" };
      return { x: px, y: py };
    },

    // Where a wheel scrolls the document: the middle of the viewport,
    // unless what's there scrolls by itself (a box, or a frame) and would
    // take the wheel first; then the first of a grid of points, out to the edges, where
    // nothing does. `skip` passes over that many, for when one moved
    // nothing; past the free ones come the rest. Kept, so where() can see
    // what scrolls under it.
    pagePoint(skip) {
      const [w, h] = [innerWidth, innerHeight];
      const points = [];
      // Out to the edges, where a person would put the mouse to get past a
      // box that fills the middle.
      const at = [0.5, 0.25, 0.75, 0.1, 0.9, 0.02, 0.98];
      for (const fy of at) for (const fx of at) points.push([w * fx, h * fy]);
      const free = ([x, y]) => {
        const el = document.elementFromPoint(x, y);
        return el && !el.matches("iframe, frame") && !scrollers(el).length;
      };
      const ordered = [...points.filter(free), ...points.filter((p) => !free(p))];
      const [x, y] = ordered[Math.min(skip, ordered.length - 1)];
      this.wheelAt = [x, y];
      return { x, y };
    },

    // How far the page has scrolled and how far it can.
    metrics() {
      const s = document.scrollingElement || document.documentElement;
      return { y: scrollY, max: s.scrollHeight - innerHeight, height: s.scrollHeight, view: innerHeight };
    },

    // Where the page, the boxes under the wheel, and the element
    // `selector` finds if given, are scrolled to, as one string to compare.
    where(selector) {
      const parts = [scrollX, scrollY];
      const under = !selector && this.wheelAt && document.elementFromPoint(...this.wheelAt);
      for (const box of under ? scrollers(under) : []) parts.push(box.scrollLeft, box.scrollTop);
      // A frame under the wheel scrolls its own document first. One from
      // this origin can be read; one from another can't, so it's marked
      // "?", counted as moving by settle(), and the caller gives up on a
      // page that doesn't follow.
      if (under && under.matches("iframe, frame")) {
        try {
          const w = under.contentWindow;
          parts.push(w.scrollX, w.scrollY);
        } catch {
          parts.push("?");
        }
      }
      const el = selector && document.querySelector(selector);
      if (el) {
        const r = el.getBoundingClientRect();
        parts.push(el.scrollLeft, el.scrollTop, r.left, r.top);
      }
      return parts.join(",");
    },

    // Waits for a scroll from `before` to start, for up to ten frames, and
    // then to stop: three frames the same. Returns whether anything moved.
    async settle(selector, before) {
      let last = this.where(selector);
      let moved = last !== before || before.endsWith("?");
      let still = 0;
      for (let i = 0; i < 60 && (moved ? still < 3 : i < 10); i++) {
        await frame();
        const now = this.where(selector);
        if (now !== before) moved = true;
        still = now === last ? still + 1 : 0;
        last = now;
      }
      return moved;
    },

    // What it takes to wheel the element `selector` finds into view: where
    // to put the mouse (over the box that scrolls it, or the middle of the
    // viewport) and by how much, or, once it's in view, its middle.
    async aim(selector) {
      const c = await this.check(selector, ["attached", "visible"], false);
      if (c.failed) return c;
      const el = document.querySelector(selector);
      let box = { left: 0, top: 0, right: innerWidth, bottom: innerHeight };
      for (let p = el.parentElement; p && p !== document.body && p !== document.documentElement; p = p.parentElement) {
        const s = getComputedStyle(p);
        const scrolls = /(auto|scroll)/.test(s.overflowX + s.overflowY);
        if (scrolls && (p.scrollHeight > p.clientHeight || p.scrollWidth > p.clientWidth)) {
          const b = p.getBoundingClientRect();
          box = {
            left: Math.max(b.left, 0),
            top: Math.max(b.top, 0),
            right: Math.min(b.right, innerWidth),
            bottom: Math.min(b.bottom, innerHeight),
          };
          break;
        }
      }
      const r = el.getBoundingClientRect();
      const [cx, cy] = [r.left + r.width / 2, r.top + r.height / 2];
      const inside = (b) => cx >= b.left && cx < b.right && cy >= b.top && cy < b.bottom;
      const viewport = { left: 0, top: 0, right: innerWidth, bottom: innerHeight };
      if (inside(box) && inside(viewport)) return { done: true, x: cx, y: cy };
      // A box scrolled out of view can't be wheeled; the caller falls back.
      if (box.right <= box.left || box.bottom <= box.top) return { stuck: true, x: 0, y: 0 };
      const [bx, by] = [(box.left + box.right) / 2, (box.top + box.bottom) / 2];
      return { x: bx, y: by, dx: cx - bx, dy: cy - by };
    },

    // Scrolls the element into view without the mouse, as click does.
    reveal(selector) {
      const el = document.querySelector(selector);
      if (el) el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    },

    // Whether the checkbox or radio button `selector` finds is checked. A
    // label speaks for its control; role=checkbox, radio and switch count.
    // One that isn't `want` (true or false) has to be enabled to change.
    checked(selector, want) {
      let el = document.querySelector(selector);
      if (!el) return { failed: "attached" };
      if (el.matches("label") && el.control) el = el.control;
      let state;
      if (el.matches("input[type=checkbox], input[type=radio]")) {
        state = { checked: el.checked, radio: el.type === "radio" };
      } else {
        const role = el.getAttribute("role");
        if (!["checkbox", "radio", "switch", "menuitemcheckbox", "menuitemradio"].includes(role)) {
          return { error: "it isn't a checkbox or a radio button" };
        }
        state = { checked: el.getAttribute("aria-checked") === "true", radio: role.endsWith("radio") };
      }
      if (want !== null && state.checked !== want && !enabled(el)) return { failed: "enabled" };
      return state;
    },

    // Selects the options of the <select> `selector` finds whose `by`
    // ("value", "label" or "index") is one of `wanted`, and tells the page
    // as a person's choice would: input, then change, and only if the
    // choice is a new one. Returns the values selected.
    async select(selector, by, wanted) {
      const c = await this.check(selector, ["attached", "visible", "enabled"], false);
      if (c.failed) return c;
      const el = document.querySelector(selector);
      if (!el.matches("select")) return { error: "it isn't a <select>" };
      if (wanted.length > 1 && !el.multiple) {
        return { error: `it takes one option, and ${wanted.length} were asked for` };
      }
      const options = [...el.options];
      const picked = [];
      for (const w of wanted) {
        const o = options.find((o) => {
          if (by === "index") return o.index === w;
          if (by === "label") return o.label === w || o.textContent.trim() === w;
          return o.value === w;
        });
        if (!o) return { failed: "option", why: `no option has the ${by} ${JSON.stringify(w)}` };
        if (o.disabled || (o.parentElement.matches("optgroup") && o.parentElement.disabled)) {
          return { failed: "option", why: `the option with the ${by} ${JSON.stringify(w)} stayed disabled` };
        }
        picked.push(o);
      }
      const changed = options.some((o) => o.selected !== picked.includes(o));
      if (changed) {
        for (const o of options) o.selected = picked.includes(o);
        el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
      }
      return { values: [...el.selectedOptions].map((o) => o.value) };
    },

    // Whether the element `selector` finds can be given `count` files.
    fileInput(selector, count) {
      const el = document.querySelector(selector);
      if (!el) return { failed: "attached" };
      if (!el.matches("input[type=file]")) return { error: "it isn't an <input type=file>" };
      if (count > 1 && !el.multiple) return { error: `it takes one file, and ${count} were given` };
      if (!enabled(el)) return { failed: "enabled" };
      return {};
    },

    // Empties a file input, telling the page as a choice would, if it held
    // anything.
    clearFiles(selector) {
      const el = document.querySelector(selector);
      if (!el || !el.files.length) return;
      el.value = "";
      el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
    },
  };
})();
