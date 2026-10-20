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

  // Whether a wheel at (x, y) goes to the document: nothing there scrolls
  // by itself, and it isn't a frame.
  const free = ([x, y]) => {
    const el = document.elementFromPoint(x, y);
    return el && !el.matches("iframe, frame") && !scrollers(el).length;
  };

  const enabled = (el) => !el.matches(":disabled") && !el.closest('[aria-disabled="true"]');

  globalThis.__netweir = {
    // Where what a finger at (x, y) drags is scrolled to, and as far as
    // it can go: [left, top, most left, most top]. With `still`, once it
    // has stopped: the same two frames running, for half a second at
    // most, since the page's script hears of a finger's scroll a frame or
    // more after Chrome has drawn it.
    async swipeMark(x, y, still) {
      const read = () => {
        const el = document.elementFromPoint(x, y);
        const s = el && scrollers(el)[0];
        if (s) return [s.scrollLeft, s.scrollTop, s.scrollWidth - s.clientWidth, s.scrollHeight - s.clientHeight];
        const d = document.scrollingElement || document.documentElement;
        return [scrollX, scrollY, d.scrollWidth - d.clientWidth, d.scrollHeight - d.clientHeight];
      };
      let last = read();
      for (let i = 0, same = 0; still && same < 2 && i < 30; i++) {
        await frame();
        const now = read();
        same = now.join() === last.join() ? same + 1 : 0;
        last = now;
      }
      return last;
    },
    // The box a finger at (x, y) would drag: the nearest one that scrolls
    // by itself, or a frame, as [left, top, right, bottom]; null for the
    // page's own.
    swipeArea(x, y) {
      const el = document.elementFromPoint(x, y);
      if (!el) return null;
      const s = el.matches("iframe, frame") ? el : scrollers(el)[0];
      if (!s) return null;
      const r = s.getBoundingClientRect();
      return [r.left, r.top, r.right, r.bottom];
    },

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

    // A point just outside the element `selector` finds, in view, where
    // the mouse can come onto it from; none if it fills the window.
    beside(selector) {
      const el = document.querySelector(selector);
      if (!el) return { failed: "attached" };
      const r = el.getBoundingClientRect();
      const [x, y] = [r.left + r.width / 2, r.top + r.height / 2];
      if (r.top >= 3) return { x, y: r.top - 2 };
      if (r.bottom <= innerHeight - 3) return { x, y: r.bottom + 2 };
      if (r.left >= 3) return { x: r.left - 2, y };
      if (r.right <= innerWidth - 3) return { x: r.right + 2, y };
      return { none: true, x: 0, y: 0 };
    },

    // Listens, from here, where the page can't see it, for the mouse
    // coming over the element `selector` finds or anything in it.
    armOver(selector) {
      if (this.disarm) this.disarm();
      this.over = false;
      const el = document.querySelector(selector);
      if (!el) return;
      const seen = () => {
        this.over = true;
      };
      el.addEventListener("mouseover", seen, true);
      this.disarm = () => {
        el.removeEventListener("mouseover", seen, true);
        this.disarm = null;
      };
    },

    // Whether that mouseover has come, waiting a few frames for it.
    async overSeen() {
      for (let i = 0; i < 10 && !this.over; i++) await frame();
      if (this.disarm) this.disarm();
      return this.over;
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
      const ordered = [...points.filter(free), ...points.filter((p) => !free(p))];
      const [x, y] = ordered[Math.min(skip, ordered.length - 1)];
      this.wheelAt = [x, y];
      return { x, y };
    },

    // The first place pagePoint() would pick.
    freeSpot() {
      const { x, y } = this.pagePoint(0);
      return [x, y];
    },

    // How far the page has scrolled and how far it can: against the height
    // the page scrolls in, which, unlike innerHeight, leaves out a
    // horizontal scrollbar.
    metrics() {
      const s = document.scrollingElement || document.documentElement;
      return { y: scrollY, max: s.scrollHeight - s.clientHeight, height: s.scrollHeight, view: s.clientHeight };
    },

    // Scrolls the page by script to as far as it goes: past it, for the
    // browser to stop at the real end, and at once, whatever the page's
    // scroll-behavior says.
    toEnd() {
      const s = document.scrollingElement || document.documentElement;
      scrollTo({ left: scrollX, top: s.scrollHeight, behavior: "instant" });
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

    // What it takes to wheel the element `selector` finds into view. Each
    // box that scrolls it is asked in turn, innermost first and the
    // document last: the first that doesn't show all of it (or its middle,
    // if it's too big to show whole), and can still scroll the way it
    // needs, is where the mouse goes and by how much. Once every one shows
    // it, it's done and its middle is returned; if one doesn't and none
    // can scroll, it's stuck, and the caller falls back.
    async aim(selector) {
      const c = await this.check(selector, ["attached", "visible"], false);
      if (c.failed) return c;
      const el = document.querySelector(selector);
      const r = el.getBoundingClientRect();
      const [cx, cy] = [r.left + r.width / 2, r.top + r.height / 2];
      const viewport = { left: 0, top: 0, right: innerWidth, bottom: innerHeight };
      const clip = (b) => ({
        left: Math.max(b.left, 0),
        top: Math.max(b.top, 0),
        right: Math.min(b.right, innerWidth),
        bottom: Math.min(b.bottom, innerHeight),
      });
      const fits = (lo, hi, a, b, mid) => (hi - lo <= b - a ? lo >= a - 0.5 && hi <= b + 0.5 : mid >= a && mid < b);
      const shows = (b) => fits(r.left, r.right, b.left, b.right, cx) && fits(r.top, r.bottom, b.top, b.bottom, cy);
      const room = (d, at, max) => (d > 0.5 ? at < max - 0.5 : d < -0.5 ? at > 0.5 : false);
      const page = document.scrollingElement || document.documentElement;
      const boxes = scrollers(el.parentElement).map((p) => [p, clip(p.getBoundingClientRect())]);
      boxes.push([page, viewport]);
      for (const [p, b] of boxes) {
        // One scrolled out of view itself is brought in by the next.
        if (b.right <= b.left || b.bottom <= b.top || shows(b)) continue;
        // The page is wheeled where nothing that scrolls by itself is
        // under the mouse, so a box at its limit can't take the turn.
        const [bx, by] = p === page ? this.freeSpot() : [(b.left + b.right) / 2, (b.top + b.bottom) / 2];
        const [dx, dy] = [cx - (b.left + b.right) / 2, cy - (b.top + b.bottom) / 2];
        const can =
          room(dy, p.scrollTop, p.scrollHeight - p.clientHeight) ||
          room(dx, p.scrollLeft, p.scrollWidth - p.clientWidth);
        if (can) return { x: bx, y: by, dx, dy };
      }
      if (shows(viewport)) return { done: true, x: cx, y: cy };
      return { stuck: true, x: 0, y: 0 };
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
