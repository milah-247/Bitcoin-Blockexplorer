/* Block Explorer frontend: vanilla JS, hash routing, no build step.
 * Routes: #/  #/block/<height|hash>  #/tx/<txid>[?block=<hash>]  #/address/<addr>
 */
"use strict";

const app = document.getElementById("app");
const PAGE_BLOCKS = 15;
const PAGE_TXS = 25;

/* ---------------------------------------------------------------- helpers */

const esc = (v) =>
  String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

const fmtInt = (n) => (n == null ? "–" : Number(n).toLocaleString());

/** Amount in BTC with the satoshi value underneath. */
function amount(sats, { sign = false } = {}) {
  if (sats == null) return '<span class="muted">unknown</span>';
  const neg = sats < 0;
  const abs = Math.abs(sats);
  const btc = (Math.floor(abs / 1e8) + "." + String(abs % 1e8).padStart(8, "0"));
  const prefix = neg ? "−" : sign && sats > 0 ? "+" : "";
  const cls = sign ? (neg ? "neg" : sats > 0 ? "pos" : "") : "";
  return `<span class="${cls}">${prefix}${btc} BTC</span><span class="sats">${prefix}${fmtInt(abs)} sat</span>`;
}

function relTime(ts) {
  const s = Math.round(Date.now() / 1000 - ts);
  if (s < 0) return "just now";
  const units = [["year", 31536000], ["day", 86400], ["hour", 3600], ["minute", 60]];
  for (const [u, n] of units) {
    if (s >= n) {
      const v = Math.floor(s / n);
      return `${v} ${u}${v > 1 ? "s" : ""} ago`;
    }
  }
  return `${s} second${s === 1 ? "" : "s"} ago`;
}

/** Relative time with the absolute time on hover (and refreshed periodically). */
function time(ts, { both = false } = {}) {
  if (ts == null) return '<span class="muted">–</span>';
  const abs = new Date(ts * 1000).toLocaleString();
  return both
    ? `<span data-ts="${ts}">${esc(relTime(ts))}</span> <span class="muted small">(${esc(abs)})</span>`
    : `<span data-ts="${ts}" title="${esc(abs)}">${esc(relTime(ts))}</span>`;
}

const short = (h, n = 10) => (h && h.length > 2 * n + 1 ? `${h.slice(0, n)}…${h.slice(-n)}` : h ?? "");

function copyBtn(text) {
  return `<button type="button" class="copy" data-copy="${esc(text)}" aria-label="Copy to clipboard">copy</button>`;
}

const hashLink = (href, h, n) => `<a class="hash" href="${esc(href)}" title="${esc(h)}">${esc(n ? short(h, n) : h)}</a>`;
const blockHref = (id) => `#/block/${encodeURIComponent(id)}`;
const txHref = (txid, block) => `#/tx/${encodeURIComponent(txid)}${block ? `?block=${encodeURIComponent(block)}` : ""}`;
const addrHref = (a) => `#/address/${encodeURIComponent(a)}`;

function toast(msg) {
  const t = document.getElementById("toast");
  t.textContent = msg;
  t.classList.add("show");
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => t.classList.remove("show"), 1400);
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Clipboard API needs a secure context; fall back for plain http on a LAN IP.
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
  toast("Copied");
}

document.addEventListener("click", (e) => {
  const b = e.target.closest("[data-copy]");
  if (b) {
    e.preventDefault();
    copyText(b.dataset.copy);
  }
});

setInterval(() => {
  document.querySelectorAll("[data-ts]").forEach((el) => (el.textContent = relTime(+el.dataset.ts)));
}, 30000);

/* -------------------------------------------------------------------- api */

