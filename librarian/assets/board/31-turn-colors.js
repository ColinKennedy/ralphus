      // ---------- per-type turn colors (RAL-563) ----------
      // RALPHUS-TURN-COLORS:BEGIN
      /** Page background per theme, mirrored from `--bg` in board.css; the contrast guard measures against these. */
      const TURN_COLOR_BG = { dark: "#0d1117", light: "#ffffff" };
      /** Minimum WCAG contrast ratio a type color must reach against its theme's background. */
      const TURN_COLOR_MIN_CONTRAST = 4.5;
      /** @type {Record<string, string>} well-known base tags pinned to a semantic color variable (defined per theme in board.css). */
      const TURN_PINNED_COLORS = Object.assign(Object.create(null), {
        error: "var(--turn-error)",
        warning: "var(--turn-warn)",
        warn: "var(--turn-warn)",
        thrash: "var(--turn-warn)",
      });
      /** @type {Map<string, string>} memo of `theme|type` -> hex color. */
      const turnColorMemo = new Map();
      /**
       * FNV-1a hash of a string, unsigned 32-bit.
       * @param {string} s
       * @returns {number}
       */
      function turnColorHash(s) {
        let h = 0x811c9dc5;
        for (let i = 0; i < s.length; i++) {
          h ^= s.charCodeAt(i);
          h = Math.imul(h, 0x01000193) >>> 0;
        }
        return h >>> 0;
      }
      /**
       * Converts HSL (h degrees, s/l 0-100) to an sRGB triple (0-255).
       * @param {number} h
       * @param {number} s
       * @param {number} l
       * @returns {[number, number, number]}
       */
      function turnHslToRgb(h, s, l) {
        const sat = s / 100;
        const lig = l / 100;
        const a = sat * Math.min(lig, 1 - lig);
        /**
         * @param {number} n
         * @returns {number}
         */
        const f = (n) => {
          const k = (n + h / 30) % 12;
          return Math.round(255 * (lig - a * Math.max(-1, Math.min(k - 3, 9 - k, 1))));
        };
        return [f(0), f(8), f(4)];
      }
      /**
       * Parses `#rrggbb` into an sRGB triple.
       * @param {string} hex
       * @returns {[number, number, number]}
       */
      function turnHexToRgb(hex) {
        return [parseInt(hex.slice(1, 3), 16), parseInt(hex.slice(3, 5), 16), parseInt(hex.slice(5, 7), 16)];
      }
      /**
       * WCAG contrast ratio between two `#rrggbb` colors.
       * @param {string} a
       * @param {string} b
       * @returns {number}
       */
      function turnContrast(a, b) {
        /**
         * @param {string} hex
         * @returns {number}
         */
        const lum = (hex) => {
          const [r, g, bl] = turnHexToRgb(hex).map((v) => {
            const c = v / 255;
            return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
          });
          return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
        };
        const la = lum(a);
        const lb = lum(b);
        return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
      }
      /**
       * The color a turn/tool type renders in: a pure, deterministic function
       * of the type string and theme. The base segment (`tool` in
       * `tool.Bash`) picks the hue; the remaining segments nudge hue and
       * lightness so a family looks related while each tool stays distinct.
       * Lightness is then pushed away from the background until the color
       * reaches {@link TURN_COLOR_MIN_CONTRAST}. Types in
       * {@link TURN_PINNED_COLORS} (matched case-insensitively on the base
       * segment) skip all of that and return their `var(--turn-*)` reference.
       * @param {string} type - the bracketed type code, e.g. `usage`, `tool.Bash`.
       * @param {string} theme - `"dark"` or `"light"`.
       * @returns {string} `#rrggbb`, or `var(--turn-error)`/`var(--turn-warn)` for a pinned type.
       */
      function typeColor(type, theme) {
        const pinned = TURN_PINNED_COLORS[type.split(".")[0].toLowerCase()];
        if (pinned !== undefined) return pinned;
        const t = theme === "light" ? "light" : "dark";
        const memoKey = `${t}|${type}`;
        const hit = turnColorMemo.get(memoKey);
        if (hit !== undefined) return hit;
        const segs = type.split(".");
        const rest = segs.slice(1).join(".");
        let hue = turnColorHash(segs[0]) % 360;
        let light = t === "dark" ? 72 : 34;
        if (rest !== "") {
          const rh = turnColorHash(rest);
          hue = (hue + 40 + (rh % 80)) % 360;
          light += ((rh >>> 8) % 2 === 0 ? -6 : 6);
        }
        const bg = TURN_COLOR_BG[t];
        const step = t === "dark" ? 2 : -2;
        let hex = "";
        for (let i = 0; i < 60; i++) {
          const [r, g, b] = turnHslToRgb(hue, 70, light);
          hex = "#" + [r, g, b].map((v) => v.toString(16).padStart(2, "0")).join("");
          if (turnContrast(hex, bg) >= TURN_COLOR_MIN_CONTRAST) break;
          light = Math.max(0, Math.min(100, light + step));
        }
        turnColorMemo.set(memoKey, hex);
        return hex;
      }
      /**
       * HTML-escapes the text and wraps every bracket-tagged entry (the tagged
       * line plus its untagged continuation lines) in a span colored by its
       * type. Text before the first tag is left uncolored.
       * @param {string} text
       * @param {string} theme
       * @returns {string}
       */
      function colorizeTurnLines(text, theme) {
        let color = "";
        return text.split("\n").map((line) => {
          const type = parseLiveViewType(line);
          if (type !== null) color = typeColor(type, theme);
          const safe = line.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
          return color && line !== "" ? `<span style="color:${color}">${safe}</span>` : safe;
        }).join("\n");
      }
      // RALPHUS-TURN-COLORS:END
      /**
       * Whether the user's "Color turn types" preference is on (default on).
       * @returns {boolean}
       */
      function turnColorsEnabled() {
        return localStorage.getItem("ralphus-turn-colors") !== "0";
      }
      /**
       * Persists the "Color turn types" preference.
       * @param {boolean} on
       * @returns {void}
       */
      function setTurnColorsEnabled(on) {
        localStorage.setItem("ralphus-turn-colors", on ? "1" : "0");
      }
      /**
       * Renders transcript text as `<pre>` inner HTML, colored per turn type
       * when the preference is on, otherwise just escaped.
       * @param {string} text
       * @returns {string}
       */
      function transcriptHtml(text) {
        if (!turnColorsEnabled()) return esc(text);
        return colorizeTurnLines(text, document.documentElement.dataset.theme === "light" ? "light" : "dark");
      }
