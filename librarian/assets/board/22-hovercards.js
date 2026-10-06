      // ---- Entity hovercard engine ----
      //
      // The tooltip engine (20-util.js) answers "what does this control do" for
      // things you click. This one answers "what *is* this" for things you read: a
      // worktree path, a branch name, a review id, a pull request. Those are the
      // values a reviewer inspects most and, until now, they were inert text — the
      // only way to learn what a worktree actually was meant leaving the page.
      //
      // Two engines rather than one because the jobs differ. A tooltip is a short
      // string that follows the mouse and is never interactive. A hovercard is a
      // small panel anchored to its element, holds structured key/value detail, and
      // stays open while you move into it so its buttons can be clicked. Putting
      // actions inside a mouse-following tooltip would make them unreachable.
      //
      // Usage: give an element `data-card="<kind>"` plus whatever `data-*` fields
      // that kind's renderer needs, and register the renderer with
      // `registerHoverCard(kind, fn)` from the chunk that owns the entity. An
      // element carrying `data-card` should not also carry `data-tip` — the card
      // supersedes it, and showing both at once reads as a bug.
      //
      // COST RULE — a hovercard must never make the page more expensive to load.
      // Renderers are called only on hover, after an intent delay, and are expected
      // to be *pure over state the board has already fetched for its own reasons*.
      // A renderer must not trigger a fetch, and must not widen an existing request
      // to carry fields only it would use. When the data it wants genuinely is not
      // loaded yet, return null (no card) or render what is known — never force a
      // load to populate a card the user may never open. If a card ever does need
      // on-demand data, fetch it from the renderer's own lazy path and show a
      // placeholder first, the way the Live View's System Prompt tab does.

      /**
       * One hovercard's rendered content. Every field is inner HTML that its
       * renderer is responsible for escaping — the engine does no escaping of its
       * own, because renderers need to emit markup (pills, monospace spans, action
       * buttons) rather than plain text.
       * @typedef {object} HoverCardView
       * @property {string} title - Short uppercase eyebrow naming what this card describes, e.g. "Branch worktree".
       * @property {string} body - The card's main content, typically a `<dl class="hc-kv">`.
       * @property {string} [badge] - Optional header-right content, typically a status pill.
       * @property {string} [foot] - Optional action row; its buttons use the normal `data-click` delegation.
       */

      /**
       * Registered hovercard renderers, keyed by the `data-card` value that selects
       * them. Populated by the chunks that own each entity (e.g. 65-reviews.js), so
       * this engine stays free of any knowledge of reviews, squads or cells.
       * @type {{[kind: string]: (ds: DOMStringMap) => (HoverCardView|null)}}
       */
      const HOVER_CARD_RENDERERS = {};

      /**
       * Registers the renderer for one `data-card` kind. A renderer returning null
       * suppresses the card entirely, which is how a renderer declines to describe
       * an entity it has no loaded detail for yet.
       * @param {string} kind - The `data-card` attribute value this renderer answers to.
       * @param {(ds: DOMStringMap) => (HoverCardView|null)} render - Builds the card from the anchor's dataset.
       * @returns {void}
       */
      function registerHoverCard(kind, render) {
        HOVER_CARD_RENDERERS[kind] = render;
      }

      /**
       * Builds one `<dt>/<dd>` pair for a hovercard's key/value list. Values are
       * emitted as-is so callers can pass markup; pass already-escaped text.
       * @param {string} key - Field label, shown muted in the left column.
       * @param {string} value - Field value as inner HTML.
       * @param {string} [cls] - Optional class for the `<dd>`, e.g. "mono".
       * @returns {string}
       */
      function hcRow(key, value, cls) {
        return `<dt>${esc(key)}</dt><dd${cls ? ` class="${cls}"` : ""}>${value}</dd>`;
      }

      /**
       * Wraps rows built by {@link hcRow} in the card's key/value list element.
       * @param {string} rows - Concatenated `<dt>/<dd>` pairs.
       * @returns {string}
       */
      function hcKv(rows) {
        return `<dl class="hc-kv">${rows}</dl>`;
      }

      /**
       * A trailing explanatory paragraph for a hovercard — the "so what" line under
       * the facts, in the same muted register the tooltips use.
       * @param {string} html - Already-escaped inner HTML.
       * @returns {string}
       */
      function hcNote(html) {
        return `<p class="hc-note">${html}</p>`;
      }

      // The engine itself: one card element reused for every anchor, shown after a
      // short hover intent delay and dismissed only once the pointer has left both
      // the anchor and the card.
      /** Wires up the global hovercard engine driven by `data-card="..."` attributes; self-invoking, no exports. */
      (function () {
        const card = document.createElement("div");
        card.id = "board-card";
        card.setAttribute("role", "tooltip");
        document.body.appendChild(card);

        /** @type {HTMLElement|null} */
        let anchor = null;
        /** @type {number} */
        let openTimer = 0;
        /** @type {number} */
        let closeTimer = 0;

        // Long enough that sweeping the pointer across a dense branch list does not
        // strobe cards open, short enough that a deliberate hover feels immediate.
        const OPEN_DELAY_MS = 180;
        // Grace period for crossing the gap between the anchor and the card.
        const CLOSE_DELAY_MS = 180;

        /**
         * Hides the card and forgets its anchor.
         * @returns {void}
         */
        function hide() {
          window.clearTimeout(openTimer);
          window.clearTimeout(closeTimer);
          anchor = null;
          card.classList.remove("show");
        }

        /**
         * Positions the card against its anchor, preferring below-left and flipping
         * on whichever axis would otherwise overflow the viewport.
         * @param {HTMLElement} el - The anchor element.
         * @returns {void}
         */
        function place(el) {
          const a = el.getBoundingClientRect();
          const w = card.offsetWidth, h = card.offsetHeight, gap = 8, pad = 8;
          let left = a.left;
          let top = a.bottom + gap;
          if (left + w > window.innerWidth - pad) left = Math.max(pad, window.innerWidth - w - pad);
          if (top + h > window.innerHeight - pad) top = Math.max(pad, a.top - h - gap);
          card.style.left = `${left}px`;
          card.style.top = `${top}px`;
        }

        /**
         * Renders and shows the card for one anchor, or does nothing when its kind
         * has no registered renderer or that renderer declines.
         * @param {HTMLElement} el - The anchor element carrying `data-card`.
         * @returns {void}
         */
        function show(el) {
          const kind = el.dataset.card ?? "";
          const render = HOVER_CARD_RENDERERS[kind];
          if (!render) return;
          const view = render(el.dataset);
          if (!view) return;
          // The card supersedes any tooltip. A `data-card` value often sits
          // inside a row that carries its own `data-tip` (a branch name inside
          // a selectable branch row, say), so both engines legitimately match
          // different elements and would otherwise paint at once.
          const tip = document.getElementById("board-tip");
          if (tip) tip.classList.remove("show");
          card.innerHTML =
            `<div class="hc-head"><span class="hc-title">${esc(view.title)}</span>` +
            `<span class="hc-sp"></span>${view.badge ?? ""}</div>` +
            `<div class="hc-body">${view.body}</div>` +
            (view.foot ? `<div class="hc-foot">${view.foot}</div>` : "");
          anchor = el;
          card.classList.add("show");
          place(el);
        }

        document.addEventListener("mouseover", (/** @type {MouseEvent} */ e) => {
          const target = /** @type {HTMLElement} */ (e.target);
          if (target.closest && target.closest("#board-card")) {
            // Pointer moved into the card itself: cancel the pending dismissal so
            // its action buttons stay reachable.
            window.clearTimeout(closeTimer);
            return;
          }
          const el = /** @type {HTMLElement|null} */ (target.closest ? target.closest("[data-card]") : null);
          if (!el) return;
          // An anchor can be a whole row, so a control inside it must keep its
          // own meaning: hovering a button shows that button's tooltip, not the
          // row's card covering the thing you were reaching for.
          const control = target.closest ? target.closest("button, a, input, select, textarea, [data-tip]") : null;
          if (control && control !== el && el.contains(control)) return;
          if (el === anchor) { window.clearTimeout(closeTimer); return; }
          window.clearTimeout(openTimer);
          openTimer = window.setTimeout(() => show(el), OPEN_DELAY_MS);
        });

        document.addEventListener("mouseout", (/** @type {MouseEvent} */ e) => {
          const target = /** @type {HTMLElement} */ (e.target);
          const leavingCard = target.closest && target.closest("#board-card");
          const leavingAnchor = target.closest && target.closest("[data-card]");
          if (!leavingCard && !leavingAnchor) return;
          const related = /** @type {HTMLElement|null} */ (e.relatedTarget);
          // Still inside the card, or still on the anchor: not a real departure.
          if (related && related.closest && (related.closest("#board-card") || related.closest("[data-card]") === anchor)) return;
          window.clearTimeout(openTimer);
          window.clearTimeout(closeTimer);
          closeTimer = window.setTimeout(hide, CLOSE_DELAY_MS);
        });

        // A card anchored to an element that has scrolled away would float
        // detached, so any scroll dismisses it outright.
        document.addEventListener("scroll", hide, true);
        document.addEventListener("keydown", (/** @type {KeyboardEvent} */ e) => {
          if (e.key === "Escape") hide();
        });
        // Acting on a card's own button should dismiss it — the card has served its
        // purpose and would otherwise sit over whatever the action just changed.
        card.addEventListener("click", () => { window.setTimeout(hide, 0); });
      })();