class ApiError extends Error {
  constructor(status, code, message) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

async function api(path) {
  let res;
  try {
    res = await fetch(path, { headers: { Accept: "application/json" } });
  } catch {
    throw new ApiError(0, "network", "Could not reach the explorer server. Is it running?");
  }
  let body = null;
  try {
    body = await res.json();
  } catch {
    /* non-JSON (e.g. proxy error page) */
  }
  if (!res.ok) {
    const err = body && body.error ? body.error : {};
    throw new ApiError(res.status, err.code || "http_" + res.status, err.message || res.statusText);
  }
  return body;
}

const ERROR_TITLES = {
  0: "Server unreachable",
  400: "That doesn't look right",
  404: "Not found",
  501: "Not available with this node",
  502: "The Bitcoin node had a problem",
  503: "The node is busy",
  504: "The node took too long",
};

const ERROR_HINTS = {
  400: "Check the height, hash, txid or address and try again.",
  404: "It may not exist, or may not be visible to this explorer yet.",
  501: "The RPC provider does not allow the call this feature needs.",
  502: "The upstream RPC endpoint returned an error or could not be reached. Try again in a moment.",
  503: "The provider's rate limit was reached or the index is still building. Wait a few seconds and retry.",
  504: "Large mainnet blocks can be slow the first time. Retrying usually hits the cache.",
};

function errorBox(e, { retry = true } = {}) {
  const status = e instanceof ApiError ? e.status : 500;
  const title = ERROR_TITLES[status] || "Something went wrong";
  const hint = ERROR_HINTS[status] || "";
  return `<div class="error-box" role="alert">
    <h2>${esc(title)}</h2>
    <p>${esc(e.message)}</p>
    ${hint ? `<p class="muted small">${esc(hint)}</p>` : ""}
    ${retry && status !== 400 && status !== 404 ? '<button type="button" data-retry>Retry</button>' : ""}
    <a class="btn" href="#/">Home</a>
  </div>`;
}

const loading = (what = "Loading") => `<div class="loading"><span class="spinner" aria-hidden="true"></span>${esc(what)}…</div>`;

/* ----------------------------------------------------------------- router */

let renderToken = 0;

function parseHash() {
  const raw = location.hash.replace(/^#/, "") || "/";
  const [path, query = ""] = raw.split("?");
  const parts = path.split("/").filter(Boolean).map(decodeURIComponent);
  return { parts, params: new URLSearchParams(query) };
}

async function route() {
  const token = ++renderToken;
  const { parts, params } = parseHash();
  // Lets async renderers bail out when the user navigated away meanwhile.
  const alive = () => token === renderToken;
  window.scrollTo(0, 0);
  try {
    if (parts.length === 0) await renderHome(alive);
    else if (parts[0] === "block" && parts[1]) await renderBlock(parts[1], alive);
    else if (parts[0] === "tx" && parts[1]) await renderTx(parts[1], params.get("block"), alive);
    else if (parts[0] === "address" && parts[1]) await renderAddress(parts[1], alive);
    else throw new ApiError(404, "not_found", "There is no page at this address.");
  } catch (e) {
    if (alive()) {
      app.innerHTML = errorBox(e);
      setTitle("Error");
    }
  }
}

function setTitle(t) {
  document.title = t ? `${t} · Block Explorer` : "Block Explorer";
}

app.addEventListener("click", (e) => {
  if (e.target.closest("[data-retry]")) route();
});
window.addEventListener("hashchange", route);

/* ----------------------------------------------------------------- search */

const searchForm = document.getElementById("search");
searchForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const input = document.getElementById("q");
  const q = input.value.trim();
  if (!q) return;
  const btn = searchForm.querySelector("button");
  btn.disabled = true;
  btn.textContent = "Searching…";
  try {
    const r = await api(`/api/search?q=${encodeURIComponent(q)}`);
    const target = r.type === "block" ? blockHref(r.value) : r.type === "tx" ? txHref(r.value) : addrHref(r.value);
    input.value = "";
    if (location.hash === target) route();
    else location.hash = target;
  } catch (err) {
    renderToken++;
    app.innerHTML = errorBox(err, { retry: false });
  } finally {
    btn.disabled = false;
    btn.textContent = "Search";
  }
});

