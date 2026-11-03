"use strict";
/* Lattice Board Console. Vanilla JS, no framework, no external request.
 *
 * Board text is written by agents and is untrusted. Every piece of it reaches the page through
 * textContent or createTextNode only. Nothing here builds markup from a string, and the test
 * suite scans this file for the calls that would. */
(function () {
  var POLL_VISIBLE_MS = 2000;
  var POLL_HIDDEN_MS = 10000;
  var FEED_PAGE = 100;
  var FEED_KEEP = 500;
  var FEED_POLL_MAX = 500;
  var PLAIN_ATTRS = { id: 1, title: 1, type: 1, name: 1, colspan: 1, scope: 1, role: 1, placeholder: 1, "for": 1 };

  var view = document.getElementById("view");
  var statusEl = document.getElementById("status");
  var bannerEl = document.getElementById("banner");
  var screen = null; // the active screen object
  var timer = null;
  var navGen = 0; // bumped on every navigation so late answers from an old screen are dropped

  // ------------------------------------------------------------------ DOM helpers

  function add(parent, kid) {
    if (kid === null || kid === undefined || kid === false) return;
    if (Array.isArray(kid)) {
      for (var i = 0; i < kid.length; i++) add(parent, kid[i]);
    } else if (typeof kid === "string" || typeof kid === "number") {
      parent.appendChild(document.createTextNode(String(kid)));
    } else {
      parent.appendChild(kid);
    }
  }

  // h(tag, props, ...children). props: class, text, on {event: fn}, value/checked/disabled/
  // selected (as properties), href (only "#/..."), and a short list of plain attributes.
  function h(tag, props) {
    var el = document.createElement(tag);
    if (props) {
      var keys = Object.keys(props);
      for (var i = 0; i < keys.length; i++) {
        var k = keys[i];
        var v = props[k];
        if (v === null || v === undefined || v === false) continue;
        if (k === "class") el.className = v;
        else if (k === "text") el.textContent = String(v);
        else if (k === "on") {
          var evs = Object.keys(v);
          for (var j = 0; j < evs.length; j++) el.addEventListener(evs[j], v[evs[j]]);
        } else if (k === "value" || k === "checked" || k === "disabled" || k === "selected" || k === "open") {
          el[k] = v;
        } else if (k === "href") {
          if (typeof v === "string" && v.indexOf("#/") === 0) el.setAttribute("href", v);
        } else if (PLAIN_ATTRS[k] || k.indexOf("aria-") === 0 || k.indexOf("data-") === 0) {
          el.setAttribute(k, String(v));
        }
      }
    }
    for (var n = 2; n < arguments.length; n++) add(el, arguments[n]);
    return el;
  }

  function clear(el) {
    while (el.firstChild) el.removeChild(el.firstChild);
  }

  function show(nodes) {
    clear(view);
    add(view, nodes);
  }

  // ------------------------------------------------------------------ formatting

  function pad(n) {
    return n < 10 ? "0" + n : String(n);
  }

  function fmtTime(epoch) {
    if (typeof epoch !== "number") return "";
    var d = new Date(epoch * 1000);
    return d.getUTCFullYear() + "-" + pad(d.getUTCMonth() + 1) + "-" + pad(d.getUTCDate()) + " " +
      pad(d.getUTCHours()) + ":" + pad(d.getUTCMinutes()) + ":" + pad(d.getUTCSeconds()) + "Z";
  }

  function fmtDur(s) {
    if (typeof s !== "number" || !isFinite(s)) return "n/a";
    s = Math.max(0, Math.round(s));
    if (s < 90) return s + " s";
    var m = Math.round(s / 60);
    if (m < 90) return m + " min";
    var hrs = Math.floor(m / 60);
    if (hrs < 48) return hrs + " h " + pad(m % 60) + " min";
    var days = Math.floor(hrs / 24);
    return days + " d " + (hrs % 24) + " h";
  }

  function fmtAge(s) {
    return fmtDur(s) + " ago";
  }

  function pct(x) {
    if (typeof x !== "number") return "n/a";
    var p = x * 100;
    return (p < 10 && p > 0 ? p.toFixed(1) : String(Math.round(p))) + "%";
  }

  function fmtNum(x) {
    if (typeof x !== "number") return "n/a";
    return Math.abs(x) >= 10 ? String(Math.round(x)) : String(Math.round(x * 10) / 10);
  }

  function fmtValue(x, unit) {
    return unit === "s" ? fmtDur(x) : fmtNum(x);
  }

  function taskHref(id) {
    return "#/task/" + encodeURIComponent(id);
  }

  function taskLink(id, title) {
    return h("a", { href: taskHref(id), title: id }, title ? title : id);
  }

  function badge(text, cls) {
    return h("span", { class: "badge " + cls, text: text });
  }

  function agentLink(name) {
    return h("a", { href: "#/feed?agent=" + encodeURIComponent(name) }, name);
  }

  // ------------------------------------------------------------------ network

  async function api(path, params) {
    var q = new URLSearchParams();
    var keys = Object.keys(params || {});
    for (var i = 0; i < keys.length; i++) {
      var v = params[keys[i]];
      if (v !== undefined && v !== null && v !== "") q.set(keys[i], String(v));
    }
    var qs = q.toString();
    var ctl = new AbortController();
    var to = setTimeout(function () { ctl.abort(); }, 20000);
    try {
      var r = await fetch(path + (qs ? "?" + qs : ""), {
        cache: "no-store", signal: ctl.signal, headers: { Accept: "application/json" }
      });
      var body = null;
      try { body = await r.json(); } catch (e) { body = null; }
      if (!r.ok) throw new Error((body && body.error) || ("HTTP " + r.status));
      return body;
    } finally {
      clearTimeout(to);
    }
  }

  function setStatus(msg) {
    statusEl.textContent = msg || "";
  }

  function applyMeta(meta) {
    if (!meta) return;
    if (meta.demo) {
      bannerEl.hidden = false;
      bannerEl.textContent = meta.banner || "DEMO DATA, not your board";
      if (document.title.indexOf("[DEMO]") !== 0) document.title = "[DEMO] " + document.title;
    } else {
      bannerEl.hidden = true;
    }
  }

  // ------------------------------------------------------------------ NOW

  function countBox(label, n) {
    return h("div", { class: "count" }, h("b", { text: n }), h("span", { class: "muted", text: label }));
  }

  function attnList(title, rule, items, render) {
    if (!items.length) return null;
    var ul = h("ul");
    for (var i = 0; i < items.length && i < 25; i++) ul.appendChild(h("li", null, render(items[i])));
    if (items.length > 25) ul.appendChild(h("li", { class: "muted", text: "and " + (items.length - 25) + " more" }));
    return h("div", null, h("h3", { text: title + " (" + items.length + ")" }), h("div", { class: "rule", text: "Rule: " + rule }), ul);
  }

  var nowScreen = {
    name: "now",
    start: function () { return this.poll(); },
    poll: async function () {
      var gen = navGen;
      var d = await api("/api/now");
      if (gen !== navGen) return;
      applyMeta(d.meta);
      var a = d.attention;
      var r = a.rules;
      var sections = [
        attnList("Stale claims", r.stale, a.stale, function (x) {
          return [taskLink(x.task_id, x.title), " held by ", x.owner || "nobody", ", no event for ", fmtDur(x.idle_s)];
        }),
        attnList("Claimed by more than one agent", r.double_claims, a.double_claims, function (x) {
          return [taskLink(x.task_id, x.title), ": ", x.claimants.join(", ")];
        }),
        attnList("Owner differs from the last claim", r.owner_mismatch, a.owner_mismatch, function (x) {
          return [taskLink(x.task_id, x.title), ": owner ", x.owner, ", last claim by ", x.last_claim_by];
        }),
        attnList("Open for too long", r.open_too_long, a.open_too_long, function (x) {
          return [taskLink(x.task_id, x.title), ", open for ", fmtDur(x.age_s)];
        }),
        attnList("Lease ran out, not reaped yet", r.expired_leases, a.expired_leases || [], function (x) {
          return [taskLink(x.task_id, x.title), " held by ", x.owner || "nobody", ", ", fmtDur(x.overdue_s), " past its lease"];
        }),
        attnList("Dead: out of attempts", r.dead_tasks, a.dead_tasks || [], function (x) {
          return [taskLink(x.task_id, x.title), ", ", x.attempts + "/" + x.max_attempts + " attempts", x.last_error ? ", last error: " + x.last_error : ""];
        }),
        attnList("Blocked for ever", r.blocked_forever, a.blocked_forever || [], function (x) {
          return [taskLink(x.task_id, x.title), " waits for ", x.reasons.join(", ")];
        })
      ].filter(Boolean);
      var total = a.stale.length + a.double_claims.length + a.owner_mismatch.length + a.open_too_long.length +
        (a.expired_leases || []).length + (a.dead_tasks || []).length + (a.blocked_forever || []).length;
      var attn = h("section", { class: "attn" + (total ? " has" : ""), "aria-label": "Needs attention" },
        h("h2", { text: total ? "Needs attention (" + total + ")" : "Needs attention: nothing" }),
        sections.length ? sections : h("p", { class: "muted", text: "No stale claims, double claims, old open tasks, expired leases, dead tasks or tasks blocked for ever under the current rules (stale after " + d.meta.stale_minutes + " min, open for more than " + d.meta.open_hours + " h)." }));

      var rows = d.agents.map(function (g) {
        return h("tr", null,
          h("td", null, agentLink(g.name)),
          h("td", null, badge(g.state, g.state)),
          h("td", null, g.current_task ? taskLink(g.current_task.id, g.current_task.title) : h("span", { class: "muted", text: "none" })),
          h("td", null, g.last_event ? [g.last_event.kind + " ", fmtAge(g.last_event.age_s)] : h("span", { class: "muted", text: "no events" })),
          h("td", { class: "num", text: g.tasks_done }),
          h("td", { class: "num", text: g.events }));
      });
      var table = h("div", { class: "scroll" }, h("table", null,
        h("thead", null, h("tr", null,
          h("th", { scope: "col", text: "Agent" }), h("th", { scope: "col", text: "State" }),
          h("th", { scope: "col", text: "Current task" }), h("th", { scope: "col", text: "Last event" }),
          h("th", { scope: "col", class: "num", text: "Tasks done" }), h("th", { scope: "col", class: "num", text: "Events" }))),
        h("tbody", null, rows)));

      var c = d.counts;
      var note = [];
      if (d.meta.truncated.tasks) note.push("Only the newest 5000 tasks are loaded.");
      if (d.meta.truncated.events) note.push("Only the newest events are loaded; older history is not counted.");
      if (d.meta.truncated.decisions) note.push("Only the newest decisions are loaded.");
      if (d.meta.clock === "client") note.push("The database clock could not be read: lease times are compared with this machine's clock.");
      show([
        h("div", { class: "counts" }, countBox("open", c.open), countBox("claimed", c.claimed), countBox("done", c.done),
          c.dead ? countBox("dead", c.dead) : null, c.cancelled ? countBox("cancelled", c.cancelled) : null,
          c.awaiting_approval ? countBox("awaiting approval", c.awaiting_approval) : null,
          c.other ? countBox("other state", c.other) : null, countBox("agents", d.agents.length),
          h("a", { class: "count linkbox", href: "#/queue" }, h("b", { text: d.queue_totals.runnable }), h("span", { class: "muted", text: "runnable: open the queue" }))),
        attn,
        h("h2", { text: "Agents" }),
        d.agents.length ? table : h("p", { class: "muted", text: "No agents yet: the board has no events and no owners." }),
        h("p", { class: "muted", text: "Agents are every event author and task owner. working: holds a claimed task with an event in the last " + d.meta.stale_minutes + " min. stale: holds claimed tasks and none has. idle: holds none." }),
        note.length ? h("p", { class: "note-line", text: note.join(" ") }) : null
      ]);
    }
  };

  // ------------------------------------------------------------------ TASK

  function eventItem(e) {
    return h("li", null,
      h("div", { class: "meta" }, "#" + e.id + " ", fmtTime(e.created_at), " ", e.author, " ",
        h("span", { class: "badge kind " + e.kind, text: e.kind })),
      e.body ? h("div", { class: "text", text: e.body }) : null);
  }

  function makeTaskScreen(id) {
    return {
      name: "task",
      start: function () { return this.poll(); },
      poll: async function () {
        var gen = navGen;
        var d;
        try {
          d = await api("/api/task", { id: id });
        } catch (e) {
          if (gen !== navGen) return;
          show([h("p", { class: "crumbs" }, h("a", { href: "#/now", text: "Back to now" })),
            h("h2", { text: "Task " + id }), h("p", { class: "note-line", text: "Not found or not readable: " + e.message })]);
          return;
        }
        if (gen !== navGen) return;
        applyMeta(d.meta);
        var t = d.task;
        var crumbs = h("div", { class: "crumbs" }, h("a", { href: "#/now", text: "Now" }));
        d.parents.forEach(function (p) {
          crumbs.appendChild(h("span", { class: "sep", text: "/" }));
          crumbs.appendChild(p.missing ? h("span", { class: "muted", text: p.id + " (no such task)" }) : taskLink(p.id, p.title));
        });
        crumbs.appendChild(h("span", { class: "sep", text: "/" }));
        crumbs.appendChild(h("strong", { text: t.title }));
        var issue = null;
        if (d.parent_issue === "cycle") issue = "The parent chain loops back on itself; showing the part before the loop.";
        else if (d.parent_issue === "missing_parent") issue = "The parent id names no task (an orphan).";
        else if (d.parent_issue === "too_deep") issue = "The parent chain is deeper than 50 levels; showing the nearest 50.";
        var kids = d.children.length ? h("ul", null, d.children.map(function (k) {
          return h("li", null, badge(k.state, k.state), " ", taskLink(k.id, k.title), k.owner ? " (" + k.owner + ")" : "");
        })) : h("p", { class: "muted", text: "No child tasks." });
        var decs = d.decisions.length ? h("ul", { class: "tl" }, d.decisions.map(eventItem)) : h("p", { class: "muted", text: "No decisions." });
        var evs = d.events.length ? h("ol", { class: "tl" }, d.events.map(eventItem)) : h("p", { class: "muted", text: "No events loaded for this task." });
        show([
          crumbs,
          issue ? h("p", { class: "note-line", text: issue }) : null,
          h("h2", null, t.title, " ", badge(t.state, t.state)),
          h("table", null, h("tbody", null,
            h("tr", null, h("th", { scope: "row", text: "Id" }), h("td", { class: "mono", text: t.id })),
            h("tr", null, h("th", { scope: "row", text: "Owner" }), h("td", { text: t.owner || "none" })),
            h("tr", null, h("th", { scope: "row", text: "Priority" }), h("td", { text: t.priority })),
            h("tr", null, h("th", { scope: "row", text: "Created" }), h("td", { text: fmtTime(t.created_at) + " by " + t.created_by })),
            h("tr", null, h("th", { scope: "row", text: "Updated" }), h("td", { text: fmtTime(t.updated_at) })))),
          t.body ? h("div", { class: "panel text", text: t.body }) : null,
          h("h3", { text: "Child tasks" }), kids,
          h("h3", { text: "Events, oldest first" }), evs,
          h("h3", { text: "Decisions" }), decs,
          d.events_truncated ? h("p", { class: "note-line", text: "Showing the newest " + d.events.length + " of " + d.events_total + " events of this task." }) : null,
          d.decisions_truncated ? h("p", { class: "note-line", text: "Showing the newest " + d.decisions.length + " of " + d.decisions_total + " decisions." }) : null,
          d.children_total > d.children.length ? h("p", { class: "note-line", text: "Showing " + d.children.length + " of " + d.children_total + " child tasks." }) : null,
          d.events_complete ? null : h("p", { class: "note-line", text: "Only the newest events are loaded; older events of this task may be missing." })
        ]);
      }
    };
  }

  // ------------------------------------------------------------------ FEED

  function readHashQuery() {
    var hash = location.hash || "";
    var i = hash.indexOf("?");
    var out = {};
    if (i < 0) return out;
    var p = new URLSearchParams(hash.slice(i + 1));
    ["agent", "kind", "task"].forEach(function (k) {
      var v = p.get(k);
      if (v) out[k] = v;
    });
    return out;
  }

  function makeFeedScreen() {
    var st = {
      events: [], pending: [], cursor: null, paused: false, filters: readHashQuery(),
      agents: [], kinds: [], hasMore: false, fresh: {}
    };
    var listEl = null;
    var barEl = null;
    var filtersEl = null;

    function params(extra) {
      var p = { agent: st.filters.agent, kind: st.filters.kind, task: st.filters.task };
      var ks = Object.keys(extra || {});
      for (var i = 0; i < ks.length; i++) p[ks[i]] = extra[ks[i]];
      return p;
    }

    function setFilter(key, value) {
      if (value) st.filters[key] = value; else delete st.filters[key];
      var q = new URLSearchParams();
      Object.keys(st.filters).forEach(function (k) { q.set(k, st.filters[k]); });
      var qs = q.toString();
      history.replaceState(null, "", "#/feed" + (qs ? "?" + qs : ""));
      restart();
    }

    async function restart() {
      var gen = navGen;
      st.events = []; st.pending = []; st.cursor = null; st.fresh = {};
      try {
        var d = await api("/api/feed", params({ limit: FEED_PAGE }));
        if (gen !== navGen) return;
        applyMeta(d.meta);
        st.events = d.events; st.cursor = d.cursor; st.agents = d.agents; st.kinds = d.kinds; st.hasMore = d.has_more;
        draw();
      } catch (e) {
        if (gen === navGen) setStatus("Feed: " + e.message);
      }
    }

    function flush() {
      if (!st.pending.length) return;
      st.pending.forEach(function (e) { st.fresh[e.id] = true; });
      st.events = st.pending.concat(st.events).slice(0, FEED_KEEP);
      st.pending = [];
      drawList();
      drawBar();
    }

    function select(label, options, current, onChange) {
      var sel = h("select", { "aria-label": label, on: { change: function () { onChange(sel.value); } } },
        h("option", { value: "", text: "all" }));
      options.forEach(function (o) {
        var opt = h("option", { value: o, text: o });
        if (o === current) opt.selected = true;
        sel.appendChild(opt);
      });
      if (current && options.indexOf(current) < 0) {
        var extra = h("option", { value: current, text: current });
        extra.selected = true;
        sel.appendChild(extra);
      }
      return h("label", null, label, sel);
    }

    function drawBar() {
      if (!barEl) return;
      clear(barEl);
      var n = st.pending.length;
      if (!n && !st.paused) return;
      var text = st.paused
        ? "Paused" + (n ? ": " + n + " new event" + (n === 1 ? "" : "s") + " waiting. Click to show them." : ". New events still arrive in the background.")
        : n + " new event" + (n === 1 ? "" : "s") + ". Click to show.";
      barEl.appendChild(h("button", {
        type: "button", class: "newbar" + (st.paused ? " paused" : ""), text: text,
        on: { click: function () { flush(); window.scrollTo(0, 0); } }
      }));
    }

    function drawList() {
      if (!listEl) return;
      clear(listEl);
      if (!st.events.length) {
        listEl.appendChild(h("li", { class: "muted", text: "No events match." }));
        return;
      }
      st.events.forEach(function (e) {
        listEl.appendChild(h("li", { class: st.fresh[e.id] ? "fresh" : "" },
          h("span", { class: "when", text: "#" + e.id + " " + fmtTime(e.created_at) }),
          h("span", null, agentLink(e.author)),
          h("span", null, h("span", { class: "badge kind " + e.kind, text: e.kind })),
          h("span", { class: "body" }, taskLink(e.task_id, e.task_title), e.body ? [" ", h("span", { class: "text", text: e.body })] : null)));
      });
      st.fresh = {};
    }

    function draw() {
      filtersEl = h("div", { class: "filters" },
        select("Agent", st.agents, st.filters.agent, function (v) { setFilter("agent", v); }),
        select("Kind", st.kinds, st.filters.kind, function (v) { setFilter("kind", v); }),
        h("label", null, "Task id", h("input", {
          type: "text", value: st.filters.task || "", placeholder: "all", "aria-label": "Task id",
          on: { change: function (ev) { setFilter("task", ev.target.value.trim()); } }
        })),
        h("button", { type: "button", text: st.paused ? "Resume" : "Pause", on: { click: function () {
          st.paused = !st.paused;
          if (!st.paused) flush();
          draw();
        } } }));
      barEl = h("div", { "aria-live": "polite" });
      listEl = h("ol", { class: "feed" });
      var more = st.hasMore ? h("button", { type: "button", text: "Load older events", on: { click: loadOlder } }) : null;
      show([h("h2", { text: "Feed, newest first" }), filtersEl, barEl, listEl, more,
        h("p", { class: "muted", text: "Polls every " + (POLL_VISIBLE_MS / 1000) + " s while this tab is visible, every " + (POLL_HIDDEN_MS / 1000) + " s when hidden. Events are added at the top only when you are at the top of the page and not paused." })]);
      drawList();
      drawBar();
    }

    async function loadOlder() {
      if (!st.events.length) return;
      var gen = navGen;
      try {
        var d = await api("/api/feed", params({ limit: FEED_PAGE, before: st.events[st.events.length - 1].id }));
        if (gen !== navGen) return;
        st.events = st.events.concat(d.events);
        st.hasMore = d.has_more;
        draw();
      } catch (e) {
        setStatus("Feed: " + e.message);
      }
    }

    return {
      name: "feed",
      start: function () { return restart(); },
      poll: async function () {
        if (st.cursor === null) return restart();
        var gen = navGen;
        // An answer can stop short of the newest event (it was too big); ask again from its cursor.
        for (var round = 0; round < 20; round++) {
          var d = await api("/api/feed", params({ after: st.cursor, limit: FEED_POLL_MAX }));
          if (gen !== navGen) return;
          applyMeta(d.meta);
          if (d.reset) return restart();
          st.cursor = d.cursor;
          if (d.events.length) {
            st.pending = d.events.concat(st.pending);
            if (!st.paused && window.scrollY < 8) flush(); else drawBar();
          }
          if (!d.has_more) break;
        }
      }
    };
  }

  // ------------------------------------------------------------------ SCORECARD

  function cellFor(m) {
    if (m.status === "insufficient") {
      return h("div", { class: "cell insufficient" },
        h("div", { class: "big", text: m.message }),
        h("div", { class: "small", text: "n = " + m.n + ", needs " + m.min_n }));
    }
    if (m.kind === "proportion") {
      return h("div", { class: "cell" },
        m.truncated ? h("div", { class: "warnline", text: "window cut: see caveat" }) : null,
        h("div", { class: "big", text: pct(m.value) }),
        h("div", { class: "small", text: "95% CI " + pct(m.lo) + " to " + pct(m.hi) }),
        h("div", { class: "small", text: m.k + " of n = " + m.n }),
        m.warning ? h("div", { class: "warnline", text: m.warning }) : null);
    }
    return h("div", { class: "cell" },
      m.truncated ? h("div", { class: "warnline", text: "window cut: see caveat" }) : null,
      h("div", { class: "big", text: "median " + fmtValue(m.p50, m.unit) }),
      h("div", { class: "small", text: "p10 " + fmtValue(m.p10, m.unit) + ", p90 " + fmtValue(m.p90, m.unit) }),
      h("div", { class: "small", text: "95% CI " + fmtValue(m.lo, m.unit) + " to " + fmtValue(m.hi, m.unit) }),
      h("div", { class: "small", text: "n = " + m.n }));
  }

  function windowFor(kind, now) {
    var day = 86400;
    var len = { "24h": day, "3d": 3 * day, "7d": 7 * day }[kind];
    return len ? { len: len, from: now - len, to: now + 1 } : null;
  }

  function makeScorecardScreen() {
    var q = new URLSearchParams((location.hash.split("?")[1]) || "");
    var st = { a: q.get("a") || "all", b: q.get("b") || "none", agent: q.get("agent") || "" };
    if (["all", "24h", "3d", "7d"].indexOf(st.a) < 0) st.a = "all";
    if (["none", "prev"].indexOf(st.b) < 0) st.b = "none";

    function persist() {
      var p = new URLSearchParams();
      if (st.a !== "all") p.set("a", st.a);
      if (st.b !== "none") p.set("b", st.b);
      if (st.agent) p.set("agent", st.agent);
      var s = p.toString();
      history.replaceState(null, "", "#/scorecard" + (s ? "?" + s : ""));
    }

    function choice(label, options, current, onChange) {
      var sel = h("select", { "aria-label": label, on: { change: function () { onChange(sel.value); } } });
      options.forEach(function (o) {
        var opt = h("option", { value: o[0], text: o[1] });
        if (o[0] === current) opt.selected = true;
        sel.appendChild(opt);
      });
      return h("label", null, label, sel);
    }

    function metricHead(m) {
      return h("th", { scope: "row", class: "metric-name" }, h("details", null,
        h("summary", { title: m.definition, text: m.name }),
        h("div", { class: "def", text: m.definition }),
        m.caveat ? h("div", { class: "def", text: "Caveat: " + m.caveat }) : null,
        m.interval ? h("div", { class: "def", text: "Interval: " + m.interval + ". Shown only when n is at least " + m.min_n + "." }) : null));
    }

    async function load() {
      var gen = navGen;
      var meta = await api("/api/now");
      if (gen !== navGen) return;
      var now = meta.meta.now;
      var wa = windowFor(st.a, now);
      var wb = null;
      if (wa && st.b === "prev") wb = { from: wa.from - wa.len, to: wa.from };
      var d = await api("/api/scorecard", {
        from: wa ? wa.from : "", to: wa ? wa.to : "",
        from2: wb ? wb.from : "", to2: wb ? wb.to : "", agent: st.agent
      });
      if (gen !== navGen) return;
      applyMeta(d.meta);

      var names = Object.keys(d.agents);
      var cols = [d.overall].concat(names.map(function (n) { return d.agents[n]; }));
      var head = h("tr", null, h("th", { scope: "col", text: "Metric" }), cols.map(function (c, i) {
        return h("th", { scope: "col" }, i === 0 ? "All agents" : names[i - 1], h("div", { class: "muted", text: c.tasks_in_cohort + " tasks" }));
      }));
      var body = d.overall.metrics.map(function (m, row) {
        return h("tr", null, metricHead(m), cols.map(function (c) { return h("td", null, cellFor(c.metrics[row])); }));
      });

      var nm = h("div", { class: "panel" }, h("h3", { text: "Not measurable with this schema" }),
        d.not_measurable.map(function (m) {
          return h("details", null, h("summary", { text: m.name + ": " + m.message }),
            h("p", { class: "text", text: "Schema change needed: " + m.schema_change }));
        }));

      var cmp = null;
      if (d.compare) {
        var rows = d.compare.rows.map(function (r) {
          var txt;
          if (r.status !== "ok") txt = r.message + " (A: n = " + r.n_a + ", B: n = " + r.n_b + ")";
          else {
            var f = r.unit === "s" ? fmtDur : (r.unit === "share" ? function (x) { return (x >= 0 ? "+" : "-") + pct(Math.abs(x)); } : fmtNum);
            txt = "A - B = " + (r.unit === "s" ? (r.delta < 0 ? "-" : "+") + fmtDur(Math.abs(r.delta)) : f(r.delta)) +
              ", 95% CI " + (r.unit === "s" ? fmtDur(r.lo) + " to " + fmtDur(r.hi) : f(r.lo) + " to " + f(r.hi)) +
              (r.excludes_zero ? ". The interval excludes zero." : ". The interval includes zero: no clear difference.") +
              " (A: n = " + r.n_a + ", B: n = " + r.n_b + ")";
          }
          return h("tr", null, h("th", { scope: "row", text: r.name }), h("td", { text: txt }));
        });
        cmp = h("div", { class: "panel" }, h("h3", { text: "Window A against window B" }),
          h("p", { class: "muted", text: d.compare.method + " " + d.compare.note }),
          d.compare.warning ? h("p", { class: "note-line", text: d.compare.warning }) : null,
          h("table", null, h("tbody", null, rows)));
      }

      var scoped = [];
      if (st.agent && !names.length) scoped.push(h("p", { class: "note-line", text: "That agent claimed no task in this window." }));

      show([
        h("h2", { text: "Scorecard" }),
        h("div", { class: "filters" },
          choice("Window A", [["all", "all time"], ["24h", "last 24 hours"], ["3d", "last 3 days"], ["7d", "last 7 days"]], st.a, function (v) { st.a = v; if (v === "all") st.b = "none"; persist(); run(); }),
          choice("Compare with B", [["none", "no comparison"], ["prev", "the period just before A"]], st.b, function (v) { st.b = v; if (v === "prev" && st.a === "all") st.a = "7d"; persist(); run(); }),
          choice("Agent", [["", "all agents"]].concat(d.agent_names.map(function (n) { return [n, n]; })), st.agent, function (v) { st.agent = v; persist(); run(); }),
          h("button", { type: "button", text: "Refresh", on: { click: run } })),
        scoped,
        h("p", { class: "muted", text: d.note + " Windows select tasks by creation time. Click a metric name for its definition." }),
        h("div", { class: "scroll" }, h("table", null, h("thead", null, head), h("tbody", null, body))),
        cmp,
        nm,
        h("p", { class: "muted", text: "These numbers describe how the board was used, not whether the work is correct. With few tasks the intervals are wide; below the minimum n no number is shown." })
      ]);
    }

    function run() {
      return load().catch(function (e) { setStatus("Scorecard: " + e.message); });
    }

    return { name: "scorecard", start: function () { return load(); }, poll: null };
  }


  // ------------------------------------------------------------------ QUEUE

  function emptyNote(text) {
    return h("p", { class: "muted", text: text });
  }

  function qTable(headers, rows, render) {
    var head = h("tr", null, headers.map(function (t) { return h("th", { scope: "col", text: t }); }));
    var body = rows.map(function (r) { return h("tr", null, render(r)); });
    return h("div", { class: "scroll" }, h("table", null, h("thead", null, head), h("tbody", null, body)));
  }

  function td(kid) {
    return h("td", null, kid);
  }

  function qSection(title, rule, rows, total, truncated, render, headers) {
    var kids = [h("h2", { text: title + " (" + total + ")" }), h("div", { class: "rule", text: rule })];
    if (!rows.length) kids.push(emptyNote("None."));
    else kids.push(qTable(headers, rows, render));
    if (truncated) kids.push(h("p", { class: "note-line", text: "Only the first " + rows.length + " are shown." }));
    return h("section", { class: "qsec" }, kids);
  }

  var queueScreen = {
    name: "queue",
    start: function () { return this.poll(); },
    poll: async function () {
      var gen = navGen;
      var d = await api("/api/queue");
      if (gen !== navGen) return;
      applyMeta(d.meta);
      var t = d.totals;
      var tr = d.truncated;
      var r = d.rules;
      var notes = [];
      if (!d.leases_available) notes.push("This board has no task_leases table yet: run `board init` (or `board migrate`). Every task is shown with default lease values.");
      if (d.meta.clock === "client") notes.push("The database clock could not be read, so lease times are compared with this machine's clock and can be off by the clock difference.");
      var sections = [
        qSection("Runnable now", r.runnable, d.runnable, t.runnable, tr.runnable, function (x) {
          return [td(taskLink(x.id, x.title)), h("td", { class: "num", text: x.priority }),
            td(fmtDur(x.age_s)), td(x.attempts + "/" + x.max_attempts), td(x.last_error || "")];
        }, ["Task", "Priority", "Open for", "Attempts", "Last error"]),
        qSection("Blocked", r.blocked, d.blocked, t.blocked, tr.blocked, function (x) {
          var kids = x.blockers.map(function (b) {
            var unknown = b.problem === "not_in_window";
            var never = b.problem && !unknown;
            return h("li", null, taskLink(b.id, b.title), " ", badge(b.state || (unknown ? "not in window" : "missing"), never ? "bad" : (b.state || "open")),
              never ? " (never done)" : (unknown ? " (older than the loaded tasks)" : ""));
          });
          return [td(taskLink(x.id, x.title)), td(x.forever ? badge(x.cycle ? "cycle" : "for ever", "bad") : badge("waiting", "open")),
            td(h("ul", { class: "plain" }, kids))];
        }, ["Task", "Outlook", "Waiting for"]),
        qSection("Claimed", r.claimed, d.claimed, t.claimed, tr.claimed, function (x) {
          var lease;
          if (!x.has_lease) lease = h("span", { class: "muted", text: "no lease (claimed before leases)" });
          else if (x.expired) lease = [badge("expired", "bad"), " " + fmtDur(-x.remaining_s) + " ago"];
          else lease = fmtDur(x.remaining_s) + " left";
          return [td(taskLink(x.id, x.title)), td(x.owner || "nobody"), td(lease),
            td(x.heartbeat_age_s === null ? h("span", { class: "muted", text: "none" }) : fmtAge(x.heartbeat_age_s)),
            td(x.attempts + "/" + x.max_attempts)];
        }, ["Task", "Held by", "Lease", "Last heartbeat", "Attempts"]),
        qSection("Retrying", r.retrying, d.retrying, t.retrying, tr.retrying, function (x) {
          return [td(taskLink(x.id, x.title)), td(x.attempts + "/" + x.max_attempts),
            td("in " + fmtDur(x.wait_s) + " (" + fmtTime(x.not_before) + ")"), td(x.last_error || "")];
        }, ["Task", "Attempts", "Next try", "Last error"]),
        qSection("Dead letter", r.dead, d.dead, t.dead, tr.dead, function (x) {
          return [td(taskLink(x.id, x.title)), td(x.attempts + "/" + x.max_attempts), td(x.last_error || h("span", { class: "muted", text: "none recorded" }))];
        }, ["Task", "Attempts", "Last error"]),
        qSection("Awaiting approval", r.awaiting, d.awaiting, t.awaiting, tr.awaiting, function (x) {
          return [td(taskLink(x.id, x.title)), h("td", { class: "num", text: x.priority }), td(fmtDur(x.age_s))];
        }, ["Task", "Priority", "Waiting for"])
      ];
      show([
        h("h2", { text: "Queue" }),
        h("p", { class: "muted", text: "Who can claim what, from the board tables. Times are the database clock. Anyone can run `board approve`: the board does not check who approves." }),
        notes.length ? h("p", { class: "note-line", text: notes.join(" ") }) : null,
        sections
      ]);
    }
  };

  // ------------------------------------------------------------------ EXPERIMENTS

  // Every rate on these screens is printed from a string the server built (a value, its interval and n
  // together). This file never formats a percentage on its own, so a bare percentage cannot appear.

  function hrefExp(name) {
    return "#/experiment/" + encodeURIComponent(name);
  }

  var experimentsScreen = {
    name: "experiments",
    start: function () { return this.poll(); },
    poll: async function () {
      var gen = navGen;
      var d = await api("/api/experiments");
      if (gen !== navGen) return;
      applyMeta(d.meta);
      var rows = d.experiments;
      show([
        h("h2", { text: "Experiments" }),
        h("p", { class: "muted", text: "Comparisons of agent configurations (arms) on the same tasks, made with `board-bench`. Small samples are inconclusive by design: the page says so and always shows n and the interval." }),
        d.note ? h("p", { class: "note-line", text: d.note }) : null,
        d.available && !rows.length ? h("p", { class: "muted", text: "No experiments yet. `board-bench plan SUITE` shows what one would run." }) : null,
        rows.length ? qTable(["Experiment", "Suite", "Repeats", "Attempts planned", "Started", "By"], rows, function (e) {
          return [td(h("a", { href: hrefExp(e.name) }, e.name)), td(e.suite), h("td", { class: "num", text: e.repeats }),
            h("td", { class: "num", text: e.planned }), td(fmtTime(e.created_at)), td(e.created_by)];
        }) : null
      ]);
    }
  };

  function cellMarks(c) {
    var marks = [];
    if (c.flaky) marks.push(h("span", { class: "mark flaky", title: "the grader's reruns disagreed", text: c.flaky + " flaky" }));
    if (c.errors) marks.push(h("span", { class: "mark error", title: "harness errors, left out of every rate", text: c.errors + " error" }));
    if (c.pending) marks.push(h("span", { class: "mark pending", title: "not recorded yet", text: c.pending + " pending" }));
    if (c.tampered) marks.push(h("span", { class: "mark tampered", title: "protected files the agent changed were restored", text: c.tampered + " tampered" }));
    if (c.timeouts) marks.push(h("span", { class: "mark", title: "the agent did not finish; counted as a failure", text: c.timeouts + " timeout" }));
    return marks;
  }

  function gridCell(c) {
    var cls = "gridcell";
    if (!c.n) cls += " empty";
    else if (c.passes === c.n) cls += " allpass";
    else if (c.passes === 0) cls += " nopass";
    else cls += " mixed";
    return h("td", { class: cls },
      c.n ? [h("span", { class: "frac", text: c.passes + "/" + c.n }), " passed"] : h("span", { text: "no counted attempt" }),
      cellMarks(c));
  }

  function fmtMs(ms) {
    return typeof ms === "number" ? fmtDur(ms / 1000) : "n/a";
  }

  function tooNarrow(a) {
    var n = a.naive_ci, c = a.pass_at_1.ci;
    return !!(n && c && (n[1] - n[0]) < (c[1] - c[0]) - 0.001);
  }

  function armTable(rep) {
    var atK = "pass@" + rep.k;
    var powK = "pass^" + rep.k;
    var head = ["Arm", "Pass@1 (task-level interval)", atK, powK, "Duration", "Cost", "Left out or flagged"];
    return qTable(head, rep.arms, function (a) {
      var cost = a.cost.n ? "mean $" + a.cost.mean.toFixed(4) + ", total $" + a.cost.total.toFixed(2) + " (n = " + a.cost.n + " of " + a.attempts + " counted attempts)" : "not reported by the agent";
      var dur = a.duration.n ? "mean " + fmtMs(a.duration.mean) + " (n = " + a.duration.n + ")" : "not reported";
      var flags = [a.errors + " harness error" + (a.errors === 1 ? "" : "s") + " (not counted)", a.pending + " pending",
        a.flaky + " flaky", a.timeouts + " timed out (counted as failures)", a.tampered + " with restored protected files"].join(", ");
      return [td([h("strong", { text: a.arm }), h("div", { class: "muted", text: a.agent })]),
        td([a.pass_at_1.text, tooNarrow(a) ? h("div", { class: "warnline", text: "Counting every attempt as independent would show a narrower, misleading interval." }) : null]),
        td(a.pass_at_k.text), td(a.pass_pow_k.text), td(dur), td(cost), td(flags)];
    });
  }

  function comparisonBlock(c) {
    var t = c.table;
    var cls = "verdict " + (c.verdict.label === "inconclusive" ? "inconclusive" : "better");
    return h("section", { class: "qsec" },
      h("h3", { text: c.b + " compared with " + c.a }),
      h("div", { class: cls },
        h("div", { class: "label", text: c.verdict.label === "inconclusive" ? "Inconclusive" : (c.verdict.label === "b_better" ? c.b + " is better" : c.a + " is better") }),
        h("div", { class: "text", text: c.verdict.text })),
      h("p", { class: "muted", text: "Paired on " + c.paired_tasks + " tasks both arms have counted attempts for. " + c.rule_text.charAt(0).toUpperCase() + c.rule_text.slice(1) + "." }),
      h("table", null, h("tbody", null,
        h("tr", null, h("th", { scope: "row", text: "Solved by both" }), h("td", { class: "num", text: t.both })),
        h("tr", null, h("th", { scope: "row", text: "Solved only by " + c.a }), h("td", { class: "num", text: t.a_only })),
        h("tr", null, h("th", { scope: "row", text: "Solved only by " + c.b }), h("td", { class: "num", text: t.b_only })),
        h("tr", null, h("th", { scope: "row", text: "Solved by neither" }), h("td", { class: "num", text: t.neither })),
        h("tr", null, h("th", { scope: "row", text: "Exact McNemar p" }),
          h("td", { text: fmtP(c.mcnemar_p) + (c.comparisons > 1 ? " (" + fmtP(c.mcnemar_p_adjusted) + " adjusted for " + c.comparisons + " comparisons)" : "") })))));
  }

  function fmtP(p) {
    if (typeof p !== "number") return "n/a";
    return p < 0.001 ? "below 0.001" : p.toFixed(3);
  }

  function makeExperimentScreen(name) {
    return {
      name: "experiment",
      start: function () { return this.poll(); },
      poll: async function () {
        var gen = navGen;
        var rep;
        try {
          rep = await api("/api/experiment", { name: name });
        } catch (e) {
          if (gen !== navGen) return;
          show([h("p", { class: "crumbs" }, h("a", { href: "#/experiments", text: "Experiments" })),
            h("h2", { text: "Experiment " + name }), h("p", { class: "note-line", text: "Not found or not readable: " + e.message })]);
          return;
        }
        if (gen !== navGen) return;
        applyMeta(rep.meta);
        var e = rep.experiment;
        var recorded = rep.arms.reduce(function (n, a) { return n + a.recorded; }, 0);
        var head = h("table", null, h("tbody", null,
          h("tr", null, h("th", { scope: "row", text: "Suite" }), td(e.suite)),
          h("tr", null, h("th", { scope: "row", text: "Repeats" }), td(String(e.repeats))),
          h("tr", null, h("th", { scope: "row", text: "Attempts" }), td(recorded + " recorded of " + e.planned + " planned, " + rep.pending + " not recorded yet")),
          h("tr", null, h("th", { scope: "row", text: "Order seed" }), td(e.seed + " (arms alternate inside each task and repeat; the order is reproducible from the seed)")),
          h("tr", null, h("th", { scope: "row", text: "Started" }), td(fmtTime(e.created_at) + " by " + e.created_by)),
          h("tr", null, h("th", { scope: "row", text: "Harness" }), td(e.harness))));
        var rules = h("details", null, h("summary", { text: "How the numbers are made" }),
          Object.keys(rep.rules).map(function (k) { return h("p", { class: "text" }, h("strong", { text: k.replace(/_/g, " ") + ": " }), rep.rules[k]); }));
        var grid = qTable(["Task"].concat(rep.arms.map(function (a) { return a.arm; })), rep.tasks, function (t) {
          return [h("td", { class: "mono", text: t })].concat(rep.arms.map(function (a) { return gridCell(rep.grid[t][a.arm]); }));
        });
        var errs = rep.errors.length ? qTable(["Board task", "Arm", "Task", "Repeat", "What went wrong"], rep.errors, function (x) {
          return [h("td", { class: "mono" }, taskLink(x.board_task_id)), td(x.arm), td(x.task), h("td", { class: "num", text: x.repeat }), td(x.summary)];
        }) : h("p", { class: "muted", text: "None." });
        show([
          h("div", { class: "crumbs" }, h("a", { href: "#/experiments", text: "Experiments" }), h("span", { class: "sep", text: "/" }), h("strong", { text: e.name })),
          h("h2", { text: e.name }),
          head,
          rep.notes.map(function (n) { return h("p", { class: "note-line", text: n }); }),
          h("h2", { text: "Arms" }),
          h("p", { class: "muted", text: "Every rate is shown with its n and a 95% interval over tasks. Harness errors and attempts not recorded yet are in no rate." }),
          rep.arms.length ? armTable(rep) : h("p", { class: "muted", text: "No arms registered." }),
          h("h2", { text: "Comparison of arms" }),
          rep.comparisons.length ? rep.comparisons.map(comparisonBlock) : h("p", { class: "muted", text: "Only one arm: there is nothing to compare it with." }),
          h("p", { class: "muted", text: "A result is called inconclusive unless the interval of the paired difference excludes zero, the exact McNemar test agrees and at least " + rep.min_tasks + " tasks are paired. Inconclusive is a result, not a failure of the experiment." }),
          h("h2", { text: "Per task and arm" }),
          rep.tasks.length ? grid : h("p", { class: "muted", text: "No tasks." }),
          h("h2", { text: "Harness errors (" + rep.errors.length + ")" }),
          h("p", { class: "muted", text: "The harness failed here, not the agent: a fixture that did not match its digest, a grader that could not run, a full disk. These attempts are in no rate." }),
          errs,
          rules
        ]);
      }
    };
  }

  // ------------------------------------------------------------------ routing and polling

  function parseRoute() {
    var hash = location.hash || "#/now";
    var path = hash.split("?")[0];
    var me = /^#\/experiment\/(.+)$/.exec(path);
    if (me) {
      try { return { name: "experiment", id: decodeURIComponent(me[1]) }; } catch (e) { return { name: "experiments" }; }
    }
    if (path === "#/experiments") return { name: "experiments" };
    var m = /^#\/task\/(.+)$/.exec(path);
    if (m) {
      try { return { name: "task", id: decodeURIComponent(m[1]) }; } catch (e) { return { name: "now" }; }
    }
    if (path === "#/feed") return { name: "feed" };
    if (path === "#/scorecard") return { name: "scorecard" };
    if (path === "#/queue") return { name: "queue" };
    return { name: "now" };
  }

  function schedule() {
    clearTimeout(timer);
    if (!screen || !screen.poll) return;
    timer = setTimeout(tick, document.hidden ? POLL_HIDDEN_MS : POLL_VISIBLE_MS);
  }

  async function tick() {
    var s = screen;
    try {
      if (s && s.poll) await s.poll();
      if (s === screen) setStatus("");
    } catch (e) {
      if (s === screen) setStatus("Cannot read the board: " + e.message + ". Retrying.");
    }
    schedule();
  }

  async function navigate() {
    navGen += 1;
    clearTimeout(timer);
    setStatus("");
    var r = parseRoute();
    var links = document.querySelectorAll("nav a");
    for (var i = 0; i < links.length; i++) {
      var on = links[i].getAttribute("data-nav") === (r.name === "task" ? "now" : (r.name === "experiment" ? "experiments" : r.name));
      links[i].className = on ? "current" : "";
    }
    if (r.name === "task") screen = makeTaskScreen(r.id);
    else if (r.name === "feed") screen = makeFeedScreen();
    else if (r.name === "scorecard") screen = makeScorecardScreen();
    else if (r.name === "queue") screen = queueScreen;
    else if (r.name === "experiments") screen = experimentsScreen;
    else if (r.name === "experiment") screen = makeExperimentScreen(r.id);
    else screen = nowScreen;
    var s = screen;
    try {
      await s.start();
    } catch (e) {
      if (s === screen) setStatus("Cannot read the board: " + e.message + ". Retrying.");
    }
    if (s === screen) schedule();
  }

  window.addEventListener("hashchange", navigate);
  document.addEventListener("visibilitychange", function () {
    if (!document.hidden && screen && screen.poll) {
      clearTimeout(timer);
      tick();
    } else {
      schedule();
    }
  });
  navigate();
})();