/* ------------------------------------------------------------- page: home */

async function renderHome(alive) {
  setTitle("");
  app.innerHTML = `
    <div class="stats" id="tip-stats">
      <div class="stat"><div class="label">Chain tip</div><div class="skeleton" style="width:60%;margin-top:6px"></div></div>
      <div class="stat"><div class="label">Latest block</div><div class="skeleton" style="width:80%;margin-top:6px"></div></div>
    </div>
    <section class="card">
      <div class="card-head"><h2>Latest blocks</h2><span class="muted small" id="blocks-note"></span></div>
      <div class="table-wrap">
        <table>
          <thead><tr><th>Height</th><th>Hash</th><th>Mined</th><th class="num">Transactions</th><th class="num hide-sm">Size</th></tr></thead>
          <tbody id="blocks-body"></tbody>
        </table>
      </div>
      <div id="blocks-status">${loading("Loading blocks")}</div>
      <div class="pager" id="blocks-pager" hidden><button type="button" id="more-blocks">Load more</button></div>
    </section>`;

  const tip = await api("/api/tip");
  if (!alive()) return;
  document.getElementById("tip-stats").innerHTML = `
    <div class="stat"><div class="label">Chain tip</div><div class="value"><a href="${blockHref(tip.height)}">${fmtInt(tip.height)}</a></div><div class="hint">chain “${esc(tip.chain)}”</div></div>
    <div class="stat"><div class="label">Latest block hash</div><div class="value hash small">${hashLink(blockHref(tip.hash), tip.hash, 12)}${copyBtn(tip.hash)}</div></div>`;

  let next = null;
  const body = document.getElementById("blocks-body");
  const status = document.getElementById("blocks-status");
  const pager = document.getElementById("blocks-pager");
  const moreBtn = document.getElementById("more-blocks");

  async function load(start) {
    moreBtn.disabled = true;
    status.innerHTML = loading("Loading blocks");
    try {
      const q = start == null ? "" : `&start=${start}`;
      const r = await api(`/api/blocks?limit=${PAGE_BLOCKS}${q}`);
      if (!alive()) return;
      body.insertAdjacentHTML(
        "beforeend",
        r.blocks
          .map(
            (b) => `<tr>
              <td><a href="${blockHref(b.height)}">${fmtInt(b.height)}</a></td>
              <td>${hashLink(blockHref(b.hash), b.hash, 8)}</td>
              <td>${time(b.time)}</td>
              <td class="num">${fmtInt(b.tx_count)}</td>
              <td class="num hide-sm">${b.size ? (b.size / 1e6).toFixed(2) + " MB" : "–"}</td>
            </tr>`,
          )
          .join(""),
      );
      next = r.next_start;
      status.innerHTML = "";
      pager.hidden = next == null;
    } catch (e) {
      if (alive()) status.innerHTML = `<div class="card-body">${errorBox(e)}</div>`;
    } finally {
      moreBtn.disabled = false;
    }
  }
  moreBtn.addEventListener("click", () => load(next));
  await load(null);
}

/* ------------------------------------------------------------ page: block */

async function renderBlock(id, alive) {
  setTitle(`Block ${short(id, 8)}`);
  app.innerHTML = loading("Loading block (large mainnet blocks can take a few seconds the first time)");
  const b = await api(`/api/block/${encodeURIComponent(id)}`);
  if (!alive()) return;
  setTitle(`Block ${fmtInt(b.height)}`);
  const conf = b.confirmations < 0
    ? '<span class="badge pending">stale (not on the best chain)</span>'
    : `<span class="badge ok">${fmtInt(b.confirmations)} confirmation${b.confirmations === 1 ? "" : "s"}</span>`;

  app.innerHTML = `
    <div class="page-head">
      <div class="kicker">Block</div>
      <h1>#${fmtInt(b.height)} ${conf}</h1>
      <div class="hash muted">${esc(b.hash)}${copyBtn(b.hash)}</div>
    </div>
    <div class="nav-links" style="margin-bottom:16px">
      ${b.previous_hash ? `<a class="btn" href="${blockHref(b.previous_hash)}">← Previous block</a>` : ""}
      ${b.next_hash ? `<a class="btn" href="${blockHref(b.next_hash)}">Next block →</a>` : ""}
    </div>
    <div class="stats">
      <div class="stat"><div class="label">Mined</div><div class="value">${time(b.time)}</div><div class="hint">${esc(new Date(b.time * 1000).toLocaleString())}</div></div>
      <div class="stat"><div class="label">Transactions</div><div class="value">${fmtInt(b.tx_count)}</div></div>
      <div class="stat"><div class="label">Total fees</div><div class="value">${amount(b.total_fees_sat)}</div></div>
      <div class="stat"><div class="label">Size / weight</div><div class="value">${b.size != null ? (b.size / 1e6).toFixed(3) + " MB" : "–"}</div><div class="hint">${fmtInt(b.weight)} WU</div></div>
    </div>
    <section class="card">
      <div class="card-head"><h2>Details</h2></div>
      <dl class="kv">
        <dt>Hash</dt><dd class="hash">${esc(b.hash)}${copyBtn(b.hash)}</dd>
        <dt>Height</dt><dd>${fmtInt(b.height)}</dd>
        <dt>Timestamp</dt><dd>${time(b.time, { both: true })}</dd>
        ${b.median_time != null ? `<dt>Median time</dt><dd>${esc(new Date(b.median_time * 1000).toLocaleString())}</dd>` : ""}
        <dt>Size</dt><dd>${fmtInt(b.size)} bytes${b.stripped_size != null ? ` <span class="muted">(${fmtInt(b.stripped_size)} without witness)</span>` : ""}</dd>
        <dt>Weight</dt><dd>${fmtInt(b.weight)} WU</dd>
        <dt>Total fees</dt><dd>${amount(b.total_fees_sat)}<span class="muted small">Coinbase outputs minus the ${esc((b.subsidy_sat / 1e8).toString())} BTC subsidy</span></dd>
        <dt>Difficulty</dt><dd>${b.difficulty != null ? Number(b.difficulty).toLocaleString(undefined, { maximumFractionDigits: 2 }) : "–"}</dd>
        <dt>Merkle root</dt><dd class="hash">${esc(b.merkle_root)}</dd>
        <dt>Bits / nonce</dt><dd class="mono">${esc(b.bits)} / ${esc(b.nonce)}</dd>
        <dt>Version</dt><dd class="mono">0x${Number(b.version >>> 0).toString(16)}</dd>
        <dt>Previous block</dt><dd>${b.previous_hash ? hashLink(blockHref(b.previous_hash), b.previous_hash) : '<span class="muted">none (genesis)</span>'}</dd>
        <dt>Next block</dt><dd>${b.next_hash ? hashLink(blockHref(b.next_hash), b.next_hash) : '<span class="muted">none yet</span>'}</dd>
      </dl>
    </section>
    <section class="card">
      <div class="card-head"><h2>Transactions</h2><span class="muted small" id="txs-count"></span></div>
      <div class="table-wrap"><table>
        <thead><tr><th class="num">#</th><th>Transaction id</th></tr></thead>
        <tbody id="txs-body"></tbody>
      </table></div>
      <div id="txs-status"></div>
      <div class="pager" id="txs-pager" hidden><button type="button" id="more-txs">Load more</button></div>
    </section>`;

  let next = 0;
  const moreBtn = document.getElementById("more-txs");
  async function load(start) {
    const status = document.getElementById("txs-status");
    moreBtn.disabled = true;
    status.innerHTML = loading("Loading transactions");
    try {
      const r = await api(`/api/block/${b.hash}/txs?start=${start}&limit=${PAGE_TXS}`);
      if (!alive()) return;
      document.getElementById("txs-count").textContent = `${fmtInt(Math.min(r.start + r.txids.length, r.total))} of ${fmtInt(r.total)}`;
      document.getElementById("txs-body").insertAdjacentHTML(
        "beforeend",
        r.txids
          .map((t, i) => `<tr><td class="num muted">${r.start + i}</td><td>${hashLink(txHref(t, b.hash), t)}${r.start + i === 0 ? ' <span class="badge">coinbase</span>' : ""}</td></tr>`)
          .join(""),
      );
      next = r.next_start;
      status.innerHTML = "";
      document.getElementById("txs-pager").hidden = next == null;
    } catch (e) {
      if (alive()) status.innerHTML = `<div class="card-body">${errorBox(e)}</div>`;
    } finally {
      moreBtn.disabled = false;
    }
  }
  moreBtn.addEventListener("click", () => load(next));
  await load(0);
}

/* --------------------------------------------------------------- page: tx */

async function renderTx(txid, blockHint, alive) {
  setTitle(`Transaction ${short(txid, 6)}`);
  app.innerHTML = loading("Loading transaction");
  const q = blockHint ? `?block=${encodeURIComponent(blockHint)}` : "";
  const t = await api(`/api/tx/${encodeURIComponent(txid)}${q}`);
  if (!alive()) return;
  const s = t.status;
  const badge = s.confirmed
    ? `<span class="badge ok">${fmtInt(s.confirmations)} confirmation${s.confirmations === 1 ? "" : "s"}</span>`
    : '<span class="badge pending">Unconfirmed (in mempool)</span>';
  const inSum = t.inputs.reduce((a, i) => (i.value_sat == null ? a : a + i.value_sat), 0);
  const outSum = t.outputs.reduce((a, o) => a + o.value_sat, 0);

  const inputs = t.inputs
    .map((i, n) => {
      if (i.coinbase) return `<li><span class="who"><span class="idx">#${n}</span>Coinbase <span class="muted small">(newly created coins + fees)</span></span><span class="amt">${amount(outSum)}</span></li>`;
      const who = i.address
        ? hashLink(addrHref(i.address), i.address)
        : `<span class="muted">${esc(i.script_type || "unknown script")}</span>`;
      return `<li><span class="who"><span class="idx">#${n}</span>${who}
          <div class="small muted">from ${hashLink(txHref(i.txid), i.txid, 8)}:${esc(i.vout)}</div></span>
        <span class="amt">${amount(i.value_sat)}</span></li>`;
    })
    .join("");
  const outputs = t.outputs
    .map((o) => {
      const who = o.address
        ? hashLink(addrHref(o.address), o.address)
        : `<span class="muted">${esc(o.script_type === "nulldata" ? "OP_RETURN data" : o.script_type || "unknown script")}</span>`;
      return `<li><span class="who"><span class="idx">#${esc(o.n)}</span>${who}</span><span class="amt">${amount(o.value_sat)}</span></li>`;
    })
    .join("");

  app.innerHTML = `
    <div class="page-head">
      <div class="kicker">Transaction</div>
      <h1 class="hash" style="font-size:1.05rem">${esc(t.txid)}${copyBtn(t.txid)}</h1>
      <div>${badge} ${t.is_coinbase ? '<span class="badge">coinbase</span>' : ""}</div>
    </div>
    <div class="stats">
      <div class="stat"><div class="label">Fee</div><div class="value">${t.is_coinbase ? '<span class="muted">none (coinbase)</span>' : amount(t.fee_sat)}</div></div>
      <div class="stat"><div class="label">Fee rate</div><div class="value">${t.fee_rate_sat_vb != null ? `${t.fee_rate_sat_vb} sat/vB` : '<span class="muted">–</span>'}</div></div>
      <div class="stat"><div class="label">Total output</div><div class="value">${amount(outSum)}</div></div>
      <div class="stat"><div class="label">${s.confirmed ? "Confirmed" : "Status"}</div><div class="value">${s.confirmed ? time(s.block_time) : "Waiting for a block"}</div></div>
    </div>
    <section class="card">
      <div class="card-head"><h2>Details</h2></div>
      <dl class="kv">
        <dt>Status</dt><dd>${badge}</dd>
        ${s.confirmed ? `<dt>Block</dt><dd><a href="${blockHref(s.block_hash)}">#${fmtInt(s.block_height)}</a> <span class="hash muted small">${esc(short(s.block_hash, 12))}</span></dd>
        <dt>Block time</dt><dd>${time(s.block_time, { both: true })}</dd>` : ""}
        <dt>Size</dt><dd>${fmtInt(t.size)} bytes · ${fmtInt(t.vsize)} vB · ${fmtInt(t.weight)} WU</dd>
        <dt>Inputs total</dt><dd>${t.is_coinbase ? '<span class="muted">–</span>' : amount(t.inputs.some((i) => i.value_sat == null) ? null : inSum)}</dd>
        <dt>Version / locktime</dt><dd class="mono">${esc(t.version)} / ${esc(t.locktime)}</dd>
      </dl>
    </section>
    <div class="io">
      <section class="card"><div class="card-head"><h2>Inputs</h2><span class="muted small">${t.inputs.length}</span></div><ul class="io-list">${inputs}</ul></section>
      <div class="io-arrow" aria-hidden="true">→</div>
      <section class="card"><div class="card-head"><h2>Outputs</h2><span class="muted small">${t.outputs.length}</span></div><ul class="io-list">${outputs}</ul></section>
    </div>`;
}

/* ---------------------------------------------------------- page: address */

async function renderAddress(addr, alive) {
  setTitle(`Address ${short(addr, 6)}`);
  app.innerHTML = loading("Loading address");
  const a = await api(`/api/address/${encodeURIComponent(addr)}`);
  if (!alive()) return;
  const idx = a.index;
  const partial = idx && !idx.complete_history;
  const syncNote = idx && !idx.synced
    ? ` The index is still catching up (block ${fmtInt(idx.indexed_height)} of ${fmtInt(idx.node_tip)}).`
    : "";

  const utxoRows = (a.unspents || [])
    .map((u) => `<tr><td>${hashLink(txHref(u.txid), u.txid, 10)}:${esc(u.vout)}</td><td><a href="${blockHref(u.height)}">${fmtInt(u.height)}</a></td><td class="num">${amount(u.amount_sat)}</td></tr>`)
    .join("");

  app.innerHTML = `
    <div class="page-head">
      <div class="kicker">Address</div>
      <h1 class="hash" style="font-size:1.05rem">${esc(a.address)}${copyBtn(a.address)}</h1>
    </div>
    <div class="notice" role="note">
      <strong>${partial ? "Partial coverage." : idx ? "Full coverage." : "Confirmed UTXOs only."}</strong>
      ${esc(a.note || "")}${esc(syncNote)}
      ${idx ? `<div class="small muted" style="margin-top:4px">Index covers blocks <a href="${blockHref(idx.start_height)}">${fmtInt(idx.start_height)}</a>–${idx.indexed_height != null ? `<a href="${blockHref(idx.indexed_height)}">${fmtInt(idx.indexed_height)}</a>` : "…"}.</div>` : ""}
    </div>
    <div class="stats">
      <div class="stat"><div class="label">Balance${partial ? " (in indexed range)" : ""}</div><div class="value">${amount(a.balance_sat)}</div></div>
      ${a.received_sat != null ? `<div class="stat"><div class="label">Received</div><div class="value">${amount(a.received_sat)}</div></div>` : ""}
      ${a.sent_sat != null ? `<div class="stat"><div class="label">Sent</div><div class="value">${amount(a.sent_sat)}</div></div>` : ""}
      <div class="stat"><div class="label">Transactions</div><div class="value">${a.tx_count != null ? fmtInt(a.tx_count) : "–"}</div><div class="hint">${fmtInt(a.utxo_count)} unspent output${a.utxo_count === 1 ? "" : "s"}</div></div>
    </div>
    ${idx ? `<section class="card">
      <div class="card-head"><h2>Transaction history</h2><span class="muted small">newest first</span></div>
      <div class="table-wrap"><table>
        <thead><tr><th>Transaction</th><th>Block</th><th>Time</th><th class="num">Change</th></tr></thead>
        <tbody id="hist-body"></tbody>
      </table></div>
      <div id="hist-status"></div>
      <div class="pager" id="hist-pager" hidden><button type="button" id="more-hist">Load more</button></div>
    </section>` : ""}
    <section class="card">
      <div class="card-head"><h2>Unspent outputs</h2><span class="muted small">${a.unspents_truncated ? `showing ${a.unspents.length} of ${fmtInt(a.utxo_count)}` : fmtInt(a.utxo_count)}</span></div>
      ${utxoRows
        ? `<div class="table-wrap"><table><thead><tr><th>Output</th><th>Block</th><th class="num">Amount</th></tr></thead><tbody>${utxoRows}</tbody></table></div>`
        : `<div class="card-body muted">No unspent outputs${partial ? " in the indexed range" : ""}.</div>`}
    </section>`;

  if (!idx) return;
  let next = 0;
  const moreBtn = document.getElementById("more-hist");
  async function load(start) {
    const status = document.getElementById("hist-status");
    moreBtn.disabled = true;
    status.innerHTML = loading("Loading history");
    try {
      const r = await api(`/api/address/${encodeURIComponent(a.address)}/txs?start=${start}&limit=${PAGE_TXS}`);
      if (!alive()) return;
      const rows = r.txs
        .map((t) => `<tr>
          <td>${hashLink(txHref(t.txid, t.block_hash), t.txid, 10)}</td>
          <td><a href="${blockHref(t.block_height)}">${fmtInt(t.block_height)}</a></td>
          <td>${time(t.block_time)}</td>
          <td class="num">${amount(t.net_sat, { sign: true })}</td></tr>`)
        .join("");
      document.getElementById("hist-body").insertAdjacentHTML("beforeend", rows);
      status.innerHTML = start === 0 && !r.txs.length ? `<div class="card-body muted">No transactions${partial ? " in the indexed range" : ""}.</div>` : "";
      next = r.next_start;
      document.getElementById("hist-pager").hidden = next == null;
    } catch (e) {
      if (alive()) status.innerHTML = `<div class="card-body">${errorBox(e)}</div>`;
    } finally {
      moreBtn.disabled = false;
    }
  }
  moreBtn.addEventListener("click", () => load(next));
  await load(0);
}

/* ----------------------------------------------------------------- footer */

async function refreshFooter() {
  try {
    const h = await api("/api/health");
    const net = document.getElementById("net");
    net.textContent = h.network === "bitcoin" ? "mainnet" : h.network;
    net.hidden = false;
    document.getElementById("footer-tip").textContent = h.rpc && h.rpc.ok ? `Node tip ${fmtInt(h.rpc.tip)}` : "Node unreachable";
    const i = h.index;
    document.getElementById("footer-index").textContent = i
      ? `Address index: blocks ${fmtInt(i.start_height)}–${i.indexed_height != null ? fmtInt(i.indexed_height) : "…"}${i.synced ? "" : " (syncing)"}`
      : "Address index disabled";
  } catch {
    document.getElementById("footer-tip").textContent = "Server unreachable";
  }
}

refreshFooter();
setInterval(refreshFooter, 30000);
route();
