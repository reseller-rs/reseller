'use strict';

/* Boot state ----------------------------------------------------------------- */
const main = document.querySelector('#main');
// Operator branding rendered into the page as inert JSON (see theme.rs).
const brand = (() => {
  try {
    return JSON.parse(document.querySelector('#branding')?.textContent || '{}');
  } catch {
    return {};
  }
})();
const brandName = brand.name || 'Reseller';
const admin = location.pathname.startsWith('/admin');
const workspace = location.pathname.startsWith('/workspace');
const docsPage = location.pathname.startsWith('/docs');
const oauthPage = location.pathname.startsWith('/oauth/authorize');
const CUSTOMER_STORE = 'reseller.customer';
const store = admin ? 'reseller.admin' : CUSTOMER_STORE;
// The docs page reads the workspace session (same tab) to list the caller's models.
let token = docsPage || oauthPage ? '' : sessionStorage.getItem(store) || '';
let tab = 'overview';

// The operator-issued workspace link carries the dashboard token as `#token=...`.
// Move it into sessionStorage and strip it from the URL right away so it does not
// linger in history, screenshots or referrers.
if (workspace) {
  const fragment = new URLSearchParams(location.hash.slice(1));
  if (fragment.has('token')) {
    const fromLink = fragment.get('token').trim();
    if (fromLink) {
      token = fromLink;
      sessionStorage.setItem(store, token);
    }
    history.replaceState(null, '', location.pathname + location.search);
  }
}

/* Formatting ----------------------------------------------------------------- */
// Every server- or user-supplied value interpolated into an HTML string must go
// through esc() (text and quoted attribute values). Never build `style=` or
// `on*=` attributes: the CSP forbids them.
const HTML_ESCAPES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
const esc = v => String(v ?? '').replace(/[&<>"']/g, c => HTML_ESCAPES[c]);
const usdFormat = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', minimumFractionDigits: 2, maximumFractionDigits: 6 });
const usd = v => usdFormat.format(Number(v || 0));
// Money arrives as decimal strings such as "0" or "0.000000".
const isPositive = v => Number(v) > 0;
const DAY = 86400;
const nowSeconds = () => Date.now() / 1000;
const toUnix = local => local ? Math.floor(new Date(local).getTime() / 1000) : null;
const date = v => v ? new Date(v * 1000).toLocaleString(undefined, { hour12: false, dateStyle: 'medium', timeStyle: 'short' }) : 'Never';
// Usage days are UTC midnights. Format them in UTC so the label matches the bucket.
const day = (v, opts = { month: 'short', day: 'numeric' }) => new Date(Number(v) * 1000).toLocaleDateString(undefined, { timeZone: 'UTC', ...opts });
const relativeFormat = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });
function ago(v) {
  if (!v) return 'Never';
  const diff = v - nowSeconds();
  const abs = Math.abs(diff);
  const [unit, size] = abs < 60 ? ['second', 1] : abs < 3600 ? ['minute', 60] : abs < DAY ? ['hour', 3600] : abs < 30 * DAY ? ['day', DAY] : abs < 365 * DAY ? ['month', 30 * DAY] : ['year', 365 * DAY];
  return relativeFormat.format(Math.round(diff / size), unit);
}
const when = v => v ? `<time datetime="${esc(new Date(v * 1000).toISOString())}" title="${esc(date(v))}">${esc(ago(v))}</time>` : '<span class="muted">Never</span>';
const localDateTime = v => {
  if (!v) return '';
  const d = new Date(Number(v) * 1000);
  d.setMinutes(d.getMinutes() - d.getTimezoneOffset());
  return d.toISOString().slice(0, 16);
};
const count = v => Number(v || 0).toLocaleString();
const plural = (n, word) => `${count(n)} ${word}${Number(n) === 1 ? '' : 's'}`;
const bytes = n => {
  n = Number(n || 0);
  if (n < 1024) return `${count(n)} B`;
  const units = ['KB', 'MB', 'GB'];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < units.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${units[i]}`;
};
const ms = v => {
  const n = Number(v || 0);
  return n < 1000 ? `${count(n)} ms` : `${(n / 1000).toFixed(n < 10_000 ? 2 : 1)} s`;
};
const shortId = id => id ? `${String(id).slice(0, 8)}...` : '-';
const percent = (part, whole) => whole > 0 ? `${((part / whole) * 100).toFixed(1)}%` : '-';
/* Plan maths ---------------------------------------------------------------------
   Everything shown about a plan is derived from the API values (price, credit,
   duration), so operators can change the catalog without touching this file. */
const MONTH_DAYS = 30.4;
// Whole dollars drop the cents ($12), other amounts keep two ($16.67).
const usdShort = n => {
  const r = Math.round(Number(n) * 100) / 100;
  return `$${Number.isInteger(r) ? r.toLocaleString() : r.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
};
function planInfo(p) {
  const price = Number(p.price_usd), credit = Number(p.credit_usd || 0);
  const months = Math.max(1, Math.round(Number(p.duration_days) / MONTH_DAYS));
  return {
    price, credit, months, days: Number(p.duration_days),
    perMonth: price / months,
    creditPerMonth: credit / months,
    bonus: credit > price ? credit - price : 0,
    ratio: price > 0 ? credit / price : 0,
  };
}
const billedLine = i => i.price <= 0 ? 'No charge' : `${usdShort(i.price)} one-time · ${i.months === 1 ? '1 month' : `${i.months} months`}`;
const perMonthLabel = i => i.price <= 0 ? 'Free' : usdShort(i.perMonth);
// Best value: most credit per dollar paid, ties go to the longest plan. Shown only
// when plans actually differ. "Most flexible" marks the one-month plan.
function planBadges(plans) {
  const badges = new Map();
  if (plans.length < 2) return badges;
  const key = p => Math.round(planInfo(p).ratio * 1e6);
  const ranked = plans.slice().sort((a, b) => key(b) - key(a) || b.duration_days - a.duration_days);
  if (key(ranked[0]) > key(ranked[ranked.length - 1])) badges.set(ranked[0].code, 'Best value');
  const monthly = plans.find(p => planInfo(p).months === 1 && !badges.has(p.code));
  if (monthly) badges.set(monthly.code, 'Most flexible');
  return badges;
}
const bestCode = plans => [...planBadges(plans)].find(([, label]) => label === 'Best value')?.[0];
const TONES = new Map(Object.entries({ active: 'ok', paid: 'ok', available: 'ok', completed: 'ok', confirmed: 'ok', blocked: 'warn', suspended: 'warn', expired: 'warn', late: 'warn', pending: 'info', redeemed: 'info', seen: 'info', revoked: 'danger', failed: 'danger', cancelled: 'danger', 'used up': 'muted' }));
const METHOD_LABELS = { stripe: 'Card', crypto: 'Crypto' };
const methodLabel = m => METHOD_LABELS[m] || m;
const KIND_LABELS = { llm: 'Text', tts: 'Speech', stt: 'Transcription', image: 'Image' };
const kindLabel = k => KIND_LABELS[k] || k || '-';
const SOURCE_LABELS = { free: 'Trial credit', purchase: 'Purchase', admin: 'Operator grant', refund: 'Refund', subscription: 'Plan credit' };
const sourceLabel = s => SOURCE_LABELS[s] || s;
const pill = (value, tone) => `<span class="pill tone-${tone || TONES.get(String(value).toLowerCase()) || 'muted'}">${esc(value)}</span>`;
const requestPill = r => !r.finished ? pill('In progress', 'info') : r.status >= 400 ? pill(r.status, 'danger') : pill(r.status, 'ok');
const lotState = c => c.expires_at && c.expires_at < nowSeconds() ? 'expired' : isPositive(c.remaining_usd) ? 'active' : 'used up';
const KEY_NAMES = [
  'My application', 'Production', 'Staging', 'Development', 'Mobile app',
  'Backend service', 'Data pipeline', 'Chat assistant', 'Automation', 'Playground',
  'Local tests', 'CI pipeline', 'Agent runtime', 'Voice demo', 'Translation service',
  'Analytics', 'Internal tools', 'Sandbox', 'Batch jobs', 'Scheduled tasks',
];
const suggestedName = () => KEY_NAMES[Math.floor(Math.random() * KEY_NAMES.length)];

/* Entity links ----------------------------------------------------------------
   Rendered as buttons and handled by one document-level listener (see the end
   of this file), so they work in views and dialogs alike. */
const accountLabel = (id, contact) => contact || shortId(id);
function accountLink(id, contact) {
  if (!id) return '<span class="muted">-</span>';
  if (!admin) return esc(accountLabel(id, contact));
  return `<button type="button" class="link" data-open-account="${esc(id)}" title="Open account ${esc(id)}">${esc(accountLabel(id, contact))}</button>`;
}
function keyLink(id, name, prefix) {
  if (!id) return '<span class="muted">-</span>';
  const label = name || (prefix ? `${prefix}...` : shortId(id));
  return `<button type="button" class="link" data-open-key="${esc(id)}" title="Open key details">${esc(label)}</button>`;
}
const goButton = (label, name, params = {}, cls = 'ghost small') =>
  `<button type="button" class="${cls}" data-go="${esc(name)}" data-go-params="${esc(new URLSearchParams(params))}">${esc(label)}</button>`;
const copyable = (value, label, display = value) =>
  `<button type="button" class="copy-value" data-copy-value="${esc(value)}" data-copy-label="${esc(label)}" title="Copy ${esc(label.toLowerCase())}"><code>${esc(display)}</code></button>`;

/* Session ---------------------------------------------------------------------- */

function signOut() {
  // Stop in-flight work and drop cached data so nothing from this session leaks
  // into the next one or renders into a view that no longer exists.
  loadController?.abort();
  generation++;
  window.onhashchange = null;
  document.querySelectorAll('dialog').forEach(d => d.close());
  sessionStorage.removeItem(store);
  token = '';
  tab = 'overview';
  requestIndex.clear();
  for (const [name, state] of Object.entries(listStates)) Object.assign(state, LIST_DEFAULTS[name]);
  history.replaceState(null, '', location.pathname);
  renderHeader();
  if (workspace || admin) login();
}

function renderHeader() {
  const el = document.querySelector('#header-actions');
  if (!el) return;
  const docs = `<a class="nav-link${docsPage ? ' current' : ''}" href="/docs/"${docsPage ? ' aria-current="page"' : ''}>Docs</a>`;
  if (oauthPage) {
    el.innerHTML = docs;
  } else if (token && (workspace || admin)) {
    el.innerHTML = `${docs}<button type="button" id="header-logout" class="ghost">Sign out</button>`;
    el.querySelector('#header-logout').onclick = signOut;
  } else {
    el.innerHTML = `${docs}<a class="button" href="/workspace/">Workspace</a>`;
  }
}

let noticeTimer;
function notice(message, error = false) {
  const el = document.querySelector('#notice');
  el.textContent = message;
  el.classList.toggle('error', error);
  // Re-promote into the top layer so the toast always sits above open dialogs.
  if (el.matches(':popover-open')) el.hidePopover();
  el.showPopover();
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => el.hidePopover(), 7000);
}
const copyText = async (text, label) => {
  try {
    await navigator.clipboard.writeText(text);
    notice(`${label} copied`);
    return true;
  } catch {
    notice('Copy failed. Select the value manually.', true);
    return false;
  }
};
function flash(el, duration = 1400) {
  if (!el) return;
  el.classList.add('copied');
  clearTimeout(el._flashTimer);
  el._flashTimer = setTimeout(() => el.classList.remove('copied'), duration);
}
function flashCopied(button) {
  if (!button) return;
  if (!button.dataset.label) button.dataset.label = button.textContent;
  button.classList.add('copied');
  button.textContent = 'Copied';
  clearTimeout(button._flashTimer);
  button._flashTimer = setTimeout(() => {
    button.classList.remove('copied');
    button.textContent = button.dataset.label;
  }, 1400);
}

const ERROR_HINTS = {
  rotate_cooldown: 'This key was created or rotated too recently to rotate again.',
  key_creation_daily_limit: 'Daily limit for creating or rotating keys reached.',
  pow_required: 'Human verification is required. Please try again.',
  pow_invalid: 'Human verification failed. Please try again.',
  pow_replayed: 'That verification was already used. Please try again.',
  signup_disabled: 'Self-service signup is currently disabled.',
};
function apiError(r, v) {
  const code = v?.error?.code || v?.error?.type || '';
  let message = v?.error?.message || ERROR_HINTS[code] || `Request failed (${r.status})`;
  const wait = Number.parseInt(r.headers.get('retry-after'), 10);
  if (r.status === 429) {
    if (!v?.error?.message && !ERROR_HINTS[code]) message = 'Too many requests. Please slow down.';
    if (wait > 0) message = `${message.replace(/[.!]?$/, '.')} Try again in ${wait >= 120 ? `${Math.ceil(wait / 60)} minutes` : `${wait} second${wait === 1 ? '' : 's'}`}.`;
  }
  return Object.assign(new Error(message), { status: r.status, code, retryAfter: wait > 0 ? wait : null, redirect: typeof v?.redirect === 'string' ? v.redirect : '' });
}

/* Proof of work (ALTCHA v1): find n with sha256(salt + n) == challenge. Solved on
   the main thread in async batches (CSP forbids blob workers), yielding to the UI. */
const hex = buf => [...new Uint8Array(buf)].map(b => b.toString(16).padStart(2, '0')).join('');
async function solveChallenge(c, onProgress, signal) {
  if (!globalThis.crypto?.subtle) throw new Error('Human verification needs a secure (HTTPS) connection.');
  const enc = new TextEncoder();
  const BATCH = 250;
  for (let start = 0; start <= c.maxnumber; start += BATCH) {
    if (signal?.aborted) throw new Error('Verification cancelled.');
    const end = Math.min(start + BATCH - 1, c.maxnumber);
    const digests = [];
    for (let n = start; n <= end; n++) digests.push(crypto.subtle.digest('SHA-256', enc.encode(c.salt + n)));
    const results = await Promise.all(digests);
    for (let i = 0; i < results.length; i++) if (hex(results[i]) === c.challenge) return start + i;
    onProgress?.(Math.min(1, (end + 1) / (c.maxnumber + 1)));
    await new Promise(r => setTimeout(r));
  }
  throw new Error('Could not complete human verification. Please try again.');
}
async function signup(fields, onProgress, signal) {
  const c = await api('/v1/signup/challenge');
  let altcha;
  if (c.required !== false) {
    const number = await solveChallenge(c, onProgress, signal);
    altcha = btoa(JSON.stringify({ algorithm: c.algorithm, challenge: c.challenge, number, salt: c.salt, signature: c.signature }));
  }
  return api('/v1/signup', 'POST', { ...fields, ...(altcha ? { altcha } : {}) });
}

async function api(path, method = 'GET', data, options = {}) {
  const headers = {};
  const credential = options.token ?? token;
  if (credential) headers[admin ? 'x-admin-token' : 'authorization'] = admin ? credential : `Bearer ${credential}`;
  if (data !== undefined) headers['content-type'] = 'application/json';
  const r = await fetch(path, { method, headers, body: data === undefined ? undefined : JSON.stringify(data), signal: options.signal });
  // Proxies and crashes can return non-JSON bodies. Surface the status instead of a parse error.
  const v = await r.json().catch(() => null);
  if (!r.ok) throw apiError(r, v);
  if (v === null) throw new Error(`Unexpected response from server (${r.status})`);
  return v;
}
const endpoint = name => admin ? `/admin/api/${name}` : `/v1/${name}`;
const get = (path, signal) => api(path, 'GET', undefined, { signal });

/* Building blocks -------------------------------------------------------------- */

// columns: [label, render, className?]. Cells carry data-label so narrow screens
// can show each row as a labelled card.
function table(columns, rows, { group, empty } = {}) {
  if (!rows.length) return emptyState(...(empty || ['Nothing here yet', 'Activity will appear here as it happens.']));
  const row = r => `<tr>${columns.map(([label, render, cls]) => `<td data-label="${esc(label)}"${cls ? ` class="${cls}"` : ''}>${render(r)}</td>`).join('')}</tr>`;
  let body = `<tbody>${rows.map(row).join('')}</tbody>`;
  if (group?.value) {
    const buckets = new Map();
    rows.forEach(r => {
      const value = group.value(r) || 'Unassigned';
      if (!buckets.has(value)) buckets.set(value, []);
      buckets.get(value).push(r);
    });
    body = [...buckets].map(([label, items]) => `<tbody><tr class="group-row"><th colspan="${columns.length}" scope="rowgroup">${esc(label)} <span>${count(items.length)}</span></th></tr>${items.map(row).join('')}</tbody>`).join('');
  }
  return `<div class="table-wrap stack"><table><thead><tr>${columns.map(([label, , cls]) => `<th scope="col"${cls ? ` class="${cls}"` : ''}>${esc(label)}</th>`).join('')}</tr></thead>${body}</table></div>`;
}
function metric(label, value, hint, { tone, html } = {}) {
  return `<div class="card metric-card${tone ? ` tone-${tone}` : ''}"><div class="label">${esc(label)}</div><div class="metric">${html ? value : esc(value)}</div>${hint ? `<div class="hint">${hint}</div>` : ''}</div>`;
}
// A compact label/value list. Values are HTML; escape them at the call site.
const facts = items => `<dl class="facts">${items.filter(Boolean).map(([label, value]) => `<div><dt>${esc(label)}</dt><dd>${value}</dd></div>`).join('')}</dl>`;
function emptyState(title, text) {
  return `<div class="empty"><strong>${esc(title)}</strong><span>${esc(text)}</span></div>`;
}
function loading(text = 'Loading...') {
  return `<div class="loading"><span class="spinner" aria-hidden="true"></span>${esc(text)}</div>`;
}

/* Skeletons ---------------------------------------------------------------------
   Placeholder shapes that mirror the real layout (same cards, grids, and row
   heights) so content replaces them without layout shift. They are inert for
   assistive tech apart from one polite "Loading" status; the busy region carries
   aria-busy while a request is in flight. */
const skel = (cls = 'skel-line', n = 1) => `<span class="skel ${cls}"></span>`.repeat(n);
const skeleton = (inner, label = 'Loading', cls = 'skeleton-body') =>
  `<div class="skeleton" role="status"><span class="sr-only">${esc(label)}...</span><div class="${cls}" aria-hidden="true">${inner}</div></div>`;
const skHead = (actions = false) => `<div class="page-head"><div>${skel('skel-title')}${skel('skel-line md')}</div>${actions ? skel('skel-btn') : ''}</div>`;
const skStats = n => `<div class="stats">${Array.from({ length: n }, () => `<div class="card metric-card">${skel('skel-line sm')}${skel('skel-metric')}${skel('skel-line md')}</div>`).join('')}</div>`;
const skBanner = () => `<div class="banner"><div>${skel('skel-line sm')}${skel('skel-metric')}${skel('skel-line md')}</div><div class="actions">${skel('skel-btn', 2)}</div></div>`;
const skChart = () => `<div class="card"><div class="card-head">${skel('skel-title')}</div>${skel('skel-chart')}</div>`;
const skToolbar = () => `<div class="toolbar">${skel('skel-input wide')}${skel('skel-input', 2)}${skel('skel-btn')}</div>`;
const skTable = (rows = 6, cols = 5) => `<div class="skel-table"><div class="skel-row head cols-${cols}">${skel('skel-line sm', cols)}</div>${Array.from({ length: rows }, () => `<div class="skel-row cols-${cols}">${skel('skel-line md')}${skel('skel-line sm', cols - 1)}</div>`).join('')}</div>`;
const skFacts = n => `<div class="skel-facts">${Array.from({ length: n }, () => `<div class="skel-fact">${skel('skel-line sm')}${skel('skel-line lg')}</div>`).join('')}</div>`;
const skPlans = n => `<div class="plans pricing" data-count="${n}">${Array.from({ length: n }, () => `<div class="card plan-card skel-plan"><div class="plan-top">${skel('skel-line sm')}</div>${skel('skel-metric')}${skel('skel-line md')}<div class="skel-feats">${skel('skel-line lg', 3)}</div>${skel('skel-line lg')}${skel('skel-btn wide')}</div>`).join('')}</div><div class="assurance skel-assure">${skel('skel-line md', 3)}</div>`;
const skHero = () => `<div class="card balance-hero"><div class="balance-main">${skel('skel-line sm')}${skel('skel-metric')}${skel('skel-line md')}</div><div class="actions">${skel('skel-btn')}</div></div>`;
const skWays = () => `<div class="ways">${Array.from({ length: 3 }, () => `<div class="card way">${skel('skel-line md')}${skel('skel-line lg', 2)}${skel('skel-btn')}</div>`).join('')}</div>`;
const skCard = body => `<div class="card">${body}</div>`;

const SKELETON_VIEWS = {
  overview: () => admin
    ? `${skHead(true)}${skStats(4)}${skChart()}${skStats(3)}<div class="split-even">${skCard(skel('skel-line sm', 1) + skFacts(5))}${skCard(skel('skel-line sm', 1) + skFacts(5))}</div>`
    : `${skBanner()}${skStats(4)}${skChart()}${skCard(`<div class="card-head">${skel('skel-title')}</div>${skTable(4, 5)}`)}`,
  keys: () => `${skHead(true)}${skToolbar()}${skTable(7, 6)}`,
  accounts: () => `${skHead()}${skToolbar()}${skTable(7, 6)}`,
  payments: () => `${skHead(true)}${skToolbar()}${skTable(7, 6)}`,
  requests: () => `${skHead(true)}${skToolbar()}${skTable(8, 6)}`,
  usage: () => `${skHead()}${skStats(3)}${skChart()}${skCard(`<div class="card-head">${skel('skel-title')}</div>${skTable(4, 4)}`)}`,
  billing: () => `${skHero()}${skStats(3)}${skWays()}${skPlans(3)}${skTable(3, 5)}${skTable(3, 5)}`,
  connect: () => `${skHead(true)}${skCard(skFacts(3))}${skCard(`<div class="card-head">${skel('skel-title')}</div>${skTable(3, 5)}`)}${skCard(skel('skel-code'))}`,
  plans: () => `${skHead(true)}${skTable(5, 6)}`,
  codes: () => `${skHead(true)}${skTable(5, 5)}`,
  settings: () => `${skHead()}${skCard(skel('skel-line lg', 2))}${skCard(skel('skel-block'))}`,
};
const viewSkeleton = name => skeleton((SKELETON_VIEWS[name] || SKELETON_VIEWS.keys)(), 'Loading');
const tabsSkeleton = () => `<div class="tabs" aria-hidden="true">${skel('skel-tab', 5)}</div>`;
const modalSkeleton = () => skeleton(`${skel('skel-title')}${skFacts(4)}${skel('skel-chart')}${skTable(3, 4)}`, 'Loading details');
const plansSkeleton = () => skeleton(skPlans(3), 'Loading plans');
const docsSkeleton = () => skeleton(`<div>${skel('skel-line', 1)}${skel('skel-line md', 1)}${skel('skel-line sm', 1)}${skel('skel-line md', 1)}${skel('skel-line', 1)}${skel('skel-line sm', 1)}</div><div class="skeleton-body wide">${skel('skel-title')}${skel('skel-line lg')}${skel('skel-block')}${skel('skel-line md')}${skTable(4, 4)}</div>`, 'Loading documentation', 'skel-docs');
const landingSkeleton = () => skeleton(`<div>${skel('skel-eyebrow')}${skel('skel-hero', 2)}${skel('skel-line lg')}${skel('skel-line md')}<div class="actions section">${skel('skel-btn', 2)}</div></div>${skel('skel-block')}`, 'Loading', 'skel-landing');
const consentSkeleton = () => skeleton(`<div class="card consent-card"><div class="skel-center">${skel('skel-circle', 2)}</div>${skel('skel-title')}${skel('skel-line md')}<div class="skel-facts">${skel('skel-line lg', 3)}${skel('skel-input')}${skel('skel-block')}</div></div>`, 'Loading authorization request', 'skel-consent');
const shellSkeleton = () => `${tabsSkeleton()}<section id="content" aria-busy="true">${viewSkeleton('overview')}</section>`;
// Painted before any data is known, so every route has a shaped placeholder at once.
function bootSkeleton() {
  if (docsPage) return docsSkeleton();
  if (oauthPage) return consentSkeleton();
  if (admin || workspace) return token ? shellSkeleton() : '';
  return landingSkeleton();
}

function modal(title, content, setup, size) {
  const d = document.createElement('dialog');
  if (size) d.classList.add(size);
  d.innerHTML = `<div class="dialog-head"><h2>${esc(title)}</h2><button type="button" data-close aria-label="Close dialog">&times;</button></div><div class="dialog-content">${content}</div>`;
  document.body.append(d);
  d.querySelector('[data-close]').onclick = () => d.close();
  // Close on a backdrop click (the dialog box itself fills its own rect).
  d.addEventListener('click', e => {
    if (e.target !== d) return;
    const r = d.getBoundingClientRect();
    if (e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom) d.close();
  });
  d.addEventListener('close', () => d.remove());
  d.showModal();
  setup?.(d);
  return d;
}
// A dialog whose body loads asynchronously and can be refreshed in place.
function liveModal(title, size, fetcher, render) {
  const d = modal(title, `<div class="live-body" aria-busy="true">${modalSkeleton()}</div>`, null, size);
  const body = d.querySelector('.live-body');
  let seq = 0;
  const refresh = async () => {
    const mine = ++seq;
    body.setAttribute('aria-busy', 'true');
    try {
      const data = await fetcher();
      if (mine !== seq || !body.isConnected) return;
      body.innerHTML = render(data, refresh);
      wireCopies(body);
      wireCharts(body);
      render.after?.(body, data, refresh);
    } catch (e) {
      if (mine !== seq || !body.isConnected) return;
      body.innerHTML = `<div class="card error">${esc(e.message)}</div>`;
    } finally {
      if (mine === seq) body.removeAttribute('aria-busy');
    }
  };
  refresh();
}

/* List state and routing --------------------------------------------------------
   The hash holds the tab and its filters, e.g. #requests?key=<id>, so every
   filtered view can be linked, bookmarked, and reached with Back. */
const LIST_DEFAULTS = {
  keys: { q: '', status: '', sort: 'created', dir: 'desc', group: '', account: '', limit: 20, offset: 0 },
  accounts: { q: '', status: '', sort: 'created', dir: 'desc', group: '', limit: 20, offset: 0 },
  payments: { q: '', status: '', sort: 'created', dir: 'desc', group: '', account: '', limit: 20, offset: 0 },
  requests: { q: '', kind: '', status: '', model: '', account: '', key: '', limit: 20, offset: 0 },
};
const FILTERS = Object.fromEntries(Object.entries(LIST_DEFAULTS).map(([name, d]) => [name, Object.keys(d).filter(k => k !== 'limit' && k !== 'offset')]));
const listStates = Object.fromEntries(Object.entries(LIST_DEFAULTS).map(([name, d]) => [name, { ...d }]));
const TABS = admin
  ? ['overview', 'accounts', 'keys', 'requests', 'payments', 'plans', 'codes', 'settings']
  : ['overview', 'keys', 'requests', 'usage', 'billing', 'connect'];
const TAB_LABELS = { overview: 'Overview', accounts: 'Accounts', keys: 'API keys', requests: 'Requests', payments: 'Payments', plans: 'Plans', codes: 'Redeem codes', settings: 'Settings', usage: 'Usage', billing: 'Billing', connect: 'Connect' };

function hashFor(name) {
  const state = listStates[name];
  const params = new URLSearchParams();
  if (state) for (const f of FILTERS[name]) if (state[f] && state[f] !== LIST_DEFAULTS[name][f]) params.set(f, state[f]);
  const qs = params.toString();
  return `#${name}${qs ? `?${qs}` : ''}`;
}
// Reads the tab and its filters from the hash into the list state.
function applyRoute() {
  const [name, qs = ''] = location.hash.slice(1).split('?');
  tab = TABS.includes(name) ? name : 'overview';
  const state = listStates[tab];
  if (state) {
    const params = new URLSearchParams(qs);
    Object.assign(state, LIST_DEFAULTS[tab], { limit: state.limit });
    for (const f of FILTERS[tab]) if (params.get(f)) state[f] = params.get(f);
  }
}
// Navigates to a tab. With params, its filters are replaced by them.
function go(name, params) {
  document.querySelectorAll('dialog[open]').forEach(d => d.close());
  const state = listStates[name];
  if (state && params) Object.assign(state, LIST_DEFAULTS[name], { limit: state.limit }, params);
  const next = hashFor(name);
  if (location.hash === next) {
    applyRoute();
    selectTab();
    load();
  } else {
    location.hash = next;
  }
}
// Records filter changes made in place without adding history entries.
const syncHash = () => history.replaceState(null, '', hashFor(tab));

function listToolbar(id, state, { placeholder, statuses, sorts, groups }) {
  const options = (items, selected) => items.map(([value, label]) => `<option value="${esc(value)}"${value === selected ? ' selected' : ''}>${esc(label)}</option>`).join('');
  return `<form class="toolbar" data-list-form="${id}" role="search">
    <input class="toolbar-search" name="q" type="search" value="${esc(state.q)}" aria-label="Search" placeholder="${esc(placeholder)}">
    <select name="status" aria-label="Filter by status"><option value="">All statuses</option>${options(statuses, state.status)}</select>
    <select name="sort" aria-label="Sort by">${options(sorts, state.sort)}</select>
    <select name="dir" aria-label="Sort direction"><option value="desc"${state.dir === 'desc' ? ' selected' : ''}>High to low, newest first</option><option value="asc"${state.dir === 'asc' ? ' selected' : ''}>Low to high, oldest first</option></select>
    ${groups ? `<select name="group" aria-label="Group by"><option value="">No grouping</option>${options(groups, state.group)}</select>` : ''}
    <div class="toolbar-actions"><button type="submit" class="primary">Apply</button><button type="button" class="ghost" data-list-clear="${id}">Reset</button></div>
  </form>`;
}
// A removable filter shown above a list, e.g. "Account: ops@example.com".
function filterChip(id, field, label, value, extra = '') {
  return `<div class="filter-chip"><span class="label">${esc(label)}</span><strong>${value}</strong>${extra}<button type="button" class="chip-remove" data-chip-remove="${esc(id)}" data-chip-field="${esc(field)}" aria-label="Remove ${esc(label.toLowerCase())} filter">&times;</button></div>`;
}
function listPager(id, state, total, shown) {
  const start = total ? state.offset + 1 : 0;
  const end = state.offset + shown;
  return `<div class="pager">
    <span class="hint">${total ? `Showing ${count(start)} to ${count(end)} of ${count(total)}` : 'No matching rows'}</span>
    <div class="pager-actions"><label class="inline" for="${id}-size">Rows</label><select id="${id}-size" data-list-size="${id}">${[20, 50, 100].map(n => `<option${state.limit === n ? ' selected' : ''}>${n}</option>`).join('')}</select><button type="button" class="ghost" data-list-prev="${id}"${state.offset === 0 ? ' disabled' : ''}>Previous</button><button type="button" class="ghost" data-list-next="${id}"${end >= total ? ' disabled' : ''}>Next</button></div>
  </div>`;
}
function bindForm(form, handler) {
  form.addEventListener('submit', async e => {
    e.preventDefault();
    const button = form.querySelector('[type=submit]');
    if (button) button.disabled = true;
    try {
      await handler(Object.fromEntries(new FormData(form)));
    } catch (e) {
      notice(e.message, true);
    } finally {
      if (button) button.disabled = false;
    }
  });
}
function confirmModal(title, message, confirmLabel = 'Confirm', danger = false) {
  return new Promise(resolve => {
    const d = modal(title, `
      <p class="muted">${esc(message)}</p>
      <div class="actions section">
        <button type="button" data-cancel class="ghost">Cancel</button>
        <button type="button" data-confirm class="${danger ? 'danger-solid' : 'primary'}">${esc(confirmLabel)}</button>
      </div>`, el => {
      el.querySelector('[data-cancel]').onclick = () => d.close();
      el.querySelector('[data-confirm]').onclick = () => { d.close(); resolve(true); };
    });
    d.addEventListener('close', () => resolve(false));
  });
}

const CHECK = '<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" focusable="false"><path d="M3 8.5l3.2 3.2L13 4.8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>';
function planCard(p, badges, cta) {
  const i = planInfo(p);
  const badge = badges.get(p.code);
  const featured = badge === 'Best value';
  return `<article class="card plan-card${featured ? ' featured' : ''}" aria-label="${esc(p.name)} plan">
    <div class="plan-top"><h3 class="plan-name">${esc(p.name)}</h3>${badge ? `<span class="plan-flag${featured ? '' : ' soft'}">${esc(badge)}</span>` : ''}</div>
    <div class="plan-price"><span class="plan-amount">${esc(perMonthLabel(i))}</span>${i.price > 0 ? '<span class="plan-unit">/mo</span>' : ''}</div>
    <div class="plan-billed">${esc(billedLine(i))}</div>
    ${i.credit ? `<ul class="plan-features">
      <li>${CHECK}<span><strong>${esc(usdShort(i.credit))} API credit</strong>${i.bonus ? `<span class="chip-bonus">+${esc(usdShort(i.bonus))} bonus</span>` : ''}</span></li>
      <li>${CHECK}<span>&asymp; ${esc(usdShort(i.creditPerMonth))} of usage per month</span></li>
      <li>${CHECK}<span>Credit valid for ${esc(plural(i.days, 'day'))}</span></li>
    </ul>` : '<ul class="plan-features"></ul>'}
    <p class="plan-desc">${esc(p.description || 'Prepaid API credit.')}</p>
    <div class="plan-cta">${cta || `<a class="button${featured ? ' primary' : ''}" href="/workspace/#billing">Choose plan</a>`}</div>
  </article>`;
}
const methodNames = methods => [methods.includes('stripe') && 'card', methods.includes('crypto') && 'crypto'].filter(Boolean);
// Plan grid, reassurance row, and a short FAQ. `cta(plan)` renders each button.
function pricingSection(plans, methods = [], cta) {
  if (!plans.length) return emptyState('No plans yet', 'An operator has not published any plans.');
  const badges = planBadges(plans);
  const paid = methodNames(methods);
  const assurances = [
    'Credit stays valid until the plan ends',
    paid.length ? `Pay by ${paid.join(' or ')}` : 'Redeem a credit code from the operator',
    'One-time purchase, no auto-renewal',
    'Usage-based: each request is deducted from credit',
  ];
  const terms = plans.map(p => `${p.name}: ${plural(p.duration_days, 'day')}`).join('; ');
  const faq = [
    ['What is API credit?', `Credit is a prepaid USD balance. Each request is priced from its usage (tokens, characters, audio seconds, or images) and deducted from it. Every key on your account shares the balance.`],
    ['What happens when my credit runs out?', 'Requests stop with a 429 insufficient_quota error that explains how to top up. Buy another plan, redeem a credit code, or add funds, and calls work again right away.'],
    ['When does my credit expire?', `Plan credit expires when its plan ends (${terms}). Credit that expires soonest is spent first, so nothing is lost while you still have time left.`],
    ['Can I pay as I go?', methods.includes('crypto')
      ? 'Yes. Open Billing in your workspace and send stablecoins to your personal deposit address. Any amount is credited 1:1 in USD, with no plan needed.'
      : 'Plans are prepaid packages. Ask the operator for a credit code to add a custom amount.'],
  ];
  return `<div class="plans pricing" data-count="${plans.length}">${plans.map(p => planCard(p, badges, cta?.(p))).join('')}</div>
    <ul class="assurance">${assurances.map(a => `<li>${CHECK}${esc(a)}</li>`).join('')}</ul>
    <div class="faq" role="list">${faq.map(([q, a]) => `<details role="listitem"><summary>${esc(q)}</summary><p>${esc(a)}</p></details>`).join('')}</div>`;
}

/* Checkout ---------------------------------------------------------------------
   Choose plan -> Pay -> Credit added, in one dialog. Card payments hand off to
   Stripe. Crypto orders stay in the dialog and update while it is open. */
let planCatalog = { plans: [], methods: [] };
const STEPS = ['Choose plan', 'Pay', 'Credit added'];
const stepper = current => `<ol class="stepper" aria-label="Checkout progress">${STEPS.map((label, n) => `<li class="${n < current ? 'done' : n === current ? 'current' : ''}"${n === current ? ' aria-current="step"' : ''}><span class="step-dot">${n < current ? CHECK : n + 1}</span><span class="step-label">${esc(label)}</span></li>`).join('')}</ol>`;
const expiryText = (i, paidAt) => paidAt
  ? `Valid until ${day(paidAt + i.days * DAY, { dateStyle: 'medium' })}`
  : `${plural(i.days, 'day')} from payment, until ${day(nowSeconds() + i.days * DAY, { dateStyle: 'medium' })}`;
function orderSummary(p, { paidAt } = {}) {
  const i = planInfo(p);
  return `<aside class="card order-summary" aria-label="Order summary">
    <div class="label">Order summary</div>
    <div class="order-plan"><strong>${esc(p.name || p.code)}</strong><span class="hint">${esc(perMonthLabel(i))}${i.price > 0 ? ' per month' : ''}</span></div>
    ${facts([
      ['Total today', `<strong>${esc(usdShort(i.price))}</strong> <span class="muted">${esc(i.months === 1 ? 'for 1 month' : `for ${i.months} months`)}, one-time</span>`],
      i.credit ? ['API credit', `<strong>${esc(usdShort(i.credit))}</strong>${i.bonus ? `<span class="chip-bonus">+${esc(usdShort(i.bonus))} bonus</span>` : ''}`] : null,
      ['Credit validity', esc(expiryText(i, paidAt))],
    ])}
  </aside>`;
}
const METHOD_INFO = {
  stripe: ['Card', 'Pay securely on the Stripe checkout page.'],
  crypto: ['Crypto', 'USDC, USDT, and native coins on supported networks.'],
};
function checkout(plan, methods) {
  if (!methods.length) return notice('Online payments are not enabled. Use a credit code.', true);
  const i = planInfo(plan);
  modal(`Checkout: ${plan.name}`, `<div data-pay>
    ${stepper(1)}
    <form class="checkout-grid">
      <div>
        <fieldset class="method-choice"><legend>Payment method</legend>
          ${methods.map((m, n) => `<label class="method"><input type="radio" name="method" value="${esc(m)}"${n === 0 ? ' checked' : ''}><span class="method-body"><strong>${esc(METHOD_INFO[m]?.[0] || m)}</strong><span class="hint">${esc(METHOD_INFO[m]?.[1] || '')}</span></span></label>`).join('')}
        </fieldset>
        <p class="field-help">Plans are one-time purchases and do not renew. Credit is added to the balance shared by all your keys.</p>
      </div>
      ${orderSummary(plan)}
      <div class="actions checkout-actions"><button type="button" class="ghost" data-cancel>Cancel</button><button type="submit" class="primary" data-submit>Continue to payment</button></div>
    </form>
  </div>`, d => {
    const box = d.querySelector('[data-pay]');
    const form = box.querySelector('form');
    const submit = form.querySelector('[data-submit]');
    const label = () => {
      const m = form.elements.method.value;
      submit.textContent = m === 'stripe' ? `Pay ${usdShort(i.price)} by card` : `Show crypto payment details`;
    };
    label();
    form.addEventListener('change', label);
    form.querySelector('[data-cancel]').onclick = () => d.close();
    bindForm(form, async b => {
      const v = await api('/v1/account/checkout', 'POST', { plan_code: plan.code, method: b.method });
      if (v.method === 'crypto') {
        d.querySelector('h2').textContent = 'Pay with crypto';
        runOrder(d, v);
        if (tab === 'billing') load();
        return;
      }
      const url = new URL(v.url);
      if (url.protocol !== 'https:' || url.hostname !== 'checkout.stripe.com') throw new Error('Unexpected checkout URL');
      submit.textContent = 'Redirecting to Stripe...';
      submit.disabled = true;
      location.href = url.href;
      await new Promise(() => {});
    });
  }, 'wide');
}

/* Charts ----------------------------------------------------------------------- */

// Sums a usage series into one entry per UTC day over the last `days`, with
// empty days filled in so the time axis is continuous.
function dailyTotals(series, days = 30) {
  const totals = new Map();
  for (const r of series) {
    const current = totals.get(Number(r.day)) || { cost: 0, requests: 0, tokens: 0 };
    current.cost += Number(r.billed_cost_usd);
    current.requests += Number(r.requests || 0);
    current.tokens += Number(r.tokens || 0);
    totals.set(Number(r.day), current);
  }
  const today = Math.floor(nowSeconds() / DAY) * DAY;
  const first = Math.min(today - days * DAY, ...totals.keys());
  const out = [];
  for (let d = first; d <= today; d += DAY) out.push([d, totals.get(d) || { cost: 0, requests: 0, tokens: 0 }]);
  return out;
}
// Per-model totals of a usage series, largest spend first.
function modelTotals(series) {
  const models = new Map();
  for (const r of series) {
    const m = models.get(r.model) || { model: r.model, cost: 0, upstream: 0, requests: 0, tokens: 0 };
    m.cost += Number(r.billed_cost_usd);
    m.upstream += Number(r.upstream_cost_usd || 0);
    m.requests += Number(r.requests || 0);
    m.tokens += Number(r.tokens || 0);
    models.set(r.model, m);
  }
  return [...models.values()].sort((a, b) => b.cost - a.cost || b.requests - a.requests);
}
const seriesTotals = series => series.reduce((t, r) => ({
  cost: t.cost + Number(r.billed_cost_usd),
  requests: t.requests + Number(r.requests || 0),
  tokens: t.tokens + Number(r.tokens || 0),
}), { cost: 0, requests: 0, tokens: 0 });

let chartSeq = 0;
function chart(series, { days = 30, compact = false } = {}) {
  if (!series.length) return emptyState('No usage yet', 'Usage appears here after the first API request.');
  const entries = dailyTotals(series, days);
  const totalCost = entries.reduce((sum, [, v]) => sum + v.cost, 0);
  const mode = totalCost > 0 ? 'cost' : 'requests';
  const value = v => mode === 'cost' ? v.cost : v.requests;
  const max = Math.max(...entries.map(([, v]) => value(v)), 1e-12);
  const W = 600, H = compact ? 120 : 170, base = H - 6, top = 10;
  const step = W / entries.length;
  const barWidth = Math.max(3, Math.min(22, step * .62));
  const id = `usage-${++chartSeq}`;
  const bars = entries.map(([d, v], i) => {
    const x = step * i + step / 2;
    const height = value(v) > 0 ? Math.max(2, (value(v) / max) * (base - top)) : 0;
    const summary = `${day(d, { weekday: 'short', month: 'short', day: 'numeric' })}|${usd(v.cost)} billed|${plural(v.requests, 'request')}|${count(v.tokens)} tokens`;
    return `<g class="chart-column" tabindex="0" data-chart-x="${(x / W).toFixed(4)}" data-chart-summary="${esc(summary)}"><rect class="chart-hit" x="${(step * i).toFixed(1)}" y="0" width="${step.toFixed(1)}" height="${base}"/>${height ? `<rect class="chart-bar" x="${(x - barWidth / 2).toFixed(1)}" y="${(base - height).toFixed(1)}" width="${barWidth.toFixed(1)}" height="${height.toFixed(1)}" rx="2"/>` : ''}<title>${esc(summary.replaceAll('|', ', '))}</title></g>`;
  }).join('');
  const mid = entries[Math.floor(entries.length / 2)][0];
  return `<div class="chart-wrap${compact ? ' compact' : ''}">
    <div class="chart-legend"><span><i></i>${mode === 'cost' ? `Billed per day, last ${days} days` : `Requests per day, last ${days} days (nothing billed)`}</span><strong>${mode === 'cost' ? esc(usd(totalCost)) : count(entries.reduce((sum, [, v]) => sum + v.requests, 0))}</strong></div>
    <svg class="chart" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-labelledby="${id}-title"><title id="${id}-title">${mode === 'cost' ? 'Daily billed cost' : 'Daily request count'}</title>
      <line x1="0" y1="${base}" x2="${W}" y2="${base}"/>
      ${bars}
    </svg>
    <div class="chart-tooltip" role="status" aria-live="polite" hidden></div>
    <div class="chart-foot"><span>${esc(day(entries[0][0]))}</span><span>${esc(day(mid))}</span><span>${esc(day(entries[entries.length - 1][0]))}</span></div>
  </div>`;
}

function wireCharts(root) {
  root.querySelectorAll('.chart-wrap').forEach(wrap => {
    const tooltip = wrap.querySelector('.chart-tooltip');
    const show = column => {
      const [heading, ...lines] = column.dataset.chartSummary.split('|');
      tooltip.innerHTML = `<strong>${esc(heading)}</strong>${lines.map(line => `<span>${esc(line)}</span>`).join('')}`;
      tooltip.hidden = false;
      // Keep the tooltip inside the chart: anchor it at the column, clamped.
      const x = Number(column.dataset.chartX);
      tooltip.style.left = `${Math.min(Math.max(x * 100, 16), 84)}%`;
      column.classList.add('active');
    };
    const hide = column => {
      tooltip.hidden = true;
      column.classList.remove('active');
    };
    wrap.querySelectorAll('.chart-column').forEach(column => {
      column.addEventListener('mouseenter', () => show(column));
      column.addEventListener('focus', () => show(column));
      column.addEventListener('mouseleave', () => hide(column));
      column.addEventListener('blur', () => hide(column));
    });
  });
}

// Spend per model as a table with each model's share of the total.
function modelTable(series, { upstream = false } = {}) {
  const models = modelTotals(series);
  const total = models.reduce((s, m) => s + m.cost, 0);
  const columns = [
    ['Model', m => `<code>${esc(m.model || '-')}</code>`, 'wrap'],
    ['Requests', m => `<span class="num">${count(m.requests)}</span>`, 'end'],
    ['Tokens', m => `<span class="num">${count(m.tokens)}</span>`, 'end'],
    ['Billed', m => `<span class="num">${esc(usd(m.cost))}</span>`, 'end'],
  ];
  if (upstream) columns.push(['Upstream', m => `<span class="num">${esc(usd(m.upstream))}</span>`, 'end']);
  columns.push(['Share', m => `<span class="share"><span class="share-bar"><i data-share="${total > 0 ? Math.round((m.cost / total) * 100) : 0}"></i></span><span class="num">${percent(m.cost, total)}</span></span>`]);
  return table(columns, models, { empty: ['No usage yet', 'Model totals appear after the first request.'] });
}
// Share bars are sized with CSSOM (the CSP forbids inline style attributes).
function wireShares(root) {
  root.querySelectorAll('[data-share]').forEach(i => { i.style.width = `${i.dataset.share}%`; });
}

/* Key creation and secrets ----------------------------------------------------- */

function redeemForm() {
  modal('Redeem a credit code', `
    <form>
      <div class="redeem-intro"><span class="redeem-icon" aria-hidden="true">+</span><div><h3>Apply credit to your balance</h3><p>Enter the single-use code supplied by your operator. Credit is available immediately after redemption.</p></div></div>
      <label for="redeem-code">Credit code</label>
      <input id="redeem-code" name="code" autocomplete="off" required placeholder="RES-...">
      <div class="actions"><button type="button" class="ghost" data-cancel>Cancel</button><button type="submit" class="primary">Redeem credit</button></div>
    </form>`, d => {
    d.querySelector('[data-cancel]').onclick = () => d.close();
    bindForm(d.querySelector('form'), async b => {
      await api('/v1/account/redeem', 'POST', b);
      d.close();
      notice('Credit applied');
      await load();
    });
  });
}

function secrets(v, { embedded = false } = {}) {
  modal('Save your credentials', `
    <p class="muted">These are shown only once. Keep the dashboard token to manage your account even if you block your API key.</p>
    <div class="cred-row">
      <span class="cred-label">API key</span>
      <code class="cred-value secret" title="${esc(v.key)}">${esc(v.key)}</code>
      <button type="button" class="small" data-copy="key">Copy</button>
    </div>
    <div class="cred-row">
      <span class="cred-label">Dashboard token</span>
      <code class="cred-value" title="${esc(v.dashboard_token)}">${esc(v.dashboard_token)}</code>
      <button type="button" class="small" data-copy="token">Copy</button>
    </div>
    <div class="actions section">
      <button type="button" data-copy-all class="primary">Copy both</button>
      ${admin || embedded ? '' : '<button type="button" data-open class="ghost">Open workspace</button>'}
    </div>`, d => {
    const wire = (selector, text, label) => {
      const button = d.querySelector(selector);
      const row = button.closest('.cred-row');
      const copy = () => copyText(text, label).then(ok => {
        if (ok) {
          flashCopied(button);
          flash(row);
        }
      });
      button.onclick = copy;
      row.querySelector('.cred-value').onclick = copy;
    };
    wire('[data-copy="key"]', v.key, 'API key');
    wire('[data-copy="token"]', v.dashboard_token, 'Dashboard token');
    const copyAll = d.querySelector('[data-copy-all]');
    copyAll.onclick = () => copyText(JSON.stringify(v, null, 2), 'Credentials').then(ok => ok && flashCopied(copyAll));
    d.querySelector('[data-open]')?.addEventListener('click', () => {
      sessionStorage.setItem(CUSTOMER_STORE, v.dashboard_token);
      location.href = '/workspace/';
    });
  }, 'wide');
}

function keyForm() {
  const signedIn = Boolean(token) && !docsPage;
  modal(signedIn ? 'Create an API key' : 'Create an account', `
    <form>
      <label for="key-name">Key name</label>
      <input id="key-name" name="name" maxlength="80" value="${esc(suggestedName())}" placeholder="My application" required>
      <label for="contact">Contact (optional)</label>
      <input id="contact" name="contact" type="email" placeholder="you@example.com">
      <p class="field-help">${signedIn ? 'The new key shares your account balance.' : 'New accounts receive evaluation credit. Additional keys share the same balance.'}</p>
      <div class="pow" role="status" aria-live="polite" hidden><div class="loading"><span class="spinner" aria-hidden="true"></span><span class="pow-text">Verifying you're human...</span></div><progress max="100" value="0"></progress></div>
      <div class="actions"><button class="primary" type="submit">Create key</button></div>
    </form>`, d => bindForm(d.querySelector('form'), async b => {
    const pow = d.querySelector('.pow');
    const bar = pow.querySelector('progress');
    const ctl = new AbortController();
    d.addEventListener('close', () => ctl.abort(), { once: true });
    let v;
    try {
      if (signedIn) v = await api('/v1/keys', 'POST', b);
      else {
        pow.hidden = false;
        v = await signup(b, p => { bar.value = Math.round(p * 100); }, ctl.signal);
      }
    } finally {
      pow.hidden = true;
    }
    d.close();
    secrets(v);
    if (document.querySelector('#content')) await load();
  }));
}

/* Key details ------------------------------------------------------------------ */

function limitList(l = {}) {
  const parts = [];
  if (l.rpm) parts.push(['Requests per minute', count(l.rpm)]);
  if (l.requests) parts.push(['Requests', `${count(l.requests.max)} per ${plural(l.requests.window_hours, 'hour')}`]);
  if (l.tokens) parts.push(['Tokens', `${count(l.tokens.max)} per ${plural(l.tokens.window_hours, 'hour')}`]);
  if (l.spend) parts.push(['Spend', `${usd(l.spend.max_usd)} per ${plural(l.spend.window_hours, 'hour')}`]);
  if (l.max_children) parts.push(['Child keys', count(l.max_children)]);
  if (l.allowed_models?.length) parts.push(['Models', l.allowed_models.map(m => `<code>${esc(m)}</code>`).join(' ')]);
  return parts.map(([k, v]) => [k, k === 'Models' ? v : esc(v)]);
}
async function blockKey(kid) {
  if (!(await confirmModal('Block key?', admin
    ? 'Requests with this key are refused until it is set back to active.'
    : 'Requests with this key are refused. Only the operator can unblock it.', 'Block key', true))) return false;
  await api(endpoint(`keys/${kid}`), 'PATCH', { status: 'blocked' });
  notice('Key blocked');
  return true;
}
async function revokeKey(kid) {
  if (!(await confirmModal('Revoke key?', 'Permanently revoke this key and its dashboard token? This cannot be undone.', 'Revoke key', true))) return false;
  await api(endpoint(`keys/${kid}/revoke`), 'POST', {});
  notice('Key revoked');
  return true;
}
async function rotateKey(kid) {
  if (!(await confirmModal('Rotate key?', 'A new secret replaces this key and the old one stops working. Applications using it need the new key.', 'Rotate key'))) return false;
  secrets(await api(endpoint(`keys/${kid}/rotate`), 'POST', {}));
  return true;
}

function keyDetails(kid) {
  const render = v => {
    const k = v.key;
    const a = v.account?.account;
    const series = v.usage?.series || [];
    const t = seriesTotals(series);
    const limits = limitList(k.limits);
    const expired = k.expires_at && k.expires_at < nowSeconds();
    return `
      <div class="entity-head">
        <div class="entity-title"><h3>${esc(k.name)}</h3>${pill(k.status)}${expired ? pill('expired', 'warn') : ''}</div>
        <div class="entity-sub">${copyable(k.key_prefix + '...', 'Key prefix')}<span class="hint">ID</span>${copyable(k.id, 'Key ID', shortId(k.id))}</div>
      </div>
      <div class="tiles">
        ${metric('Spent, all time', usd(k.billed_usd))}
        ${metric('Last 30 days', usd(t.cost), esc(plural(t.requests, 'request')))}
        ${metric('Tokens, 30 days', count(t.tokens))}
        ${metric('Last used', when(k.last_used_at), `Created ${esc(date(k.created_at))}`, { html: true })}
      </div>
      <div class="quick-links">
        ${goButton('View requests', 'requests', { key: k.id }, 'primary small')}
        <button type="button" class="small" data-key-edit>${admin ? 'Edit key' : 'Rename'}</button>
        <button type="button" class="small" data-key-rotate${k.status !== 'active' ? ' disabled' : ''}>Rotate</button>
        ${k.status === 'active' ? '<button type="button" class="small danger" data-key-block>Block</button>' : ''}
        ${admin && k.status !== 'revoked' ? '<button type="button" class="small danger" data-key-revoke>Revoke</button>' : ''}
      </div>
      <section class="detail-section">${chart(series, { compact: true })}</section>
      ${series.length ? `<section class="detail-section"><h4>By model</h4>${modelTable(series, { upstream: admin })}</section>` : ''}
      <div class="detail-columns">
        <section class="detail-section"><h4>Settings</h4>${facts([
          ['Expires', k.expires_at ? esc(date(k.expires_at)) : 'Never'],
          admin && ['Markup', k.markup_pct ? `${esc(k.markup_pct)}%` : 'Account default'],
          ...(limits.length ? limits : [['Limits', 'Account defaults']]),
        ])}</section>
        ${a ? `<section class="detail-section"><h4>Account</h4>${facts([
          ['Account', accountLink(a.id, a.contact)],
          ['Status', pill(a.status)],
          ['Balance', `<span class="num">${esc(usd(a.balance_usd))}</span>`],
          ['Spent, all time', `<span class="num">${esc(usd(a.billed_usd))}</span>`],
        ])}</section>` : ''}
      </div>`;
  };
  render.after = (body, v, refresh) => {
    wireShares(body);
    const k = v.key;
    const run = fn => async e => {
      const button = e.currentTarget;
      button.disabled = true;
      try {
        if (await fn()) {
          refresh();
          if (document.querySelector('#content')) load();
        }
      } catch (err) {
        notice(err.message, true);
      } finally {
        button.disabled = false;
      }
    };
    body.querySelector('[data-key-edit]').onclick = () => editKeyForm(k.id, k.name, () => { refresh(); load(); });
    body.querySelector('[data-key-rotate]').onclick = run(() => rotateKey(k.id));
    body.querySelector('[data-key-block]')?.addEventListener('click', run(() => blockKey(k.id)));
    body.querySelector('[data-key-revoke]')?.addEventListener('click', run(() => revokeKey(k.id)));
  };
  liveModal('API key', 'large', () => api(endpoint(`keys/${encodeURIComponent(kid)}`)), render);
}

async function editKeyForm(kid, name, done) {
  const key = admin ? (await api(endpoint(`keys/${kid}`))).key : { name };
  const limits = key.limits || {};
  const windowFields = (field, label, value, placeholder, step = '1') => `<div><label for="key-${field}-max">${label}</label><input id="key-${field}-max" name="${field}_max" type="number" min="${step === '1' ? '1' : '0.000001'}" step="${step}" value="${esc(value?.max || '')}" placeholder="No limit"></div><div><label for="key-${field}-window">Window (hours)</label><input id="key-${field}-window" name="${field}_window" type="number" min="1" value="${esc(value?.window_hours || '')}" placeholder="${placeholder}"></div>`;
  modal(admin ? 'Edit API key' : 'Rename API key', `<form>
    <label for="edit-key-name">Key name</label><input id="edit-key-name" name="name" maxlength="80" value="${esc(key.name)}" required>
    ${admin ? `<div class="form-grid"><div><label for="edit-key-status">Status</label><select id="edit-key-status" name="status">${key.status === 'revoked' ? '<option value="revoked">Revoked (permanent)</option>' : `<option value="active"${key.status === 'active' ? ' selected' : ''}>Active</option><option value="blocked"${key.status === 'blocked' ? ' selected' : ''}>Blocked</option>`}</select></div><div><label for="edit-key-markup">Markup %</label><input id="edit-key-markup" name="markup_pct" type="number" step="0.000001" value="${esc(key.markup_pct || '')}" placeholder="Account default"></div><div><label for="edit-key-expiry">Expires</label><input id="edit-key-expiry" name="expires_at" type="datetime-local" value="${esc(localDateTime(key.expires_at))}"></div><div><label for="edit-key-rpm">Requests per minute</label><input id="edit-key-rpm" name="rpm" type="number" min="1" value="${esc(limits.rpm || '')}" placeholder="No limit"></div></div>
    <details class="advanced-fields"><summary>Usage limits and models</summary><div class="form-grid">${windowFields('requests', 'Request limit', limits.requests, '24')}${windowFields('tokens', 'Token limit', limits.tokens, '720')}${windowFields('spend', 'Spend limit (USD)', limits.spend && { max: limits.spend.max_usd, window_hours: limits.spend.window_hours }, '720', '0.000001')}<div><label for="edit-key-children">Maximum child keys</label><input id="edit-key-children" name="max_children" type="number" min="1" value="${esc(limits.max_children || '')}" placeholder="No limit"></div></div><label for="edit-key-models">Allowed models</label><textarea id="edit-key-models" name="allowed_models" rows="4" placeholder="One model per line. Blank allows all">${esc((limits.allowed_models || []).join('\n'))}</textarea></details>` : ''}
    <div class="actions"><button class="primary" type="submit">Save changes</button></div>
  </form>`, d => bindForm(d.querySelector('form'), async b => {
    const payload = { name: b.name };
    if (admin) {
      const bounded = (field, fallback) => b[`${field}_max`] ? { max: Number(b[`${field}_max`]), window_hours: Number(b[`${field}_window`]) || fallback } : null;
      Object.assign(payload, {
        status: b.status,
        markup_pct: b.markup_pct || null,
        expires_at: toUnix(b.expires_at),
        limits: {
          rpm: b.rpm ? Number(b.rpm) : null,
          requests: bounded('requests', 24),
          tokens: bounded('tokens', 720),
          spend: b.spend_max ? { max_usd: b.spend_max, window_hours: Number(b.spend_window) || 720 } : null,
          max_children: b.max_children ? Number(b.max_children) : null,
          allowed_models: b.allowed_models.split(/[\n,]/).map(v => v.trim()).filter(Boolean),
        },
      });
    }
    await api(endpoint(`keys/${kid}`), 'PATCH', payload);
    d.close();
    notice('Key updated');
    await (done || load)();
  }), 'wide');
}

/* Account management (operator) ------------------------------------------------ */

function manageAccount(aid) {
  const keyLimit = 10;
  let keyOffset = 0;
  let hasPlans;
  const fetcher = async () => {
    const [account, planData] = await Promise.all([
      api(endpoint(`accounts/${aid}?key_limit=${keyLimit}&key_offset=${keyOffset}&key_sort=spend`)),
      hasPlans === undefined ? api(endpoint('plans')) : Promise.resolve(null),
    ]);
    if (planData) hasPlans = planData.plans.some(p => p.active);
    return account;
  };
  const render = v => {
    const a = v.account;
    const sub = a.subscription;
    const keys = v.keys || [];
    const keyTotal = Number(v.key_total ?? keys.length);
    const credits = v.credits || [];
    const series = v.usage?.series || [];
    const t = seriesTotals(series);
    const activeLots = credits.filter(c => lotState(c) === 'active');
    return `
      <div class="entity-head">
        <div class="entity-title"><h3>${esc(a.contact || 'No contact')}</h3>${pill(a.status)}</div>
        <div class="entity-sub"><span class="hint">ID</span>${copyable(a.id, 'Account ID', shortId(a.id))}<span class="hint">Created ${esc(date(a.created_at))}</span></div>
        ${a.note ? `<p class="entity-note">${esc(a.note)}</p>` : ''}
      </div>
      <div class="tiles">
        ${metric('Balance', usd(a.balance_usd), isPositive(a.debt_usd) ? `Debt ${esc(usd(a.debt_usd))} settles from the next top-up` : `${esc(plural(activeLots.length, 'active credit lot'))}`, { tone: isPositive(a.debt_usd) ? 'warn' : '' })}
        ${metric('Spent, all time', usd(a.billed_usd))}
        ${metric('Last 30 days', usd(t.cost), esc(`${plural(t.requests, 'request')}, ${count(t.tokens)} tokens`))}
        ${metric('Plan', sub ? sub.plan_code : 'None', sub ? `Until ${esc(date(sub.expires_at))}` : 'No active plan')}
      </div>
      <div class="quick-links">
        ${goButton('Requests', 'requests', { account: a.id })}
        ${goButton(`Keys (${count(keyTotal)})`, 'keys', { account: a.id })}
        ${goButton('Payments', 'payments', { account: a.id })}
        <span class="spacer"></span>
        <button type="button" class="primary small" data-credit>Add credit</button>
        ${hasPlans ? '<button type="button" class="small" data-sub>Grant plan</button>' : ''}
        <button type="button" class="small" data-key>Create key</button>
        <button type="button" class="small" data-edit>Edit</button>
        ${a.status === 'active' ? '<button type="button" class="danger small" data-suspend>Suspend</button>' : '<button type="button" class="small" data-activate>Reactivate</button>'}
      </div>
      ${a.status === 'suspended' ? '<p class="callout warn">Suspended: its keys cannot call the API and workspace sign-in is blocked. Its credit is not counted as outstanding.</p>' : ''}
      <section class="detail-section">${chart(series, { compact: true })}</section>
      <section class="detail-section">
        <div class="section-head"><h4>Keys by spend</h4><span class="hint">${count(keyTotal)} total</span></div>
        ${table([
          ['Key', k => `${keyLink(k.id, k.name)}<div class="hint"><code>${esc(k.key_prefix)}...</code></div>`, 'wrap'],
          ['Status', k => pill(k.status)],
          ['Spent', k => `<span class="num">${esc(usd(k.billed_usd))}</span>`, 'end'],
          ['30 days', k => `<span class="num">${esc(usd(k.usage_30d_usd))}</span>`, 'end'],
          ['Last used', k => when(k.last_used_at)],
        ], keys, { empty: ['No keys', 'Create a key for this account.'] })}
        ${keyTotal > keyLimit ? `<div class="pager"><span class="hint">${count(keyOffset + 1)} to ${count(Math.min(keyOffset + keys.length, keyTotal))} of ${count(keyTotal)}</span><div class="pager-actions"><button type="button" class="ghost small" data-key-prev${keyOffset === 0 ? ' disabled' : ''}>Previous</button><button type="button" class="ghost small" data-key-next${keyOffset + keys.length >= keyTotal ? ' disabled' : ''}>Next</button></div></div>` : ''}
      </section>
      ${series.length ? `<section class="detail-section"><h4>Spend by model, last 30 days</h4>${modelTable(series, { upstream: true })}</section>` : ''}
      <section class="detail-section">
        <div class="section-head"><h4>Credit lots</h4><span class="hint">Spent soonest-expiring first</span></div>
        ${creditTable(credits)}
      </section>
      <section class="detail-section"><h4>Settings</h4>${facts([
        ['Markup', a.markup_pct ? `${esc(a.markup_pct)}%` : 'Global default'],
        ['Key creation', a.can_create_keys ? 'Beyond self-service limits' : 'Self-service limits'],
        ['Contact', esc(a.contact || '-')],
      ])}</section>`;
  };
  render.after = (body, v, refresh) => {
    wireShares(body);
    const a = v.account;
    body.querySelector('[data-credit]').onclick = () => creditForm(aid, refresh);
    body.querySelector('[data-sub]')?.addEventListener('click', () => grantForm(aid, refresh));
    body.querySelector('[data-key]').onclick = () => createAccountKeyForm(aid, refresh);
    body.querySelector('[data-edit]').onclick = () => editAccountForm(a, refresh);
    body.querySelector('[data-key-prev]')?.addEventListener('click', () => { keyOffset = Math.max(0, keyOffset - keyLimit); refresh(); });
    body.querySelector('[data-key-next]')?.addEventListener('click', () => { keyOffset += keyLimit; refresh(); });
    const setStatus = async (status, message) => {
      try {
        await api(endpoint(`accounts/${aid}`), 'PATCH', { status });
        notice(message);
        refresh();
        if (document.querySelector('#content')) load();
      } catch (e) {
        notice(e.message, true);
      }
    };
    body.querySelector('[data-suspend]')?.addEventListener('click', async () => {
      if (await confirmModal('Suspend account?', 'All of its keys stop working and workspace sign-in is blocked until the account is reactivated.', 'Suspend', true)) setStatus('suspended', 'Account suspended');
    });
    body.querySelector('[data-activate]')?.addEventListener('click', () => setStatus('active', 'Account reactivated'));
  };
  liveModal('Manage account', 'large', fetcher, render);
}

function creditTable(credits) {
  return table([
    ['Source', c => esc(sourceLabel(c.source))],
    ['Amount', c => `<span class="num">${esc(usd(c.amount_usd))}</span>`, 'end'],
    ['Remaining', c => `<span class="num">${esc(usd(c.remaining_usd))}</span>`, 'end'],
    ['State', c => pill(lotState(c))],
    ['Expires', c => c.expires_at ? esc(date(c.expires_at)) : '<span class="muted">Never</span>'],
  ], credits, { empty: ['No credit yet', 'Credit lots appear after a top-up, plan, or grant.'] });
}

function creditForm(aid, done) {
  modal('Add credit', `
    <form>
      <div class="form-grid">
        <div><label for="credit-amount">Amount (USD)</label><input id="credit-amount" name="amount_usd" inputmode="decimal" placeholder="10" required></div>
        <div><label for="credit-source">Source</label>
        <select id="credit-source" name="source">
          <option value="admin" selected>Operator grant</option>
          <option value="refund">Refund</option>
          <option value="purchase">Purchase</option>
        </select></div>
      </div>
      <label for="credit-expiry">Expires (optional)</label>
      <input id="credit-expiry" name="expires_at" type="datetime-local">
      <label for="credit-note">Note (optional)</label>
      <input id="credit-note" name="note" placeholder="Goodwill credit">
      <p class="field-help">Outstanding debt is repaid first. Only the rest becomes spendable.</p>
      <div class="actions"><button class="primary" type="submit">Add credit</button></div>
    </form>`, d => bindForm(d.querySelector('form'), async b => {
    const payload = { amount_usd: b.amount_usd, source: b.source, note: b.note };
    if (b.expires_at) payload.expires_at = toUnix(b.expires_at);
    await api(endpoint(`accounts/${aid}/credits`), 'POST', payload);
    d.close();
    notice('Credit added');
    done?.();
  }));
}

async function grantForm(aid, done) {
  let plans;
  try {
    plans = (await api(endpoint('plans'))).plans.filter(p => p.active);
  } catch (e) {
    notice(e.message, true);
    return;
  }
  if (!plans.length) {
    notice('No active plans to grant', true);
    return;
  }
  modal('Grant plan', `
    <form>
      <label for="grant-plan">Plan</label>
      <select id="grant-plan" name="plan_code" required>
        ${plans.map(p => `<option value="${esc(p.code)}">${esc(p.name)}: ${esc(usd(p.credit_usd))} credit, ${count(p.duration_days)} days</option>`).join('')}
      </select>
      <p class="field-help">Grants the plan's credit immediately. Existing subscriptions are not extended or renewed.</p>
      <div class="actions"><button class="primary" type="submit">Grant plan</button></div>
    </form>`, d => bindForm(d.querySelector('form'), async b => {
    await api(endpoint(`accounts/${aid}/subscriptions`), 'POST', { plan_code: b.plan_code });
    d.close();
    notice('Plan granted');
    done?.();
  }));
}

function createAccountKeyForm(aid, done) {
  modal('Create account key', `
    <form>
      <label for="account-key-name">Key name</label>
      <input id="account-key-name" name="name" maxlength="80" value="${esc(suggestedName())}" required>
      <p class="field-help">Limits, markup, and expiry can be set afterwards with Edit key.</p>
      <div class="actions"><button class="primary" type="submit">Create key</button></div>
    </form>`, d => bindForm(d.querySelector('form'), async b => {
    const v = await api(endpoint(`accounts/${aid}/keys`), 'POST', { name: b.name });
    d.close();
    done?.();
    secrets(v);
  }));
}

function editAccountForm(a, done) {
  modal('Edit account', `
    <form>
      <div class="form-grid">
        <div><label for="account-contact">Contact</label><input id="account-contact" name="contact" type="email" value="${esc(a.contact)}" placeholder="ops@example.com"></div>
        <div><label for="account-status">Status</label>
        <select id="account-status" name="status">
          <option value="active"${a.status === 'active' ? ' selected' : ''}>Active</option>
          <option value="suspended"${a.status === 'suspended' ? ' selected' : ''}>Suspended</option>
        </select></div>
      </div>
      <label for="account-note">Note</label>
      <input id="account-note" name="note" value="${esc(a.note)}" placeholder="VIP customer">
      <label for="account-markup">Markup %</label>
      <input id="account-markup" name="markup_pct" inputmode="decimal" value="${a.markup_pct ? esc(a.markup_pct) : ''}" placeholder="Global default">
      <label class="inline check-line"><input type="checkbox" name="can_create_keys"${a.can_create_keys ? ' checked' : ''}> Allow key creation beyond self-service limits</label>
      <div class="actions"><button class="primary" type="submit">Save changes</button></div>
    </form>`, d => bindForm(d.querySelector('form'), async b => {
    await api(endpoint(`accounts/${a.id}`), 'PATCH', {
      contact: b.contact,
      note: b.note,
      status: b.status,
      markup_pct: b.markup_pct?.trim() ? b.markup_pct.trim() : null,
      can_create_keys: b.can_create_keys === 'on',
    });
    d.close();
    notice('Account updated');
    done?.();
  }), 'wide');
}

/* Requests --------------------------------------------------------------------- */

// Rows seen in any request table, so details open without another fetch.
const requestIndex = new Map();
function remember(rows) {
  if (requestIndex.size > 2000) requestIndex.clear();
  rows.forEach(r => requestIndex.set(r.id, r));
}
const routeCell = r => `<span class="route"><span class="method">${esc(r.method)}</span><code>${esc(r.path)}</code></span>`;
function requestTable(rows, { compact = false } = {}) {
  remember(rows);
  const columns = [
    ['Time', r => when(r.created_at)],
    ['Request', r => `${routeCell(r)}${r.model ? `<div class="hint"><code>${esc(r.model)}</code></div>` : ''}`, 'wrap'],
  ];
  if (!compact) columns.push(['Key', r => `${keyLink(r.key_id, r.key_name, r.key_prefix)}${admin ? `<div class="hint">${accountLink(r.account_id, r.account_contact)}</div>` : ''}`, 'wrap']);
  columns.push(
    ['Status', r => `${requestPill(r)}${r.finished ? `<div class="hint">${esc(ms(r.duration_ms))}</div>` : ''}`],
    ['Tokens', r => `<span class="num">${count(r.tokens)}</span>`, 'end'],
    ['Billed', r => `<span class="num">${esc(usd(r.billed_usd))}</span>`, 'end'],
    ['', r => `<button type="button" class="small" data-request-details="${esc(r.id)}">Details</button>`, 'end'],
  );
  return table(columns, rows, { empty: ['No requests', 'Requests appear here as soon as a key is used.'] });
}

function requestDetails(r) {
  document.querySelector('dialog[data-request-dialog]')?.close();
  const usage = [
    ['Total tokens', count(r.tokens)],
    r.input_tokens > 0 && ['Input tokens', count(r.input_tokens)],
    r.cached_tokens > 0 && ['Cached input', count(r.cached_tokens)],
    r.output_tokens > 0 && ['Output tokens', count(r.output_tokens)],
    r.characters > 0 && ['Characters', count(r.characters)],
    r.audio_ms > 0 && ['Audio', esc(ms(r.audio_ms))],
    r.images > 0 && ['Images', count(r.images)],
  ];
  const d = modal('Request details', `
    <div class="request-hero">
      ${routeCell(r)}
      <div class="request-meta">${requestPill(r)}<span>${esc(date(r.created_at))}</span><span>${esc(ms(r.duration_ms))}</span></div>
    </div>
    ${r.error ? `<div class="callout danger"><strong>Error</strong><p>${esc(r.error)}</p></div>` : ''}
    <div class="tiles">
      ${metric('Billed', usd(r.billed_usd), r.estimated ? 'Tokens estimated: the provider reported no usage' : 'Deducted from the balance')}
      ${admin ? metric('Upstream cost', usd(r.upstream_usd), `Margin ${esc(usd(Number(r.billed_usd) - Number(r.upstream_usd)))}`) : ''}
      ${metric('Tokens', count(r.tokens), r.estimated ? 'Estimated' : 'Reported by the provider')}
    </div>
    <div class="detail-columns">
      <section class="detail-section"><h4>Request</h4>${facts([
        ['Request ID', copyable(r.id, 'Request ID')],
        ['Model', r.model ? `<code>${esc(r.model)}</code>` : '-'],
        ['Kind', esc(kindLabel(r.kind))],
        ['Key', `${keyLink(r.key_id, r.key_name, r.key_prefix)}${r.key_prefix ? ` <code>${esc(r.key_prefix)}...</code>` : ''}`],
        admin && ['Account', accountLink(r.account_id, r.account_contact)],
        ['Client IP', `<code>${esc(r.ip || '-')}</code>`],
      ])}</section>
      <section class="detail-section"><h4>Usage and transfer</h4>${facts([
        ...usage,
        ['Sent', esc(bytes(r.bytes_in))],
        ['Received', esc(bytes(r.bytes_out))],
      ])}</section>
    </div>
    <div class="actions section">
      ${r.key_id ? goButton('All requests from this key', 'requests', { key: r.key_id }) : ''}
      ${admin && r.account_id ? goButton('All requests from this account', 'requests', { account: r.account_id }) : ''}
    </div>`, null, 'wide');
  d.dataset.requestDialog = '';
  wireCopies(d);
}

/* API and MCP guidance ----------------------------------------------------------- */

const codeBlock = (text, label) => `<div class="code-block">${label ? `<div class="code-label">${esc(label)}</div>` : ''}<button type="button" class="small ghost" data-copy-code>Copy</button><pre><code>${esc(text)}</code></pre></div>`;

function snippetTabs(items) {
  return `<div class="snippets">
    <div class="snippet-tabs" role="tablist">${items.map((s, i) => `<button type="button" role="tab" data-snippet-tab="${i}" aria-selected="${i === 0}">${esc(s.label)}</button>`).join('')}</div>
    ${items.map((s, i) => `<div role="tabpanel" data-snippet-panel="${i}"${i ? ' hidden' : ''}>${codeBlock(s.code)}</div>`).join('')}
  </div>`;
}

function wireDocs(root) {
  root.querySelectorAll('[data-copy-code]').forEach(button => {
    button.onclick = () => copyText(button.parentElement.querySelector('code').textContent, 'Snippet').then(ok => ok && flashCopied(button));
  });
  root.querySelectorAll('.snippets').forEach(box => {
    const tabs = [...box.querySelectorAll('[data-snippet-tab]')];
    tabs.forEach(t => t.onclick = () => {
      tabs.forEach(o => o.setAttribute('aria-selected', String(o === t)));
      box.querySelectorAll('[data-snippet-panel]').forEach(p => { p.hidden = p.dataset.snippetPanel !== t.dataset.snippetTab; });
    });
  });
}

// Example model names for the snippets, chosen by what each model really does.
// `known` is false when no model list is available: placeholders are then shown.
function exampleModels(models = []) {
  const first = {};
  models.forEach(m => {
    const kind = modelKind(m);
    if (kind && !first[kind]) first[kind] = m.id;
  });
  return {
    chat: first.chat || 'your-model',
    tts: first.speech || 'your-tts-model',
    stt: first.transcription || 'your-stt-model',
    image: first.image || 'your-image-model',
    has: Object.fromEntries(Object.keys(CAPABILITIES).map(k => [k, Boolean(first[k])])),
    known: models.length > 0,
  };
}

function apiSnippets(m = exampleModels()) {
  const base = `${location.origin}/v1`;
  const all = [
    { label: 'cURL', code: `curl ${base}/chat/completions \\
  -H "Authorization: Bearer $RESELLER_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${m.chat}",
    "messages": [{"role": "user", "content": "Hello"}]
  }'` },
    { label: 'Python', code: `# pip install openai
import os
from openai import OpenAI

client = OpenAI(base_url="${base}", api_key=os.environ["RESELLER_KEY"])
reply = client.chat.completions.create(
    model="${m.chat}",
    messages=[{"role": "user", "content": "Hello"}],
)
print(reply.choices[0].message.content)` },
    { label: 'Node', code: `// npm install openai
import OpenAI from "openai";

const client = new OpenAI({ baseURL: "${base}", apiKey: process.env.RESELLER_KEY });
const reply = await client.chat.completions.create({
  model: "${m.chat}",
  messages: [{ role: "user", content: "Hello" }],
});
console.log(reply.choices[0].message.content);` },
    { label: 'Streaming', code: `curl -N ${base}/chat/completions \\
  -H "Authorization: Bearer $RESELLER_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{"model": "${m.chat}", "stream": true,
       "messages": [{"role": "user", "content": "Count to five"}]}'` },
    { label: 'Speech', code: `curl ${base}/audio/speech \\
  -H "Authorization: Bearer $RESELLER_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{"model": "${m.tts}", "input": "Hello there", "voice": "alloy"}' \\
  --output speech.mp3` },
    { label: 'Transcription', code: `curl ${base}/audio/transcriptions \\
  -H "Authorization: Bearer $RESELLER_KEY" \\
  -F model=${m.stt} \\
  -F file=@audio.mp3` },
    { label: 'Image', code: `curl ${base}/images/generations \\
  -H "Authorization: Bearer $RESELLER_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{"model": "${m.image}", "prompt": "A lighthouse at dusk", "size": "1024x1024"}'` },
  ];
  // With a known model list, only offer examples the account can actually run.
  const kinds = { cURL: 'chat', Python: 'chat', Node: 'chat', Streaming: 'chat', Speech: 'speech', Transcription: 'transcription', Image: 'image' };
  return all.filter(x => !m.known || m.has[kinds[x.label]]);
}

// OAuth: the client discovers everything from the URL and opens the consent page.
function mcpOauth() {
  const url = `${location.origin}/mcp`;
  return `<div class="card">
    <div class="card-head"><h3>Connect with OAuth</h3><span class="pill tone-ok">Recommended</span></div>
    <p>Add this URL as a remote MCP server. Your client opens a ${esc(brandName)} page in the browser, you sign in and approve, and no key is pasted into the client.</p>
    <p>${copyable(url, 'MCP endpoint')}</p>
    <ul class="doc-list">
      <li><strong>Claude</strong>: Settings, Connectors, Add custom connector.</li>
      <li><strong>ChatGPT</strong>: Settings, Connectors, add a custom connector (developer mode).</li>
      <li><strong>Cursor</strong>: Settings, MCP, add a server with this URL.</li>
      <li><strong>VS Code</strong>: run <em>MCP: Add Server</em> and choose HTTP.</li>
    </ul>
    <p class="hint">Disconnect an app any time under Connect in your workspace.</p>
  </div>`;
}

// `cliKey` is what the one-line command shows in place of the secret.
function mcpPanel(cliKey) {
  const url = `${location.origin}/mcp`;
  return `<div class="docs-split">
    <div class="card">
      <div class="card-head"><h3>Static Bearer key</h3><span class="hint">For clients without OAuth</span></div>
      <p>Send an API key with every request. Endpoint: <code>${esc(url)}</code></p>
      ${codeBlock(`claude mcp add --transport http reseller ${url} \\\n  --header "Authorization: Bearer ${cliKey}"`)}
      <p class="hint">Or add this to <code>.mcp.json</code>. Keep the key in an environment variable.</p>
      ${codeBlock(JSON.stringify({ mcpServers: { reseller: { type: 'http', url, headers: { Authorization: 'Bearer ${RESELLER_KEY}' } } } }, null, 2))}
    </div>
    <div class="card">
      <div class="card-head"><h3>Local bridge</h3><span class="hint">stdio</span></div>
      <p>Runs next to your agent. Secrets and the signup proof of work stay on your machine, and generated media is saved to disk.</p>
      ${codeBlock(`cargo install reseller --locked\nclaude mcp add reseller -- reseller mcp --url ${location.origin}`)}
      <p class="hint">Or run <code>reseller mcp --url ${esc(location.origin)}</code> from any MCP client that launches stdio servers.</p>
    </div>
  </div>`;
}

const ENDPOINT_DOCS = [
  ['POST', '/v1/chat/completions', 'Chat completions. Set "stream": true for server-sent events.'],
  ['POST', '/v1/responses', 'Responses API.'],
  ['POST', '/v1/completions', 'Legacy text completions.'],
  ['POST', '/v1/embeddings', 'Text embeddings.'],
  ['POST', '/v1/moderations', 'Content moderation.'],
  ['POST', '/v1/audio/speech', 'Text to speech. Returns audio.'],
  ['POST', '/v1/audio/transcriptions', 'Speech to text. Multipart file upload.'],
  ['POST', '/v1/audio/translations', 'Speech to English text. Multipart file upload.'],
  ['POST', '/v1/images/generations', 'Image generation from a prompt.'],
  ['POST', '/v1/images/edits', 'Image edits. Multipart upload.'],
  ['POST', '/v1/images/variations', 'Image variations. Multipart upload.'],
  ['GET', '/v1/realtime', 'Realtime API over WebSocket.'],
  ['GET', '/v1/models', 'Models your key can use, in the OpenAI format.'],
  ['GET', '/v1/models/<id>', 'One model.'],
];
const ACCOUNT_DOCS = [
  ['GET', '/v1/account', 'Balance, debt, plan, credit lots, and top-up instructions.'],
  ['GET', '/v1/account/models', 'Models of the signed-in key. Also works with a dashboard token.'],
  ['GET', '/v1/account/usage?days=30', 'Daily usage per model (1 to 365 days). Add key=<id> for one key.'],
  ['GET', '/v1/account/requests?limit=20', 'Request history. Filters: key, model, kind, status (2xx, 4xx, 5xx).'],
  ['GET', '/v1/keys', 'Your keys with lifetime and 30-day spend.'],
  ['POST', '/v1/keys', 'Create another key on the same balance. Body {"name"}.'],
  ['GET', '/v1/keys/<id>', 'One key with its 30-day usage.'],
  ['PATCH', '/v1/keys/<id>', 'Rename or block a key.'],
  ['POST', '/v1/keys/<id>/rotate', 'Replace the secret. The old key stops working.'],
  ['POST', '/v1/account/redeem', 'Redeem a credit code. Body {"code"}.'],
  ['GET', '/v1/plans', 'Public plan catalog and enabled payment methods. No credential needed.'],
  ['POST', '/v1/account/checkout', 'Start a card or crypto purchase. Body {"plan_code", "method"}.'],
  ['GET', '/v1/account/payments', 'Your orders. Add /<id> for one order.'],
  ['GET', '/v1/account/apps', 'OAuth apps connected to your account.'],
  ['DELETE', '/v1/account/apps/<id>', 'Disconnect an app. Its access stops at once.'],
];
const ERROR_DOCS = [
  ['400', 'invalid_request_error', '-', 'The request body or parameters are invalid.'],
  ['401', 'invalid_request_error', 'missing_api_key, invalid_api_key', 'No key, or an unknown key, was sent.'],
  ['401', 'invalid_request_error', 'key_revoked, key_blocked, key_expired', 'The key exists but cannot be used.'],
  ['401', 'invalid_request_error', 'dashboard_token_not_allowed', 'Dashboard tokens manage the account. Use the API key for API calls.'],
  ['403', 'permission_error', 'account_suspended', 'The operator suspended the account.'],
  ['404', 'invalid_request_error', 'model_not_found', 'The model does not exist or the key may not use it.'],
  ['404', 'invalid_request_error', 'unknown_url', 'The path is not an API route.'],
  ['429', 'insufficient_quota', 'insufficient_quota', 'The balance is empty. The message explains how to top up.'],
  ['429', 'rate_limit_error', 'rate_limit_exceeded, request_limit_exceeded, token_limit_exceeded, spend_limit_exceeded, concurrency_limit_exceeded', 'A key or account limit was reached. Wait for Retry-After seconds.'],
  ['502, 504', 'server_error', 'upstream_error', 'The model provider failed or timed out. Nothing is billed for failed requests without usage.'],
];
const MCP_TOOL_DOCS = [
  ['generate_text', 'API key', 'Generate text with a chat model.'],
  ['generate_speech', 'API key', 'Convert text to speech.'],
  ['transcribe_audio', 'API key', 'Transcribe audio to text.'],
  ['generate_image', 'API key', 'Generate images from a prompt.'],
  ['list_models', 'API key', 'Models the key may use.'],
  ['account_overview', 'API key or token', 'Balance, debt, plan, and credit lots.'],
  ['usage_summary', 'API key or token', 'Spending and tokens over the last N days.'],
  ['list_requests', 'API key or token', 'Request history with billed cost.'],
  ['list_keys', 'API key or token', 'Key names and prefixes, never secrets.'],
  ['create_key', 'API key or token', 'Create another key on the account.'],
  ['rotate_key', 'API key or token', 'Replace a key with a new secret.'],
  ['block_key', 'API key or token', 'Block a key.'],
  ['redeem_code', 'API key or token', 'Redeem a single-use credit code.'],
  ['start_checkout', 'API key or token', 'Start a card or crypto purchase.'],
  ['list_plans', 'None', 'The public plan catalog.'],
  ['signup_challenge', 'None', 'Get a proof-of-work challenge for a new account.'],
  ['create_account', 'None', 'Create an account from a solved challenge.'],
];
// Connected OAuth apps (MCP clients that were approved on the consent page).
function appsCard(apps) {
  const body = apps.error
    ? `<div class="callout warn"><p>Connected apps could not be loaded: ${esc(apps.error)}</p></div>`
    : table([
      ['App', a => `<strong>${esc(a.client_name || 'Unnamed app')}</strong>`, 'wrap'],
      ['Connected', a => when(a.created_at)],
      ['Last used', a => when(a.last_used_at)],
      ['', a => `<button type="button" class="small danger" data-revoke-app="${esc(a.id)}" data-app-name="${esc(a.client_name || 'this app')}">Disconnect</button>`, 'end'],
    ], apps, { empty: ['No connected apps', 'Apps you approve through the sign-in page in your browser appear here.'] });
  return `<div class="card"><div class="card-head"><h3>Connected apps</h3><span class="hint">${apps.error ? '' : esc(plural(apps.length, 'app'))}</span></div>${body}</div>`;
}
const methodTag = m => `<span class="method method-${esc(m.toLowerCase())}">${esc(m)}</span>`;
const endpointTable = rows => table([
  ['Method', r => methodTag(r[0])],
  ['Path', r => `<code>${esc(r[1])}</code>`, 'wrap'],
  ['Description', r => esc(r[2]), 'wrap'],
], rows);

// What a model does, as reported by the server (`architecture.capability`). When a
// server predates that field, the modalities are used. Anything else is unknown.
const CAPABILITIES = {
  chat: ['Chat', 'POST /v1/chat/completions'],
  speech: ['Text-to-speech', 'POST /v1/audio/speech'],
  transcription: ['Speech-to-text', 'POST /v1/audio/transcriptions'],
  image: ['Image', 'POST /v1/images/generations'],
};
function modelKind(m) {
  const a = m.architecture || {};
  if (CAPABILITIES[a.capability]) return a.capability;
  const input = a.input_modalities || [], output = a.output_modalities || [];
  if (output.includes('image')) return 'image';
  if (output.includes('audio')) return 'speech';
  if (input.includes('audio') && output.includes('text')) return 'transcription';
  return input.includes('text') && output.includes('text') ? 'chat' : null;
}
const modalityText = list => list?.length ? list.map(x => x.charAt(0).toUpperCase() + x.slice(1)).join(', ') : DASH;
// Prices arrive per unit in USD. Show them in the unit people compare.
const perMillion = v => `$${Number((Number(v) * 1e6).toPrecision(4)).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 4 })}`;
const perUnit = v => `$${Number(Number(v).toPrecision(4)).toLocaleString(undefined, { maximumFractionDigits: 6 })}`;
function modelPrice(m, kind) {
  const p = m.pricing;
  if (!p) return DASH;
  const parts = [];
  if (kind === 'chat' || !kind) {
    if (p.prompt && p.completion) parts.push(`${perMillion(p.prompt)} in / ${perMillion(p.completion)} out per 1M tokens`);
  } else if (kind === 'speech') {
    if (isPositive(p.characters)) parts.push(`${perMillion(p.characters)} per 1M characters`);
    if (isPositive(p.audio_second)) parts.push(`${perUnit(Number(p.audio_second) * 60)} per audio minute`);
  } else if (kind === 'transcription') {
    if (isPositive(p.audio_second)) parts.push(`${perUnit(Number(p.audio_second) * 60)} per audio minute`);
  } else if (kind === 'image' && isPositive(p.image)) {
    parts.push(`${perUnit(p.image)} per image`);
  }
  if (isPositive(p.request)) parts.push(`${perUnit(p.request)} per request`);
  return parts.length ? parts.map(esc).join('<br>') : DASH;
}
function modelCatalog(models) {
  if (!models.length) return emptyState('No models listed', 'The operator has not published a model catalog.');
  const groups = Object.keys(CAPABILITIES).map(kind => [kind, models.filter(m => modelKind(m) === kind)]);
  const other = models.filter(m => !modelKind(m));
  if (other.length) groups.push(['', other]);
  return `<div class="model-groups">${groups.filter(([, list]) => list.length).map(([kind, list]) => `<section class="model-group" aria-label="${esc(kind ? CAPABILITIES[kind][0] : 'Other')} models">
    <div class="model-group-head"><span class="cap-badge cap-${kind || 'other'}">${esc(kind ? CAPABILITIES[kind][0] : 'Other')}</span>${kind ? `<code>${esc(CAPABILITIES[kind][1])}</code>` : '<span class="hint">Capability not reported</span>'}<span class="hint">${esc(plural(list.length, 'model'))}</span></div>
    ${table([
      ['Model', m => `<div class="model-cell">${m.name && m.name !== m.id ? `<strong>${esc(m.name)}</strong>` : ''}<div>${copyable(m.id, 'Model ID')}</div>${m.description ? `<div class="hint">${esc(m.description)}</div>` : ''}</div>`, 'wrap'],
      ['Input', m => modalityText(m.architecture?.input_modalities)],
      ['Output', m => modalityText(m.architecture?.output_modalities)],
      ['Context', m => m.context_length ? `<span><span class="num">${count(m.context_length)}</span> <span class="hint">tokens</span></span>` : DASH, 'end'],
      ['Price', m => modelPrice(m, kind), 'wrap'],
    ], list)}</section>`).join('')}</div>`;
}

/* Docs page ---------------------------------------------------------------------- */

const DOC_SECTIONS = [
  ['quick-start', 'Quick start'],
  ['authentication', 'Authentication'],
  ['models', 'Models'],
  ['endpoints', 'Endpoints'],
  ['examples', 'Examples'],
  ['billing', 'Billing and balance'],
  ['errors', 'Errors and limits'],
  ['account-api', 'Account API'],
  ['mcp', 'MCP for agents'],
];

const quickStartCode = (base, model) => codeBlock(`export RESELLER_KEY="sk-res-..."\ncurl ${base}/chat/completions \\\n  -H "Authorization: Bearer $RESELLER_KEY" \\\n  -H "Content-Type: application/json" \\\n  -d '{"model": "${model}", "messages": [{"role": "user", "content": "Hello"}]}'`, 'Terminal');

async function docs() {
  // Same-tab workspace session, if any, to show the caller's own models.
  const session = sessionStorage.getItem(CUSTOMER_STORE) || '';
  const base = `${location.origin}/v1`;
  const m = exampleModels();
  const modelsSlot = session
    ? skeleton(skTable(5, 4), 'Loading your models')
    : `<div class="callout info"><strong>Your model list</strong><p>Sign in to the <a href="/workspace/">workspace</a> in this tab to see the models your key can use here, or call the endpoint below.</p></div>`;
  main.innerHTML = `
    <div class="docs-layout">
      <nav class="docs-nav" aria-label="Documentation">
        <button type="button" class="docs-nav-toggle" aria-expanded="false" aria-controls="docs-toc"><span>On this page</span><strong data-toc-current>${esc(DOC_SECTIONS[0][1])}</strong></button>
        <div class="eyebrow">On this page</div>
        <ol id="docs-toc">${DOC_SECTIONS.map(([id, label]) => `<li><a href="#${id}" data-doc-link="${id}">${esc(label)}</a></li>`).join('')}</ol>
      </nav>
      <article class="docs-body">
        <header class="docs-hero">
          <div class="docs-hero-main">
            <div class="eyebrow">${esc(brandName)} API</div>
            <h1>Developer docs</h1>
            <p class="lead">One OpenAI-compatible API for text, speech, transcription, and images. Change the base URL and key, keep your code.</p>
            <div class="actions">${session ? '<a class="button primary" href="/workspace/#keys">API keys</a>' : '<button type="button" class="primary" data-create>Get an API key</button><a class="button ghost" href="/workspace/">Sign in</a>'}<a class="button ghost" href="#quick-start">Quick start</a></div>
          </div>
          <div class="connect-card" role="group" aria-label="Connection details">
            <div class="connect-row"><span class="label">Base URL</span>${copyable(base, 'Base URL')}</div>
            <div class="connect-row"><span class="label">MCP URL</span>${copyable(`${location.origin}/mcp`, 'MCP endpoint')}</div>
            <div class="connect-row"><span class="label">Auth header</span>${copyable('Authorization: Bearer $RESELLER_KEY', 'Auth header')}</div>
          </div>
        </header>

        <section id="quick-start" class="doc-section">
          <h2>Quick start</h2>
          <ol class="start-steps">
            <li class="card"><span class="step-num">1</span><div><h3>Get a key</h3><p>${session ? 'Create one in the workspace under API keys.' : 'Create an account. A short proof of work runs in your browser, no card needed.'}</p></div></li>
            <li class="card"><span class="step-num">2</span><div><h3>Export it</h3><p>Store it as <code>RESELLER_KEY</code> in your shell or secret store.</p></div></li>
            <li class="card"><span class="step-num">3</span><div><h3>Call the API</h3><p>Point your SDK at the base URL and send a request.</p></div></li>
          </ol>
          <div id="doc-quick">${quickStartCode(base, m.chat)}</div>
        </section>

        <section id="authentication" class="doc-section">
          <h2>Authentication</h2>
          <p>Every API call needs a credential, sent as a bearer token: <code>Authorization: Bearer sk-res-...</code>. The <code>x-api-key</code> header works as well.</p>
          ${facts([
            ['API key', 'Starts with <code>sk-res-</code>. Calls the API and the MCP generation tools. Shown once when created or rotated.'],
            ['Dashboard token', 'Starts with <code>dash_</code>. Signs in to the workspace and manages the account (keys, usage, billing, MCP account tools). It cannot call generation endpoints.'],
            ['Several keys', 'All keys of an account share one balance. Create one key per app to see spend per app.'],
            ['Administrator token', 'The operator token is also accepted on <code>/v1</code> and <code>/mcp</code> as an unbilled operator credential. Nothing is charged to any account.'],
          ])}
          <p class="hint">Lost a key? Rotate it under API keys. The old secret stops working at once.</p>
        </section>

        <section id="models" class="doc-section">
          <h2>Models</h2>
          <p>Send the model ID in the <code>model</code> field. <code>GET /v1/models</code> lists the models your key may use, in the OpenAI format.</p>
          <div id="doc-models">${modelsSlot}</div>
          ${codeBlock(`curl ${base}/models -H "Authorization: Bearer $RESELLER_KEY"`)}
        </section>

        <section id="endpoints" class="doc-section">
          <h2>Endpoints</h2>
          <p>Requests and responses follow the OpenAI API, including streaming. Upstream errors keep the OpenAI error shape.</p>
          ${endpointTable(ENDPOINT_DOCS)}
          <p class="hint"><a href="/v1">GET /v1</a> returns a machine-readable route list.</p>
        </section>

        <section id="examples" class="doc-section">
          <h2>Examples</h2>
          <p>Each example reads the key from <code>$RESELLER_KEY</code><span id="doc-examples-note"></span>.</p>
          <div id="doc-examples">${snippetTabs(apiSnippets(m))}</div>
        </section>

        <section id="billing" class="doc-section">
          <h2>Billing and balance</h2>
          <ul class="doc-list">
            <li>Usage is prepaid. Each request is priced from its reported usage (tokens, characters, audio seconds, or images) and deducted from the account balance.</li>
            <li>Credit comes from plans, credit codes, card or crypto payments, and the trial grant. Credit that expires soonest is spent first.</li>
            <li>A request that runs past the remaining balance completes. The difference is recorded as debt and settled from the next top-up.</li>
            <li>When the balance is empty, calls return <code>429 insufficient_quota</code> with top-up steps in the message.</li>
            <li>Every charge is listed per request in the workspace under Requests, and <code>GET /v1/account</code> returns the balance.</li>
          </ul>
        </section>

        <section id="errors" class="doc-section">
          <h2>Errors and limits</h2>
          <p>Errors use the OpenAI shape: <code>{"error": {"message", "type", "param", "code"}}</code>. Every <code>429</code> carries a <code>Retry-After</code> header in seconds.</p>
          ${table([
            ['Status', r => `<strong class="num">${esc(r[0])}</strong>`],
            ['Type', r => `<code>${esc(r[1])}</code>`],
            ['Code', r => r[2] === '-' ? '<span class="muted">-</span>' : r[2].split(', ').map(c => `<code>${esc(c)}</code>`).join(' '), 'wrap'],
            ['Meaning', r => esc(r[3]), 'wrap'],
          ], ERROR_DOCS)}
        </section>

        <section id="account-api" class="doc-section">
          <h2>Account API</h2>
          <p>Manage the account from code with the API key or the dashboard token.</p>
          ${endpointTable(ACCOUNT_DOCS)}
        </section>

        <section id="mcp" class="doc-section">
          <h2>MCP for agents</h2>
          <p>Agents connect over the Model Context Protocol (Streamable HTTP, POST only). Generation tools are billed like API calls.</p>
          <h3 class="docs-title" id="mcp-oauth">Connect with OAuth</h3>
          <p>Most clients only need the URL. Paste <code>${esc(location.origin)}/mcp</code> into Claude, ChatGPT, Cursor, or VS Code as a remote MCP server. The client opens a consent page in your browser. Sign in with an API key or dashboard token, or create an account, and choose Allow. Connected apps are listed in the workspace under Connect, where you can revoke them.</p>
          ${mcpOauth()}
          <h3 class="docs-title" id="mcp-static">Static Bearer key and local bridge</h3>
          ${mcpPanel('$RESELLER_KEY')}
          <div class="callout info"><strong>Operator credential</strong><p>The administrator token also works on <code>/v1</code> and <code>/mcp</code> as an unbilled operator credential. Use it for testing and support, not for customers.</p></div>
          <h3 class="docs-title" id="mcp-tools">Tools</h3>
          ${table([['Tool', r => `<code>${esc(r[0])}</code>`], ['Credential', r => esc(r[1])], ['Description', r => esc(r[2]), 'wrap']], MCP_TOOL_DOCS)}
        </section>
      </article>
    </div>`;
  wireDocs(main);
  wireCopies(main);
  main.querySelector('[data-create]')?.addEventListener('click', keyForm);
  // Highlight the section in view.
  const links = new Map([...main.querySelectorAll('[data-doc-link]')].map(a => [a.dataset.docLink, a]));
  const toc = main.querySelector('.docs-nav');
  const toggle = toc.querySelector('.docs-nav-toggle');
  toggle.onclick = () => {
    const open = toc.classList.toggle('open');
    toggle.setAttribute('aria-expanded', String(open));
  };
  toc.addEventListener('click', e => {
    if (!e.target.closest('a')) return;
    toc.classList.remove('open');
    toggle.setAttribute('aria-expanded', 'false');
  });
  const observer = new IntersectionObserver(entries => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      links.forEach((a, id) => id === entry.target.id ? a.setAttribute('aria-current', 'location') : a.removeAttribute('aria-current'));
      toc.querySelector('[data-toc-current]').textContent = links.get(entry.target.id)?.textContent || '';
    }
  }, { rootMargin: '-15% 0px -70% 0px' });
  main.querySelectorAll('.doc-section').forEach(s => observer.observe(s));
  if (location.hash) document.getElementById(location.hash.slice(1))?.scrollIntoView();
  if (!session) return;
  // The model list arrives after the page: fill its slots in place.
  const slot = main.querySelector('#doc-models');
  slot.setAttribute('aria-busy', 'true');
  try {
    const models = (await api('/v1/account/models', 'GET', undefined, { token: session })).data || [];
    const ex = exampleModels(models);
    slot.innerHTML = modelCatalog(models);
    main.querySelector('#doc-examples').innerHTML = snippetTabs(apiSnippets(ex));
    main.querySelector('#doc-examples-note').textContent = ' and uses a model from your list';
    main.querySelector('#doc-quick').innerHTML = quickStartCode(base, ex.chat);
    wireDocs(main);
    wireCopies(main);
  } catch {
    slot.innerHTML = `<div class="callout info"><strong>Your model list</strong><p>Your workspace session expired. <a href="/workspace/">Sign in again</a> to list your models here.</p></div>`;
  } finally {
    slot.removeAttribute('aria-busy');
  }
}

/* Landing and sign-in ------------------------------------------------------------ */

async function landing() {
  main.innerHTML = `
    <section class="hero">
      <div>
        <div class="eyebrow">${esc(brand.hero_eyebrow || 'ONE ENDPOINT, ENDLESS POSSIBILITIES')}</div>
        <h1>${brand.hero_title ? esc(brand.hero_title) : 'Intelligence,<br>on your terms<span class="accent">.</span>'}</h1>
        <p>${esc(brand.hero_text || 'Language, speech, audio, and images through one familiar API. Create a key, connect your app, and stay in control of every request.')}</p>
        <div class="actions section">
          <button type="button" class="primary" id="create">Get an API key</button>
          <a class="button ghost" href="/docs/">Read the docs</a>
        </div>
        <ul class="chips" aria-label="Highlights"><li>OpenAI-compatible</li><li>Prepaid, pay as you go</li><li>MCP ready</li><li>Per-request history</li></ul>
      </div>
      <div class="terminal">
        <div class="bar">QUICK START / cURL</div>
        <pre><span class="accent">curl</span> ${esc(location.origin)}/v1/chat/completions \\
  -H "Authorization: Bearer $RESELLER_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "your-model",
    "messages": [{
      "role": "user",
      "content": "Build something great."
    }],
    "stream": true
  }'</pre>
      </div>
    </section>
    <section class="section get-started" id="get-started">
      <div class="eyebrow">GET STARTED</div>
      <h2>Your first request in three steps</h2>
      <ol class="start-steps">
        <li class="card"><span class="step-num">1</span><div><h3>Create a key</h3><p>Choose Get an API key. A short proof of work runs in your browser. No card needed.</p></div></li>
        <li class="card"><span class="step-num">2</span><div><h3>Call the API</h3><p>Point any OpenAI SDK at <code>${esc(location.origin)}/v1</code> and send the key as a bearer token.</p></div></li>
        <li class="card"><span class="step-num">3</span><div><h3>Connect an agent</h3><p>Paste <code>${esc(location.origin)}/mcp</code> into Claude, ChatGPT, Cursor, or VS Code and approve in your browser.</p></div></li>
      </ol>
      <div class="card">
        <div class="card-head"><h3>Call the API</h3><a class="button ghost small" href="/docs/#examples">All examples</a></div>
        ${snippetTabs(apiSnippets())}
      </div>
    </section>
    <section class="section">
      <div class="row">
        <div><div class="eyebrow">ROOM TO BUILD</div><h2>Choose your credit plan</h2></div>
        <span class="muted">Simple, prepaid access.</span>
      </div>
      <div class="section" id="plans" aria-busy="true">${plansSkeleton()}</div>
    </section>
    <section class="steps section">
      <div class="card step"><div class="eyebrow">01 / CONNECT ONCE</div><p>Use your existing OpenAI SDK. Change the base URL and API key.</p></div>
      <div class="card step"><div class="eyebrow">02 / SEE EVERY REQUEST</div><p>Track tokens, request history, and spending in your workspace.</p></div>
      <div class="card step"><div class="eyebrow">03 / STAY IN CONTROL</div><p>Rotate keys, add credit, and keep your applications running.</p></div>
    </section>`;
  document.querySelector('#create').onclick = keyForm;
  wireDocs(main);
  const slot = document.querySelector('#plans');
  try {
    const v = await api('/v1/plans');
    slot.innerHTML = pricingSection(v.plans, v.payment_methods || []);
  } catch (e) {
    slot.innerHTML = `<div class="card error"><strong>Plans could not be loaded</strong><p>${esc(e.message)}</p></div>`;
  } finally {
    slot.removeAttribute('aria-busy');
  }
}

function login() {
  const intro = admin
    ? `<section class="auth-intro">
        <div class="eyebrow">OPERATOR CONSOLE</div>
        <h1>Run the<br>platform<span class="accent">.</span></h1>
        <p class="muted">Manage accounts, plans, redeem codes, payments, and every request from one place.</p>
      </section>`
    : `<section class="auth-intro">
        <div class="eyebrow">YOUR API WORKSPACE</div>
        <h1>Your keys.<br>Your usage.<span class="accent"> One balance.</span></h1>
        <p class="muted">Sign in with an API key or dashboard token to manage keys, redeem credit, and watch every request.</p>
        <ul class="auth-points">
          <li>Create, block, and rotate keys</li>
          <li>Track tokens, spend, and history</li>
          <li>Redeem codes and prepaid plans</li>
        </ul>
      </section>`;
  main.innerHTML = `
    <div class="auth">
      ${intro}
      <section class="auth-panel">
        <form class="card auth-card">
          <div class="eyebrow">${esc(admin ? `${brandName} administrator` : `Sign in to ${brandName}`).toUpperCase()}</div>
          <h2>Welcome back<span class="accent">.</span></h2>
          <label for="login-token">${admin ? 'Administrator token' : 'API key or dashboard token'}</label>
          <input id="login-token" name="token" type="password" autocomplete="off" required>
          <p class="field-help">Credentials stay in this browser tab for the session.</p>
          <div class="actions full"><button class="primary" type="submit">Open ${admin ? 'console' : 'workspace'}</button></div>
        </form>
        ${admin ? '' : `
        <div class="auth-alt">
          <div>
            <div class="label">New here?</div>
            <div class="hint">Create an account and get evaluation credit. No card required.</div>
          </div>
          <button type="button" id="create" class="button ghost">Create a new account</button>
        </div>`}
      </section>
    </div>`;
  bindForm(main.querySelector('form'), async b => {
    token = b.token.trim();
    try {
      await api(endpoint(admin ? 'overview' : 'account'));
    } catch (e) {
      token = '';
      throw e;
    }
    sessionStorage.setItem(store, token);
    renderHeader();
    await shell();
  });
  document.querySelector('#create')?.addEventListener('click', keyForm);
}

/* Shell -------------------------------------------------------------------------- */

function selectTab() {
  const content = main.querySelector('#content');
  main.querySelectorAll('[data-tab]').forEach(t => {
    const selected = t.dataset.tab === tab;
    t.setAttribute('aria-selected', String(selected));
    t.tabIndex = selected ? 0 : -1;
    if (selected) {
      content?.setAttribute('aria-labelledby', t.id);
      t.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    }
  });
}

async function shell() {
  applyRoute();
  if (location.hash.slice(1).split('?')[0] !== tab) history.replaceState(null, '', hashFor(tab));
  main.innerHTML = `
    <div class="tabs" role="tablist" aria-label="${admin ? 'Console sections' : 'Workspace sections'}">
      ${TABS.map(t => `<button type="button" role="tab" id="tab-${t}" data-tab="${t}" aria-controls="content" aria-selected="${t === tab}" tabindex="${t === tab ? 0 : -1}">${esc(TAB_LABELS[t])}</button>`).join('')}
    </div>
    <section id="content" role="tabpanel" aria-busy="true" data-view="${tab}">${viewSkeleton(tab)}</section>`;
  const tabButtons = [...main.querySelectorAll('[data-tab]')];
  selectTab();
  tabButtons.forEach((b, i) => {
    b.onclick = () => {
      if (tab !== b.dataset.tab) go(b.dataset.tab);
    };
    // Arrow/Home/End keys move between tabs (roving tabindex pattern).
    b.onkeydown = e => {
      const target = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabButtons.length - 1 }[e.key];
      if (target === undefined) return;
      e.preventDefault();
      const next = tabButtons[(target + tabButtons.length) % tabButtons.length];
      next.focus();
      next.click();
    };
  });
  window.onhashchange = () => {
    applyRoute();
    selectTab();
    load();
  };
  await load();
}

let generation = 0;
let loadController;
async function load() {
  const gen = ++generation;
  loadController?.abort();
  const controller = new AbortController();
  loadController = controller;
  const content = document.querySelector('#content');
  if (!content) return; // signed out or navigated away
  content.setAttribute('aria-busy', 'true');
  // Switching views swaps in a skeleton shaped like the next view. Refreshing the
  // same view keeps its content (dimmed) so nothing jumps.
  if (!content.children.length || content.dataset.view !== tab) content.innerHTML = viewSkeleton(tab);
  content.dataset.view = tab;
  try {
    const html = await views[tab](controller.signal);
    if (gen !== generation) return;
    content.innerHTML = html;
    wire(content);
  } catch (e) {
    if (e.name === 'AbortError' || gen !== generation) return;
    delete content.dataset.view;
    content.innerHTML = `<div class="card error"><strong>Something went wrong</strong><p>${esc(e.message)}</p><div class="actions"><button type="button" data-refresh class="ghost">Try again</button></div></div>`;
    wire(content);
  } finally {
    if (gen === generation) content.removeAttribute('aria-busy');
  }
}

const pageHead = (title, subtitle, actions = '') => `<div class="page-head"><div><h2>${esc(title)}</h2>${subtitle ? `<p class="muted">${subtitle}</p>` : ''}</div>${actions ? `<div class="actions">${actions}</div>` : ''}</div>`;

/* Views -------------------------------------------------------------------------- */

const views = {
  async overview(signal) {
    if (admin) {
      const [v, u, topKeys, topAccounts] = await Promise.all([
        get(endpoint('overview'), signal),
        get(endpoint('usage'), signal),
        get(endpoint('keys?sort=usage&dir=desc&limit=5'), signal),
        get(endpoint('accounts?sort=usage&dir=desc&limit=5'), signal),
      ]);
      const billed = Number(v.billed_cost_usd);
      const top = (rows, label, link, value) => rows.filter(r => isPositive(value(r))).length
        ? `<ol class="leaderboard">${rows.filter(r => isPositive(value(r))).map(r => `<li><span class="leader-name">${link(r)}</span><span class="num">${esc(usd(value(r)))}</span></li>`).join('')}</ol>`
        : `<p class="hint">No ${label} spent anything in the last 30 days.</p>`;
      return `${pageHead('Overview', 'Platform totals since launch. Charts and rankings cover the last 30 days.', '<button type="button" data-refresh class="ghost small">Refresh</button>')}
        <div class="stats">
          ${metric('API revenue', usd(v.billed_cost_usd), `${esc(count(v.requests))} requests, ${esc(count(v.tokens))} tokens`)}
          ${metric('Upstream cost', usd(v.upstream_cost_usd), 'Paid to model providers')}
          ${metric('Gross margin', usd(v.margin_usd), `${percent(Number(v.margin_usd), billed)} of revenue`)}
          ${metric('Outstanding credit', usd(v.outstanding_usd), isPositive(v.suspended_credit_usd) ? `Spendable by active accounts. ${esc(usd(v.suspended_credit_usd))} more is held by suspended accounts.` : 'Unexpired credit active accounts can spend')}
        </div>
        <div class="card">
          <div class="card-head"><h3>Usage</h3>${goButton('Request log', 'requests')}</div>
          ${chart(u.series)}
        </div>
        <div class="stats">
          ${metric('Accounts', count(v.accounts), Number(v.suspended_accounts) ? `${esc(count(v.suspended_accounts))} suspended` : 'None suspended')}
          ${metric('Usable keys', count(v.active_keys), 'Active, unexpired, on active accounts')}
          ${metric('Customer debt', usd(v.debt_usd), 'Spend not yet covered by credit', { tone: isPositive(v.debt_usd) ? 'warn' : '' })}
        </div>
        <div class="split-even">
          <div class="card">
            <div class="card-head"><h3>Top keys, 30 days</h3>${goButton('All keys', 'keys', { sort: 'usage' })}</div>
            ${top(topKeys.keys, 'key', k => `${keyLink(k.id, k.name)}<span class="hint">${esc(accountLabel(k.account_id, k.account_contact))}</span>`, k => k.usage_30d_usd)}
          </div>
          <div class="card">
            <div class="card-head"><h3>Top accounts, 30 days</h3>${goButton('All accounts', 'accounts', { sort: 'usage' })}</div>
            ${top(topAccounts.accounts, 'account', a => accountLink(a.id, a.contact), a => a.usage_30d_usd)}
          </div>
        </div>
        <div class="card docs-note">
          <div class="card-head"><h3>Customer docs</h3><a class="button ghost small" href="/docs/">Open docs</a></div>
          <p>Customers connect through the OpenAI-compatible API at <code>${esc(location.origin)}/v1</code> and MCP at <code>${esc(location.origin)}/mcp</code>. The public docs page lists endpoints, examples, errors, and billing rules.</p>
        </div>`;
    }
    const [v, u, r] = await Promise.all([get('/v1/account', signal), get('/v1/account/usage', signal), get('/v1/account/requests?limit=5', signal)]);
    const a = v.account;
    const sub = a.subscription;
    const t = seriesTotals(u.series);
    const activeLots = v.credits.filter(c => lotState(c) === 'active');
    const empty = !isPositive(a.balance_usd);
    return `<div class="banner${empty ? ' warn' : ''}">
        <div>
          <div class="label">Available balance</div>
          <div class="metric">${esc(usd(a.balance_usd))}</div>
          <div class="hint">${isPositive(a.debt_usd) ? `Outstanding debt ${esc(usd(a.debt_usd))} settles from the next top-up.` : empty ? 'API calls are refused until you add credit.' : 'Shared by every key on this account.'}</div>
        </div>
        <div class="actions">${goButton('Add credit', 'billing', {}, empty ? 'primary' : 'ghost')}<button type="button" class="${empty ? 'ghost' : 'primary'}" data-create>Create key</button></div>
      </div>
      <div class="stats">
        ${metric('Spent, all time', usd(a.billed_usd))}
        ${metric('Last 30 days', usd(t.cost), esc(`${plural(t.requests, 'request')}, ${count(t.tokens)} tokens`))}
        ${metric('Plan', sub ? sub.plan_code : 'None', sub ? `Until ${esc(date(sub.expires_at))}` : 'Pay as you go')}
        ${metric('Active credit lots', count(activeLots.length), 'Soonest-expiring credit is spent first')}
      </div>
      <div class="card">
        <div class="card-head"><h3>Your usage</h3>${goButton('Usage details', 'usage')}</div>
        ${chart(u.series)}
      </div>
      <div class="card">
        <div class="card-head"><h3>Recent requests</h3>${goButton('All requests', 'requests')}</div>
        ${requestTable(r.requests, { compact: true })}
      </div>`;
  },
  async connect(signal) {
    let prefix = token.startsWith('sk-res-') ? token.slice(0, 15) : '';
    const [keys, models, apps] = await Promise.all([
      prefix ? Promise.resolve([]) : get('/v1/keys?limit=50', signal).then(v => v.keys || []).catch(e => { if (e.name === 'AbortError') throw e; return []; }),
      get('/v1/account/models', signal).then(v => v.data || []).catch(e => { if (e.name === 'AbortError') throw e; return []; }),
      // Older servers have no connected-apps route: hide the section instead of failing the page.
      get('/v1/account/apps', signal).then(v => v.apps || []).catch(e => { if (e.name === 'AbortError') throw e; return e.status === 404 ? null : { error: e.message }; }),
    ]);
    if (!prefix) prefix = (keys.find(x => x.status === 'active') || keys[0] || {}).key_prefix || '';
    const m = exampleModels(models);
    return `${pageHead('Connect', 'Everything an app or agent needs to call the API.', '<a class="button ghost small" href="/docs/">Full docs</a>')}
      <div class="card">
        <div class="card-head"><h3>Connection details</h3></div>
        ${facts([
          ['API base URL', `${copyable(`${location.origin}/v1`, 'API base URL')}<div class="hint">Works with any OpenAI-compatible SDK.</div>`],
          ['MCP endpoint', `${copyable(`${location.origin}/mcp`, 'MCP endpoint')}<div class="hint">Streamable HTTP, POST only.</div>`],
          ['API key', `${prefix ? `<code>${esc(prefix)}...</code>` : '<span class="muted">No active key</span>'}<div class="hint">Only the prefix is shown. The full secret appears once when a key is created. Lost it? Rotate the key under API keys.</div>`],
        ])}
      </div>
      <div class="card">
        <div class="card-head"><h3>Your models</h3><span class="hint">${esc(plural(models.length, 'model'))}</span></div>
        ${modelCatalog(models)}
      </div>
      <div class="card">
        <div class="card-head"><h3>Call the API</h3><span class="hint">Reads the key from <code>$RESELLER_KEY</code></span></div>
        ${snippetTabs(apiSnippets(m))}
      </div>
      <h3 class="docs-title">Connect an agent over MCP</h3>
      ${mcpOauth()}
      <div class="section-gap"></div>
      ${apps ? appsCard(apps) : ''}
      <h3 class="docs-title">Other ways to connect</h3>
      ${mcpPanel('$RESELLER_KEY')}`;
  },
  async keys(signal) {
    const state = listStates.keys;
    const params = listParams(state);
    if (admin && state.account) params.set('account', state.account);
    const v = await get(`${endpoint('keys')}?${params}`, signal);
    const columns = [
      ['Key', r => `${keyLink(r.id, r.name)}<div class="hint"><code>${esc(r.key_prefix)}...</code></div>`, 'wrap'],
      ['Status', r => `${pill(r.status)}${r.expires_at ? `<div class="hint">${r.expires_at < nowSeconds() ? 'expired' : 'expires'} ${esc(date(r.expires_at))}</div>` : ''}`],
    ];
    if (admin) columns.push(['Account', r => accountLink(r.account_id, r.account_contact), 'wrap']);
    columns.push(
      ['Spent', r => `<span class="num">${esc(usd(r.billed_usd))}</span>`, 'end'],
      ['30 days', r => `<span class="num">${esc(usd(r.usage_30d_usd))}</span>`, 'end'],
      ['Last used', r => when(r.last_used_at)],
    );
    columns.push(['', r => `<div class="actions">
        ${goButton('Requests', 'requests', { key: r.id }, 'small')}
        <button type="button" class="small" data-edit-key="${esc(r.id)}" data-key-name="${esc(r.name)}">${admin ? 'Edit' : 'Rename'}</button>
        <button type="button" class="small" data-rotate="${esc(r.id)}"${r.status !== 'active' ? ' disabled' : ''}>Rotate</button>
      </div>`, 'end']);
    const toolbar = listToolbar('keys', state, {
      placeholder: 'Search name, prefix, or ID',
      statuses: [['active', 'Active'], ['blocked', 'Blocked'], ['revoked', 'Revoked']],
      sorts: [['created', 'Created'], ['spend', 'Spent, all time'], ['usage', 'Spent, 30 days'], ['last_used', 'Last used'], ['name', 'Name'], ['status', 'Status'], ...(admin ? [['account', 'Account']] : [])],
      groups: [['status', 'Group by status'], ...(admin ? [['account', 'Group by account']] : [])],
    });
    const group = state.group === 'status' ? { value: r => r.status } : state.group === 'account' ? { value: r => accountLabel(r.account_id, r.account_contact) } : null;
    const scope = admin && state.account
      ? `<div class="filter-chips">${filterChip('keys', 'account', 'Account', esc(accountLabel(state.account, v.keys[0]?.account_contact)), `<button type="button" class="link" data-open-account="${esc(state.account)}">Open account</button>`)}</div>`
      : '';
    return `${pageHead('API keys', `${esc(plural(v.total, 'key'))}${admin ? '' : ' on this account'}. Open a key for its usage, limits, and requests.`, admin ? '' : '<button type="button" class="primary" data-create>Create key</button>')}
      ${toolbar}${scope}${table(columns, v.keys, { group, empty: ['No keys match', 'Change the filters or create a key.'] })}${listPager('keys', state, Number(v.total || 0), v.keys.length)}`;
  },
  async accounts(signal) {
    const state = listStates.accounts;
    const v = await get(`${endpoint('accounts')}?${listParams(state)}`, signal);
    const toolbar = listToolbar('accounts', state, {
      placeholder: 'Search account ID or contact',
      statuses: [['active', 'Active'], ['suspended', 'Suspended']],
      sorts: [['created', 'Created'], ['spend', 'Spent, all time'], ['usage', 'Spent, 30 days'], ['balance', 'Balance'], ['keys', 'Key count'], ['contact', 'Contact'], ['status', 'Status']],
      groups: [['status', 'Group by status'], ['plan', 'Group by plan']],
    });
    const group = state.group === 'status' ? { value: r => r.status } : state.group === 'plan' ? { value: r => r.plan_code || 'No active plan' } : null;
    return `${pageHead('Accounts', `${esc(plural(v.total, 'customer account'))}. Open one to manage credit, keys, and status.`)}
      ${toolbar}${table([
        ['Account', r => `${accountLink(r.id, r.contact)}<div class="hint"><code>${esc(shortId(r.id))}</code></div>`, 'wrap'],
        ['Status', r => pill(r.status)],
        ['Balance', r => `<strong class="num${Number(r.balance_usd) < 0 ? ' negative' : ''}">${esc(usd(r.balance_usd))}</strong>${isPositive(r.debt_usd) ? `<div class="hint warn">debt ${esc(usd(r.debt_usd))}</div>` : ''}`, 'end'],
        ['Spent', r => `<span class="num">${esc(usd(r.billed_usd))}</span>`, 'end'],
        ['30 days', r => `<span class="num">${esc(usd(r.usage_30d_usd))}</span>`, 'end'],
        ['Keys', r => `<span class="num">${count(r.active_key_count)} / ${count(r.key_count)}</span><div class="hint">active / all</div>`, 'end'],
        ['Plan', r => r.plan_code ? `<code>${esc(r.plan_code)}</code>` : '<span class="muted">-</span>'],
        ['Created', r => when(r.created_at)],
        ['', r => `<div class="actions"><button type="button" class="small primary" data-open-account="${esc(r.id)}">Manage</button>${goButton('Keys', 'keys', { account: r.id }, 'small')}${goButton('Requests', 'requests', { account: r.id }, 'small')}</div>`, 'end'],
      ], v.accounts, { group, empty: ['No accounts match', 'Change the filters.'] })}${listPager('accounts', state, Number(v.total || 0), v.accounts.length)}`;
  },
  async plans(signal) {
    const v = await get(endpoint('plans'), signal);
    const best = bestCode(v.plans.filter(p => p.active));
    return `${pageHead('Plans', 'Deactivate a plan to hide it from customers. Plans referenced by subscriptions, payments, or redeem codes cannot be deleted, only retired.', '<button type="button" class="primary" data-plan>Create plan</button>')}
      ${table([
        ['Plan', r => `<strong>${esc(r.name)}</strong><div class="hint"><code>${esc(r.code)}</code></div>`, 'wrap'],
        ['Price', r => `<span class="num">${esc(usd(r.price_usd))}</span>`, 'end'],
        ['Credit', r => `<span class="num">${esc(usd(r.credit_usd))}</span>`, 'end'],
        ['Days', r => `<span class="num">${count(r.duration_days)}</span>`, 'end'],
        ['Status', r => r.active ? pill(r.code === best ? 'best value' : 'active', 'ok') : pill('inactive', 'muted')],
        ['', r => `<div class="actions">
          <button type="button" class="small" data-edit-plan="${esc(r.code)}" data-plan-name="${esc(r.name)}" data-plan-price="${esc(r.price_usd)}" data-plan-credit="${esc(r.credit_usd)}" data-plan-days="${esc(r.duration_days)}" data-plan-active="${Boolean(r.active)}" data-plan-description="${esc(r.description)}">Edit</button>
          <button type="button" class="small danger" data-delete-plan="${esc(r.code)}">Delete</button>
        </div>`, 'end'],
      ], v.plans)}`;
  },
  async codes(signal) {
    const v = await get(endpoint('codes'), signal);
    const now = nowSeconds();
    const state = r => r.redeemed_at ? pill('redeemed', 'info') : r.expires_at && r.expires_at < now ? pill('expired', 'warn') : pill('available', 'ok');
    return `${pageHead('Redeem codes', 'Codes are shown once and stored hashed. Unredeemed codes can be edited or cancelled. Redeemed codes are kept as audit records.', '<button type="button" class="primary" data-code>Issue codes</button>')}
      ${table([
        ['Code', r => `<code>${esc(r.code_prefix)}...</code>${r.note ? `<div class="hint">${esc(r.note)}</div>` : ''}`, 'wrap'],
        ['Grants', r => r.plan_code ? `Plan <code>${esc(r.plan_code)}</code>` : `<span class="num">${esc(usd(r.credit_usd))}</span>`],
        ['Status', state],
        ['Redeemed by', r => r.redeemed_by ? `${accountLink(r.redeemed_by)}<div class="hint">${esc(date(r.redeemed_at))}</div>` : '<span class="muted">-</span>'],
        ['Expires', r => r.expires_at ? esc(date(r.expires_at)) : '<span class="muted">Never</span>'],
        ['', r => r.redeemed_at ? '' : `<div class="actions">
          <button type="button" class="small" data-edit-code="${esc(r.id)}" data-code-expires="${esc(r.expires_at ?? '')}" data-code-note="${esc(r.note)}">Edit</button>
          <button type="button" class="small danger" data-delete-code="${esc(r.id)}">Delete</button>
        </div>`, 'end'],
      ], v.codes)}`;
  },
  async payments(signal) {
    const state = listStates.payments;
    const params = listParams(state);
    if (state.account) params.set('account', state.account);
    const v = await get(`${endpoint('payments')}?${params}`, signal);
    const toolbar = listToolbar('payments', state, {
      placeholder: 'Search order, account, plan, or provider',
      statuses: [['pending', 'Pending'], ['paid', 'Paid']],
      sorts: [['created', 'Created'], ['amount', 'Amount'], ['status', 'Status'], ['provider', 'Method'], ['account', 'Account'], ['plan', 'Plan']],
      groups: [['status', 'Group by status'], ['method', 'Group by method'], ['plan', 'Group by plan'], ['account', 'Group by account']],
    });
    const group = state.group ? { value: r => state.group === 'plan' ? r.plan_code : state.group === 'account' ? r.account_id : state.group === 'method' ? methodLabel(r.method) : r.status } : null;
    const scope = state.account ? `<div class="filter-chips">${filterChip('payments', 'account', 'Account', esc(shortId(state.account)), `<button type="button" class="link" data-open-account="${esc(state.account)}">Open account</button>`)}</div>` : '';
    return `${pageHead('Payments', `${esc(plural(v.total, 'card or crypto order'))}.`, '<button type="button" class="ghost" data-payroof>Crypto payments setup</button>')}
      ${toolbar}${scope}${table([
        ['Created', r => when(r.created_at)],
        ['Order', r => `<code title="${esc(r.id)}">${esc(shortId(r.id))}</code>`],
        ['Account', r => accountLink(r.account_id)],
        ['Plan', r => `<code>${esc(r.plan_code)}</code>`],
        ['Method', r => esc(methodLabel(r.method))],
        ['Amount', r => `<span class="num">${esc(usd(r.price_usd))}</span>`, 'end'],
        ['Received', r => `<span class="num">${esc(usd(r.received_usd))}</span>`, 'end'],
        ['Status', r => `${pill(r.status)}${r.paid_at ? `<div class="hint">${esc(date(r.paid_at))}</div>` : ''}`],
        ['', r => `<button type="button" class="small" data-payment="${esc(r.id)}">Details</button>`, 'end'],
      ], v.payments, { group, empty: ['No payments match', 'Orders appear when customers check out.'] })}${listPager('payments', state, Number(v.total || 0), v.payments.length)}`;
  },
  async requests(signal) {
    const state = listStates.requests;
    const params = new URLSearchParams({ limit: state.limit, offset: state.offset });
    for (const f of FILTERS.requests) if (state[f]) params.set(f, state[f]);
    const [v, models] = await Promise.all([
      get(`${endpoint(admin ? 'requests' : 'account/requests')}?${params}`, signal),
      requestModels(signal),
    ]);
    const first = v.requests[0];
    const chips = [];
    if (state.key) chips.push(filterChip('requests', 'key', 'Key', esc(first?.key_name || shortId(state.key)), `<button type="button" class="link" data-open-key="${esc(state.key)}">Open key</button>`));
    if (state.account && admin) chips.push(filterChip('requests', 'account', 'Account', esc(accountLabel(state.account, first?.account_contact)), `<button type="button" class="link" data-open-account="${esc(state.account)}">Open account</button>`));
    if (state.model) chips.push(filterChip('requests', 'model', 'Model', `<code>${esc(state.model)}</code>`));
    const option = (value, label, selected) => `<option value="${esc(value)}"${value === selected ? ' selected' : ''}>${esc(label)}</option>`;
    return `${pageHead('Requests', admin ? 'Every proxied request with its usage and charge.' : 'Every request made with your keys and what it was billed.', '<button type="button" data-refresh class="ghost small">Refresh</button>')}
      <form class="toolbar" data-list-form="requests" role="search">
        <input class="toolbar-search" name="q" type="search" value="${esc(state.q)}" aria-label="Search model, route, or request ID" placeholder="Search model, route, or request ID">
        <select name="model" aria-label="Model"><option value="">All models</option>${[...new Set([state.model, ...models].filter(Boolean))].map(m => option(m, m, state.model)).join('')}</select>
        <select name="kind" aria-label="Kind">${option('', 'All kinds', state.kind)}${Object.entries(KIND_LABELS).map(([k, l]) => option(k, l, state.kind)).join('')}</select>
        <select name="status" aria-label="Status">${[['', 'All statuses'], ['2xx', 'Succeeded (2xx)'], ['4xx', 'Rejected (4xx)'], ['5xx', 'Failed (5xx)'], ['pending', 'In progress']].map(([k, l]) => option(k, l, state.status)).join('')}</select>
        <input type="hidden" name="key" value="${esc(state.key)}"><input type="hidden" name="account" value="${esc(state.account)}">
        <div class="toolbar-actions"><button type="submit" class="primary">Apply</button><button type="button" class="ghost" data-list-clear="requests">Reset</button></div>
      </form>
      ${chips.length ? `<div class="filter-chips">${chips.join('')}</div>` : ''}
      ${requestTable(v.requests)}
      ${listPager('requests', state, Number(v.total || 0), v.requests.length)}`;
  },
  async usage(signal) {
    const v = await get('/v1/account/usage', signal);
    const t = seriesTotals(v.series);
    const days = [...dailyTotals(v.series)].filter(([, d]) => d.requests > 0).reverse();
    return `${pageHead('Usage', 'Billed usage of all keys over the last 30 days. Open a key under API keys for its own numbers.')}
      <div class="stats">
        ${metric('Billed', usd(t.cost))}
        ${metric('Requests', count(t.requests))}
        ${metric('Tokens', count(t.tokens))}
      </div>
      <div class="card"><div class="card-head"><h3>Daily spend</h3></div>${chart(v.series)}</div>
      <div class="card"><div class="card-head"><h3>By model</h3></div>${modelTable(v.series)}</div>
      <div class="card"><div class="card-head"><h3>By day</h3></div>${table([
        ['Day', ([d]) => esc(day(d, { weekday: 'short', month: 'short', day: 'numeric' }))],
        ['Requests', ([, d]) => `<span class="num">${count(d.requests)}</span>`, 'end'],
        ['Tokens', ([, d]) => `<span class="num">${count(d.tokens)}</span>`, 'end'],
        ['Billed', ([, d]) => `<span class="num">${esc(usd(d.cost))}</span>`, 'end'],
      ], days, { empty: ['No usage yet', 'Daily totals appear after the first request.'] })}</div>`;
  },
  async billing(signal) {
    const [v, p, orders] = await Promise.all([get('/v1/account', signal), get('/v1/plans', signal), get('/v1/account/payments', signal)]);
    const a = v.account;
    const methods = p.payment_methods || [];
    planCatalog = { plans: p.plans, methods };
    const topUp = v.top_up || {};
    const hasTopUp = topUp.available === false || Boolean(topUp.networks?.length);
    const badges = planBadges(p.plans);
    const cta = plan => methods.length
      ? `<button type="button" class="${badges.get(plan.code) === 'Best value' ? 'primary' : ''}" data-checkout="${esc(plan.code)}">Choose ${esc(plan.name)}</button>`
      : '<span class="hint">Online payments are not enabled. Use a credit code.</span>';
    const lots = v.credits.filter(c => lotState(c) === 'active');
    const trial = lots.filter(c => c.source === 'free');
    const trialLeft = trial.reduce((sum, c) => sum + Number(c.remaining_usd), 0);
    const trialEnds = Math.min(...trial.map(c => c.expires_at || Infinity));
    const nextExpiry = lots.filter(c => c.expires_at).sort((x, y) => x.expires_at - y.expires_at)[0];
    const sub = a.subscription;
    const subPlan = sub && p.plans.find(x => x.code === sub.plan_code);
    const daysLeft = sub ? Math.max(0, Math.ceil((sub.expires_at - nowSeconds()) / DAY)) : 0;
    const empty = !isPositive(a.balance_usd);
    const debt = isPositive(a.debt_usd);
    const planName = code => p.plans.find(x => x.code === code)?.name || code;
    const ways = [
      methods.includes('stripe') && ['Card', 'Pay for a plan on Stripe\'s secure checkout page.', '<button type="button" class="small" data-scroll="plans">Choose a plan</button>'],
      methods.includes('crypto') && ['Crypto checkout', 'Pay for a plan with USDC, USDT, or native coins on a supported network.', '<button type="button" class="small" data-scroll="plans">Choose a plan</button>'],
      hasTopUp && ['Stablecoin top-up', 'Send stablecoins to your personal address. Any amount, no plan needed.', '<button type="button" class="small" data-scroll="topup">Show my address</button>'],
      ['Redeem a code', 'Apply a single-use credit code from the operator.', '<button type="button" class="small" data-redeem>Redeem a code</button>'],
    ].filter(Boolean);
    return `<section class="card balance-hero${empty || debt ? ' warn' : ''}" aria-labelledby="balance-label">
        <div class="balance-main">
          <div class="label" id="balance-label">Available balance</div>
          <div class="metric balance-amount">${esc(usd(a.balance_usd))}</div>
          <ul class="balance-notes">
            ${debt ? `<li class="warn">Outstanding debt of ${esc(usd(a.debt_usd))} is repaid from your next top-up.</li>` : ''}
            ${empty && !debt ? '<li class="warn">Your balance is empty. API calls are refused until you add credit.</li>' : ''}
            ${trialLeft > 0 ? `<li>Includes ${esc(usd(trialLeft))} of trial credit${Number.isFinite(trialEnds) ? `, valid until ${esc(date(trialEnds))}` : ''}.</li>` : ''}
            ${nextExpiry && nextExpiry !== trial[0] ? `<li>Next credit expiry: ${esc(usd(nextExpiry.remaining_usd))} on ${esc(date(nextExpiry.expires_at))}. Credit that expires first is spent first.</li>` : ''}
            <li>Shared by every key on your account.</li>
          </ul>
        </div>
        <div class="actions"><button type="button" class="${empty ? 'primary' : 'ghost'}" data-scroll="${hasTopUp || methods.length ? 'ways' : 'plans'}">Add credit</button></div>
      </section>
      <div class="stats">
        ${metric('Current plan', sub ? (subPlan?.name || sub.plan_code) : 'No active plan', sub ? `Active until ${esc(date(sub.expires_at))}, ${esc(plural(daysLeft, 'day'))} left` : 'Pay as you go. Buy a plan below to add bundled credit.')}
        ${metric('Spent so far', usd(a.billed_usd), 'Across all your keys')}
        ${metric('Credit lots in use', count(lots.length), 'Soonest-expiring credit is spent first')}
      </div>
      <section class="billing-ways" id="ways" tabindex="-1" aria-labelledby="ways-title">
        <div class="section-head"><h3 id="ways-title">Ways to add credit</h3></div>
        <div class="ways">${ways.map(([title, text, action]) => `<div class="card way"><strong>${esc(title)}</strong><p>${esc(text)}</p>${action}</div>`).join('')}</div>
      </section>
      <section class="billing-catalog" id="plans" tabindex="-1" aria-labelledby="plans-title">
        <div class="catalog-head"><h3 id="plans-title">Prepaid plans</h3><p>Non-renewing packages. Credit is shared by every key on your account.${methods.includes('crypto') ? ' Crypto checkout accepts USDC, USDT, and native coins on supported networks.' : ''}</p></div>
        ${pricingSection(p.plans, methods, cta)}
      </section>
      ${topUpCard(topUp)}
      <section class="billing-section" aria-labelledby="history-title">
        <div class="section-head"><h3 id="history-title">Payment history</h3><span class="hint">Latest ${esc(plural(orders.payments.length, 'payment'))}</span></div>
        ${table([
          ['Date', r => when(r.created_at)],
          ['Plan', r => `<strong>${esc(planName(r.plan_code))}</strong><div class="hint">${esc(plural(r.duration_days, 'day'))}, ${esc(usd(r.credit_usd))} credit</div>`],
          ['Method', r => esc(methodLabel(r.method))],
          ['Amount', r => `<span class="num">${esc(usd(r.price_usd))}</span>`, 'end'],
          ['Received', r => r.method === 'crypto' ? `<span class="num">${esc(usd(r.received_usd))}</span>` : DASH, 'end'],
          ['Status', r => pill(r.status)],
          ['', r => `<div class="actions">${r.method === 'crypto' && r.status === 'pending' ? `<button type="button" class="small primary" data-order="${esc(r.id)}">Pay</button>` : ''}<button type="button" class="small" data-receipt="${esc(r.id)}" aria-label="Details of payment ${esc(shortId(r.id))}">Details</button></div>`, 'end'],
        ], orders.payments, { empty: ['No payments yet', 'Plan purchases appear here with their status and a receipt.'] })}
      </section>
      <section class="billing-section" aria-labelledby="ledger-title">
        <div class="section-head"><h3 id="ledger-title">Credit ledger</h3><span class="hint">Each top-up, plan, or grant is one lot with its own expiry</span></div>
        ${creditTable(v.credits)}
      </section>`;
  },
  async settings(signal) {
    const v = await get(endpoint('settings'), signal);
    return `${pageHead('Settings', 'Runtime configuration stored in SQLite. Secret fields are redacted: leave them blank to keep their values. USD amounts and percentages are decimal strings. CORS changes need a restart.')}
      <div class="callout info"><p><code>branding</code> controls the name, copy, logo, favicon, colours (<code>css_vars</code>), and custom CSS of these pages. <code>stripe</code> and <code>payroof</code> enable card and crypto checkout. <code>models</code> is the catalog customers see.</p></div>
      <form id="settings" class="card">
        <label for="settings-json">Configuration JSON</label>
        <textarea id="settings-json" name="json" rows="30" spellcheck="false">${esc(JSON.stringify(v.settings, null, 2))}</textarea>
        <div class="actions"><button class="primary" type="submit">Save settings</button></div>
      </form>`;
  },
};

const listParams = state => {
  const params = new URLSearchParams({ limit: state.limit, offset: state.offset, sort: state.sort, dir: state.dir });
  if (state.q) params.set('q', state.q);
  if (state.status) params.set('status', state.status);
  return params;
};

// Model names for the request filter, cached for a minute.
let requestModelCache = { values: [], expiresAt: 0 };
async function requestModels(signal) {
  if (requestModelCache.expiresAt > Date.now()) return requestModelCache.values;
  try {
    const values = admin
      ? (await get(endpoint('requests/groups?field=model&limit=200'), signal)).groups.map(g => g.label)
      : modelTotals((await get('/v1/account/usage', signal)).series).map(m => m.model);
    requestModelCache = { values: values.filter(Boolean).sort(), expiresAt: Date.now() + 60_000 };
  } catch (e) {
    if (e.name === 'AbortError') throw e;
    // Suggestions are optional. The filter still works without them.
  }
  return requestModelCache.values;
}

/* Plans and codes ------------------------------------------------------------------ */

function planForm(initial = {}) {
  const editing = Boolean(initial.code);
  modal(editing ? `Edit plan: ${initial.code}` : 'Create plan', `<form><div class="form-grid"><div><label for="plan-code">Code</label><input id="plan-code" name="code" value="${esc(initial.code || '')}" placeholder="starter" maxlength="80" pattern="[A-Za-z0-9_-]+" ${editing ? 'readonly' : 'required'}></div><div><label for="plan-name">Name</label><input id="plan-name" name="name" value="${esc(initial.name || '')}" placeholder="Starter" required></div><div><label for="plan-price">Price (USD)</label><input id="plan-price" name="price_usd" type="number" min="0.50" step="0.01" value="${esc(initial.price_usd || '10')}" required></div><div><label for="plan-credit">API credit (USD)</label><input id="plan-credit" name="credit_usd" type="number" min="0.000001" step="0.000001" value="${esc(initial.credit_usd || '10')}" required></div><div><label for="plan-days">Duration (days)</label><input id="plan-days" name="duration_days" type="number" min="1" max="3650" value="${esc(initial.duration_days || 30)}" required></div></div><label for="plan-description">Description</label><input id="plan-description" name="description" value="${esc(initial.description || '')}" maxlength="4096" placeholder="Thirty days of API credit"><label class="inline check-line"><input type="checkbox" name="active"${initial.active !== false ? ' checked' : ''}> Visible to customers</label><div class="actions"><button class="primary" type="submit">${editing ? 'Save changes' : 'Create plan'}</button></div></form>`, d => bindForm(d.querySelector('form'), async b => {
    await api(endpoint('plans'), 'POST', { ...b, duration_days: Number(b.duration_days), active: b.active === 'on' });
    d.close();
    notice(editing ? 'Plan updated' : 'Plan created');
    await load();
  }), 'wide');
}

function showIssuedCodes(v) {
  const codes = v.codes || [v.code];
  const list = codes.join('\n');
  modal(codes.length > 1 ? 'Save your redeem codes' : 'Save your redeem code', `<p class="muted">${count(codes.length)} single-use code${codes.length === 1 ? '' : 's'}. Shown only once. Copy ${codes.length === 1 ? 'it' : 'them'} now.</p><pre class="code-list secret">${esc(list)}</pre><div class="actions section"><button type="button" class="primary" data-copy-codes>Copy ${codes.length === 1 ? 'code' : 'all'}</button></div>`, d => {
    const button = d.querySelector('[data-copy-codes]');
    button.onclick = () => copyText(list, 'Redeem codes').then(ok => ok && flashCopied(button));
  }, 'wide');
}

async function codeForm() {
  const plans = (await api(endpoint('plans'))).plans.filter(p => p.active);
  modal('Issue redeem codes', `<form><div class="form-grid"><div><label for="code-plan">Credit source</label><select id="code-plan" name="plan_code"><option value="">Fixed credit amount</option>${plans.map(p => `<option value="${esc(p.code)}">Plan: ${esc(p.name)}</option>`).join('')}</select></div><div><label for="code-credit">Credit (USD)</label><input id="code-credit" name="credit_usd" type="number" min="0.000001" step="0.000001" value="10" required></div><div><label for="code-count">Number of codes</label><input id="code-count" name="count" type="number" min="1" max="100" value="1" required></div><div><label for="code-expiry">Expires (optional)</label><input id="code-expiry" name="expires_at" type="datetime-local"></div></div><label for="code-note">Internal note (optional)</label><input id="code-note" name="note" maxlength="4096" placeholder="Campaign or customer reference"><p class="field-help">Codes are single use and only displayed once after creation.</p><div class="actions"><button class="primary" type="submit">Issue codes</button></div></form>`, d => {
    const form = d.querySelector('form');
    const plan = form.querySelector('[name=plan_code]');
    const credit = form.querySelector('[name=credit_usd]');
    plan.onchange = () => {
      credit.disabled = Boolean(plan.value);
      credit.required = !plan.value;
    };
    bindForm(form, async b => {
      const item = { note: b.note, expires_at: toUnix(b.expires_at) };
      if (b.plan_code) item.plan_code = b.plan_code;
      else item.credit_usd = b.credit_usd;
      const amount = Number(b.count);
      const result = await api(endpoint('codes'), 'POST', amount === 1 ? item : Array.from({ length: amount }, () => ({ ...item })));
      d.close();
      showIssuedCodes(result);
      await load();
    });
  }, 'wide');
}

function editCodeForm(button) {
  modal('Edit redeem code', `<form><label for="edit-code-expiry">Expires (optional)</label><input id="edit-code-expiry" name="expires_at" type="datetime-local" value="${esc(localDateTime(button.dataset.codeExpires))}"><label for="edit-code-note">Internal note</label><input id="edit-code-note" name="note" value="${esc(button.dataset.codeNote || '')}" maxlength="4096"><div class="actions"><button class="primary" type="submit">Save changes</button></div></form>`, d => bindForm(d.querySelector('form'), async b => {
    await api(endpoint(`codes/${button.dataset.editCode}`), 'PATCH', { expires_at: toUnix(b.expires_at), note: b.note });
    d.close();
    notice('Code updated');
    await load();
  }));
}

/* Payments ------------------------------------------------------------------------- */

const shortHash = v => v && v.length > 18 ? `${v.slice(0, 10)}...${v.slice(-6)}` : v || '';
function wireCopies(root) {
  root.querySelectorAll('[data-copy-value]').forEach(b => {
    b.onclick = () => copyText(b.dataset.copyValue, b.dataset.copyLabel || 'Value').then(ok => ok && flash(b));
  });
}
/* QR code ------------------------------------------------------------------------
   A small dependency-free encoder: byte mode, error correction level M, versions
   1 to 10 (up to 213 bytes). Enough for addresses and EIP-681 payment links.
   qrSvg(text) returns an inline SVG (black on white, with the 4-module quiet zone
   scanners need, so it also works on dark themes) or '' when the text is too long. */
// Per version at level M: [data codewords, EC codewords per block, blocks in group 1, data per block in group 1, blocks in group 2].
const QR_M = [null, [16, 10, 1, 16, 0], [28, 16, 1, 28, 0], [44, 26, 1, 44, 0], [64, 18, 2, 32, 0], [86, 24, 2, 43, 0], [108, 16, 4, 27, 0], [124, 18, 4, 31, 0], [154, 22, 2, 38, 2], [182, 22, 3, 36, 2], [216, 26, 4, 43, 1]];
const QR_ALIGN = [null, [], [6, 18], [6, 22], [6, 26], [6, 30], [6, 34], [6, 22, 38], [6, 24, 42], [6, 26, 46], [6, 28, 50]];
const QR_EXP = new Uint8Array(512);
const QR_LOG = new Uint8Array(256);
for (let i = 0, x = 1; i < 255; i++) {
  QR_EXP[i] = x;
  QR_LOG[x] = i;
  x <<= 1;
  if (x & 256) x ^= 0x11d;
}
for (let i = 255; i < 512; i++) QR_EXP[i] = QR_EXP[i - 255];
const qrMul = (a, b) => a && b ? QR_EXP[QR_LOG[a] + QR_LOG[b]] : 0;
function qrReedSolomon(data, degree) {
  let gen = [1];
  for (let i = 0; i < degree; i++) {
    const next = new Array(gen.length + 1).fill(0);
    gen.forEach((c, j) => {
      next[j] ^= c;
      next[j + 1] ^= qrMul(c, QR_EXP[i]);
    });
    gen = next;
  }
  const rest = new Array(degree).fill(0);
  for (const byte of data) {
    const factor = byte ^ rest.shift();
    rest.push(0);
    for (let j = 0; j < degree; j++) rest[j] ^= qrMul(gen[j + 1], factor);
  }
  return rest;
}
function qrBch(value, poly, bits) {
  let r = value << bits;
  for (let i = 31 - Math.clz32(r); i >= bits; i = 31 - Math.clz32(r)) r ^= poly << (i - bits);
  return (value << bits) | r;
}
const QR_MASKS = [
  (r, c) => (r + c) % 2 === 0,
  (r) => r % 2 === 0,
  (r, c) => c % 3 === 0,
  (r, c) => (r + c) % 3 === 0,
  (r, c) => (Math.floor(r / 2) + Math.floor(c / 3)) % 2 === 0,
  (r, c) => ((r * c) % 2) + ((r * c) % 3) === 0,
  (r, c) => (((r * c) % 2) + ((r * c) % 3)) % 2 === 0,
  (r, c) => (((r + c) % 2) + ((r * c) % 3)) % 2 === 0,
];
function qrPenalty(m) {
  const n = m.length;
  let score = 0;
  const runs = line => {
    let s = 0, run = 1;
    for (let i = 1; i <= line.length; i++) {
      if (i < line.length && line[i] === line[i - 1]) run++;
      else {
        if (run >= 5) s += run - 2;
        run = 1;
      }
    }
    return s;
  };
  const finder = line => {
    let s = 0;
    const a = [1, 0, 1, 1, 1, 0, 1, 0, 0, 0, 0], b = [0, 0, 0, 0, 1, 0, 1, 1, 1, 0, 1];
    for (let i = 0; i + 11 <= line.length; i++) {
      if (a.every((v, j) => line[i + j] === v)) s += 40;
      if (b.every((v, j) => line[i + j] === v)) s += 40;
    }
    return s;
  };
  for (let i = 0; i < n; i++) {
    const row = m[i], col = m.map(r => r[i]);
    score += runs(row) + runs(col) + finder(row) + finder(col);
  }
  for (let r = 0; r < n - 1; r++) for (let c = 0; c < n - 1; c++) {
    if (m[r][c] === m[r][c + 1] && m[r][c] === m[r + 1][c] && m[r][c] === m[r + 1][c + 1]) score += 3;
  }
  const dark = m.reduce((s, row) => s + row.reduce((t, v) => t + v, 0), 0);
  return score + Math.floor(Math.abs(dark * 20 - n * n * 10) / (n * n)) * 10;
}
function qrMatrix(text) {
  const bytes = [...new TextEncoder().encode(text)];
  const version = QR_M.findIndex((v, i) => v && bytes.length <= v[0] - (i < 10 ? 2 : 3));
  if (version < 1) return null;
  const [total, ecLen, g1, d1, g2] = QR_M[version];
  // Bit stream: mode 0100, length, data, terminator, padding.
  const bits = [];
  const push = (value, n) => { for (let i = n - 1; i >= 0; i--) bits.push((value >> i) & 1); };
  push(4, 4);
  push(bytes.length, version < 10 ? 8 : 16);
  bytes.forEach(b => push(b, 8));
  push(0, Math.min(4, total * 8 - bits.length));
  while (bits.length % 8) bits.push(0);
  const words = [];
  for (let i = 0; i < bits.length; i += 8) words.push(parseInt(bits.slice(i, i + 8).join(''), 2));
  for (let pad = 0xec; words.length < total; pad ^= 0xec ^ 0x11) words.push(pad);
  // Split into blocks, add EC, interleave.
  const blocks = [];
  let at = 0;
  for (let i = 0; i < g1 + g2; i++) {
    const size = i < g1 ? d1 : d1 + 1;
    const data = words.slice(at, at + size);
    at += size;
    blocks.push({ data, ec: qrReedSolomon(data, ecLen) });
  }
  const stream = [];
  for (let i = 0; i < d1 + 1; i++) blocks.forEach(b => { if (i < b.data.length) stream.push(b.data[i]); });
  for (let i = 0; i < ecLen; i++) blocks.forEach(b => stream.push(b.ec[i]));
  // Function patterns.
  const n = 17 + 4 * version;
  const m = Array.from({ length: n }, () => new Array(n).fill(0));
  const fixed = Array.from({ length: n }, () => new Array(n).fill(false));
  const set = (r, c, v) => { m[r][c] = v ? 1 : 0; fixed[r][c] = true; };
  const finderAt = (r0, c0) => {
    for (let dr = -1; dr <= 7; dr++) for (let dc = -1; dc <= 7; dc++) {
      const r = r0 + dr, c = c0 + dc;
      if (r < 0 || c < 0 || r >= n || c >= n) continue;
      const edge = Math.max(Math.abs(dr - 3), Math.abs(dc - 3));
      set(r, c, edge !== 2 && edge !== 4);
    }
  };
  finderAt(0, 0);
  finderAt(0, n - 7);
  finderAt(n - 7, 0);
  for (let i = 8; i < n - 8; i++) {
    set(6, i, i % 2 === 0);
    set(i, 6, i % 2 === 0);
  }
  const centers = QR_ALIGN[version];
  centers.forEach(cr => centers.forEach(cc => {
    const last = centers[centers.length - 1];
    if ((cr === 6 && (cc === 6 || cc === last)) || (cr === last && cc === 6)) return; // overlaps a finder
    for (let dr = -2; dr <= 2; dr++) for (let dc = -2; dc <= 2; dc++) set(cr + dr, cc + dc, Math.max(Math.abs(dr), Math.abs(dc)) !== 1);
  }));
  // Reserve format and version areas, and the dark module.
  for (let i = 0; i < 9; i++) { fixed[8][i] = true; fixed[i][8] = true; }
  for (let i = 0; i < 8; i++) { fixed[8][n - 1 - i] = true; fixed[n - 1 - i][8] = true; }
  set(n - 8, 8, true);
  if (version >= 7) {
    const info = qrBch(version, 0x1f25, 12);
    for (let i = 0; i < 18; i++) {
      const bit = (info >> i) & 1, a = Math.floor(i / 3), b = (i % 3) + n - 11;
      set(a, b, bit);
      set(b, a, bit);
    }
  }
  // Data bits in the zig-zag order, right to left in column pairs.
  const dataBits = [];
  stream.forEach(w => { for (let i = 7; i >= 0; i--) dataBits.push((w >> i) & 1); });
  const cells = [];
  for (let right = n - 1, up = true; right >= 1; right -= 2, up = !up) {
    if (right === 6) right = 5;
    for (let k = 0; k < n; k++) {
      const r = up ? n - 1 - k : k;
      for (const c of [right, right - 1]) if (!fixed[r][c]) cells.push([r, c]);
    }
  }
  cells.forEach(([r, c], i) => { m[r][c] = dataBits[i] || 0; });
  // Pick the mask with the lowest penalty and write the format bits.
  let best = null;
  QR_MASKS.forEach((mask, id) => {
    const t = m.map(row => row.slice());
    cells.forEach(([r, c]) => { if (mask(r, c)) t[r][c] ^= 1; });
    const format = qrBch((0 << 3) | id, 0x537, 10) ^ 0x5412; // level M = 00
    writeFormat(t, n, format);
    const score = qrPenalty(t);
    if (!best || score < best.score) best = { score, t };
  });
  return best.t;
}
// Format bits (15): copy 1 around the top-left finder, copy 2 split between the other two.
function writeFormat(t, n, format) {
  for (let i = 0; i < 15; i++) {
    const bit = (format >> i) & 1;
    // Copy 1.
    if (i < 6) t[i][8] = bit;
    else if (i < 8) t[i + 1][8] = bit;
    else if (i === 8) t[8][7] = bit;
    else t[8][14 - i] = bit;
    // Copy 2.
    if (i < 8) t[8][n - 1 - i] = bit;
    else t[n - 15 + i][8] = bit;
  }
  t[n - 8][8] = 1;
}
function qrSvg(text, label = 'QR code') {
  const m = qrMatrix(text);
  if (!m) return '';
  const n = m.length, size = n + 8;
  let path = '';
  m.forEach((row, r) => {
    for (let c = 0; c < n; c++) {
      if (!row[c]) continue;
      let end = c;
      while (end + 1 < n && row[end + 1]) end++;
      path += `M${c + 4} ${r + 4}h${end - c + 1}v1h-${end - c + 1}z`;
      c = end;
    }
  });
  return `<svg class="qr" viewBox="0 0 ${size} ${size}" role="img" aria-label="${esc(label)}" shape-rendering="crispEdges"><rect width="${size}" height="${size}" fill="#fff"/><path d="${path}" fill="#000"/></svg>`;
}

/* Crypto helpers ---------------------------------------------------------------
   Used by the stablecoin top-up card and the "Pay with crypto" dialog. Every link
   comes from server-supplied explorer templates and must be https. */
const DASH = '<span class="muted" title="Not available">&mdash;</span>';
const joinOr = names => names.length < 2 ? names.join('') : `${names.slice(0, -1).join(', ')} or ${names[names.length - 1]}`;
function explorerHref(links, kind, value) {
  const template = links?.[kind];
  if (!template || !value) return '';
  try {
    const url = new URL(template.replace(`{${kind}}`, encodeURIComponent(value)));
    return url.protocol === 'https:' ? url.href : '';
  } catch {
    return '';
  }
}
function explorerLink(links, kind, value, label = 'Explorer') {
  const href = explorerHref(links, kind, value);
  return href ? `<a class="ext-link" href="${esc(href)}" target="_blank" rel="noopener noreferrer">${esc(label)}<span class="sr-only"> on ${esc(links.name)} (opens in a new tab)</span></a>` : '';
}
// Base units to a decimal string, without floating point: ("19990000", 6) is "19.99".
function fromUnits(units, decimals) {
  try {
    const digits = BigInt(units).toString().padStart(decimals + 1, '0');
    const whole = digits.slice(0, digits.length - decimals);
    const frac = digits.slice(digits.length - decimals).replace(/0+$/, '');
    return frac ? `${whole}.${frac}` : whole;
  } catch {
    return String(units);
  }
}
// EIP-681 payment request that wallets can scan: a token transfer, or a native
// coin transfer. Without a chain id there is nothing to encode but the address.
function paymentUri(q) {
  if (!q.chain_id) return q.address;
  return q.contract
    ? `ethereum:${q.contract}@${q.chain_id}/transfer?address=${q.address}&uint256=${q.amount_units}`
    : `ethereum:${q.address}@${q.chain_id}?value=${q.amount_units}`;
}
const contractCell = (links, contract) => contract
  ? `<span class="contract">${copyable(contract, 'Contract address', shortHash(contract))}${explorerLink(links, 'token', contract)}</span>`
  : '<span class="muted">Native coin, no contract</span>';
const NOTE_LABELS = {
  unsupported_asset: 'Token is not credited',
  invalid_amount: 'Amount could not be valued',
  unquoted_asset: 'Not an asset this order accepts',
  address_mismatch: 'Sent to another address or network',
  late: 'Arrived after the quote expired',
};
const noteLabel = n => NOTE_LABELS[n] || String(n).replace(/_/g, ' ');
const countdown = seconds => {
  const s = Math.max(0, Math.floor(seconds));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), r = s % 60;
  return h ? `${h}:${String(m).padStart(2, '0')}:${String(r).padStart(2, '0')}` : `${m}:${String(r).padStart(2, '0')}`;
};

/* Stablecoin top-up -------------------------------------------------------------
   Standing per-account addresses. The server lists, per network, the address and
   the tokens it credits 1:1. Anything else sent there is recorded, not credited. */
let topUpState = { data: null, chain: '' };
const DEPOSIT_STATES = { confirming: ['Confirming', 'info'], credited: ['Credited', 'ok'], not_credited: ['Not credited', 'warn'] };
function depositsTable(rows) {
  return table([
    ['Seen', r => when(r.seen_at)],
    ['Network', r => esc(r.chain_name || r.chain)],
    ['Amount', r => r.amount ? `<span class="num">${esc(r.amount)} ${esc(r.symbol)}</span>` : DASH, 'end'],
    ['Status', r => { const [label, tone] = DEPOSIT_STATES[r.state] || [r.state, 'muted']; return `${pill(label, tone)}${r.note ? `<div class="hint">${esc(noteLabel(r.note))}</div>` : ''}`; }],
    ['Credited', r => isPositive(r.credited_usd) ? `<span class="num">${esc(usd(r.credited_usd))}</span>` : DASH, 'end'],
    ['Transaction', r => `<span class="contract">${copyable(r.transaction, 'Transaction hash', shortHash(r.transaction))}${explorerLink(r.explorer, 'tx', r.transaction)}</span>`],
  ], rows, { empty: ['No deposits yet', 'Transfers to your address show up here as soon as the network reports them.'] });
}
function netPanel(n, t) {
  const symbols = [...new Set(n.tokens.map(x => x.symbol))];
  const minimum = n.min_deposit_usd ? `${usd(n.min_deposit_usd)} per transfer` : 'None enforced. You pay the network fee, so very small transfers are rarely worth sending.';
  const timing = [
    n.confirmations ? `${plural(n.confirmations, 'confirmation')}` : null,
    n.eta_minutes ? `usually about ${plural(n.eta_minutes, 'minute')}` : null,
  ].filter(Boolean).join(', ');
  return `<div class="callout warn" role="note"><p><strong>Send only these tokens on ${esc(n.chain_name)}.</strong> Other tokens or networks can be lost. Native coins and any other token sent here are recorded but not credited.</p></div>
    <div class="addr-card">
      <div class="qr-box">${qrSvg(n.address, `QR code of your ${n.chain_name} deposit address`)}<span class="hint">Scan to read the address</span></div>
      <div class="addr-body">
        <div class="label">Your ${esc(n.chain_name)} deposit address</div>
        <div class="addr-value">${copyable(n.address, `${n.chain_name} address`)}</div>
        <div class="actions"><button type="button" class="small" data-copy-value="${esc(n.address)}" data-copy-label="${esc(n.chain_name)} address">Copy address</button>${explorerLink(n.explorer, 'address', n.address, 'View on explorer')}</div>
        <p class="hint">The address is personal to your account and does not change. Send ${esc(joinOr(symbols))} on the ${esc(n.chain_name)} network only.</p>
      </div>
    </div>
    ${facts([
      ['Network', `${esc(n.chain_name)}${n.chain_id ? ` <span class="hint">chain ID ${esc(n.chain_id)}</span>` : ''}`],
      ['Credit', `Each accepted token is credited 1:1 in USD (for example 1 ${esc(symbols[0] || 'USDC')} = ${esc(usd(1))}).`],
      timing && ['Credited after', esc(timing)],
      ['Minimum', esc(minimum)],
    ])}
    <h4 class="sub-head">Accepted tokens on ${esc(n.chain_name)}</h4>
    ${table([
      ['Token', x => `<div class="model-cell"><strong>${esc(x.symbol)}</strong>${x.name && x.name !== x.symbol ? `<span class="hint">${esc(x.name)}</span>` : ''}</div>`],
      ['Contract address', x => contractCell(n.explorer, x.contract), 'wrap'],
      ['Decimals', x => `<span class="num">${esc(x.decimals)}</span>`, 'end'],
      ['Credited as', x => esc(x.credit)],
    ], n.tokens)}`;
}
function topUpCard(t) {
  if (t.available === false) {
    return `<section class="card" id="topup" aria-labelledby="topup-title"><div class="card-head"><h3 id="topup-title">Top up with stablecoins</h3></div>
      <div class="callout warn"><p>Your deposit addresses are temporarily unavailable. Try again in a moment.</p><div class="actions"><button type="button" class="small" data-refresh>Try again</button></div></div></section>`;
  }
  if (!t.networks?.length) return '';
  topUpState.data = t;
  if (!t.networks.some(n => n.chain === topUpState.chain)) topUpState.chain = t.networks[0].chain;
  const symbols = [...new Set(t.networks.flatMap(n => n.tokens.map(x => x.symbol)))];
  const steps = [
    ['Pick a network', 'Choose the network your wallet or exchange will send on.'],
    ['Send a supported token', `Send ${joinOr(symbols)} to your personal address. Anything else is not credited.`],
    ['Credit appears', `Once the network confirms the transfer (usually about ${plural(t.eta_minutes, 'minute')}), the amount is added to your balance 1:1 in USD.`],
  ];
  return `<section class="card topup" id="topup" aria-labelledby="topup-title" data-topup>
    <div class="card-head"><h3 id="topup-title">Top up with stablecoins</h3><span class="hint">Any amount, no plan needed</span></div>
    <ol class="how-steps">${steps.map(([title, text], n) => `<li><span class="step-dot">${n + 1}</span><div><strong>${esc(title)}</strong><p>${esc(text)}</p></div></li>`).join('')}</ol>
    <div class="net-tabs" role="tablist" aria-label="Network">${t.networks.map(n => `<button type="button" role="tab" id="net-tab-${esc(n.chain.replace(/\W/g, '-'))}" data-net="${esc(n.chain)}" aria-selected="${n.chain === topUpState.chain}" aria-controls="net-panel" tabindex="${n.chain === topUpState.chain ? 0 : -1}">${esc(n.chain_name)}</button>`).join('')}</div>
    <div id="net-panel" role="tabpanel" data-net-panel>${netPanel(t.networks.find(n => n.chain === topUpState.chain), t)}</div>
    <h4 class="sub-head">Recent deposits</h4>
    ${depositsTable(t.recent || [])}
  </section>`;
}
function wireTopUp(root) {
  const box = root.querySelector('[data-topup]');
  if (!box) return;
  const tabs = [...box.querySelectorAll('[data-net]')];
  const select = (chain, focus) => {
    topUpState.chain = chain;
    tabs.forEach(b => {
      b.setAttribute('aria-selected', String(b.dataset.net === chain));
      b.tabIndex = b.dataset.net === chain ? 0 : -1;
      if (focus && b.dataset.net === chain) b.focus();
    });
    const panel = box.querySelector('[data-net-panel]');
    panel.innerHTML = netPanel(topUpState.data.networks.find(n => n.chain === chain), topUpState.data);
    wireCopies(panel);
  };
  tabs.forEach((b, i) => {
    b.onclick = () => select(b.dataset.net);
    b.onkeydown = e => {
      const target = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[e.key];
      if (target === undefined) return;
      e.preventDefault();
      select(tabs[(target + tabs.length) % tabs.length].dataset.net, true);
    };
  });
}

/* Pay with crypto ---------------------------------------------------------------- */
function transfersTable(o) {
  const transfers = o.transfers || [];
  if (!transfers.length) return '';
  const quote = t => (o.quotes || []).find(q => q.asset === t.asset);
  return `<section class="detail-section"><h4>Transfers</h4>${table([
    ['Seen', r => when(r.seen_at)],
    ['Amount', r => { const q = quote(r); return q ? `<span class="num">${esc(fromUnits(r.amount_units, q.decimals))} ${esc(q.symbol)}</span><div class="hint">${esc(q.chain_name)}</div>` : DASH; }],
    ['Transaction', r => `<span class="contract">${copyable(r.transaction, 'Transaction hash', shortHash(r.transaction))}${explorerLink(r.explorer, 'tx', r.transaction)}</span>`],
    ['Status', r => r.status === 'confirmed' ? pill('Confirmed', 'ok') : pill('Detected', 'info')],
    ['Counted', r => r.status === 'confirmed' ? `<span class="num">${esc(usd(r.value_usd))}</span>` : DASH, 'end'],
    ['Note', r => r.note ? pill(noteLabel(r.note), 'warn') : ''],
  ], transfers)}</section>`;
}
const planOfOrder = o => {
  const known = planCatalog.plans.find(p => p.code === o.plan_code);
  return { code: o.plan_code, name: known?.name || o.plan_code, price_usd: o.price_usd, credit_usd: o.credit_usd, duration_days: o.duration_days };
};
// pending, confirming, underpaid, review, paid, expired, or failed.
function orderState(o) {
  if (o.status === 'paid') return 'paid';
  if (o.status === 'failed' || o.status === 'cancelled') return 'failed';
  const transfers = o.transfers || [];
  if (transfers.some(t => t.status === 'seen')) return 'confirming';
  const confirmed = transfers.filter(t => t.status === 'confirmed');
  if (confirmed.some(t => isPositive(t.value_usd))) return 'underpaid';
  if (confirmed.length) return 'review';
  return o.status === 'expired' ? 'expired' : 'pending';
}
const PAY_STEPS = ['Waiting for payment', 'Detected', 'Confirmed', 'Credited'];
const PAY_PROGRESS = { pending: 0, expired: 0, failed: 0, confirming: 1, underpaid: 2, review: 2, paid: 3 };
const payTimeline = state => `<ol class="timeline" aria-label="Payment progress">${PAY_STEPS.map((label, n) => {
  const done = state === 'paid' || n < PAY_PROGRESS[state];
  const current = !done && n === PAY_PROGRESS[state];
  return `<li class="${done ? 'done' : current ? 'current' : ''}"${current ? ' aria-current="step"' : ''}><span class="step-dot">${done ? CHECK : n + 1}</span><span class="step-label">${esc(label)}</span></li>`;
}).join('')}</ol>`;
const STATE_ICONS = { paid: CHECK, expired: '!', failed: '&times;', review: '!', underpaid: '!', pending: '<span class="spinner" aria-hidden="true"></span>', confirming: '<span class="spinner" aria-hidden="true"></span>' };
const STATE_TONES = { paid: 'ok', expired: 'warn', failed: 'danger', review: 'warn', underpaid: 'warn' };
function orderMessage(o, state, i) {
  const quotes = o.quotes || [];
  const seen = (o.transfers || []).find(t => t.status === 'seen');
  const confirmations = quotes.find(q => seen && q.chain === seen.chain)?.confirmations;
  const price = Number(o.price_usd), received = Number(o.received_usd);
  const tolerance = o.underpayment_tolerance_pct;
  const flagged = (o.transfers || []).find(t => t.note);
  switch (state) {
    case 'pending': return ['Waiting for your transfer', 'Send the amount shown below from your wallet or exchange. This window updates by itself.'];
    case 'confirming': return ['Payment detected', `The network has seen your transfer. Waiting for ${confirmations ? plural(confirmations, 'confirmation') : 'network confirmations'}. You can close this window: credit is added automatically.`];
    case 'underpaid': return ['Payment received, but it is not enough yet', `${usd(received)} of ${usd(price)} has been confirmed. Send the remaining ${usd(Math.max(0, price - received))} in any listed token to finish the order.${tolerance && Number(tolerance) > 0 ? ` Shortfalls up to ${tolerance}% are accepted.` : ''}${o.status === 'expired' ? ' The quote has expired: stablecoin transfers still count, native coin transfers need the operator.' : ''}`];
    case 'review': return ['A transfer needs review', `${flagged ? `${noteLabel(flagged.note)}. ` : ''}It was not counted towards this order. Contact the operator with the transaction hash shown below.`];
    case 'paid': return ['Payment confirmed', `${usdShort(i.credit || i.price)} of API credit was added. ${expiryText(i, o.paid_at)}.${received > price + 0.01 ? ` You sent ${usd(received)}: the plan credit is granted once, so ask the operator about the extra ${usd(received - price)}.` : ''}`];
    case 'expired': return ['This quote expired', 'Quotes are valid for a limited time. If you already sent a stablecoin it is still counted once it confirms. Otherwise start a new payment to get fresh amounts.'];
    default: return ['Payment not completed', 'This order was cancelled or failed. Contact the operator if you already sent funds.'];
  }
}
function pickQuote(quotes, sel) {
  const stable = q => q.rate_usd === '1' ? 0 : 1;
  const chosen = quotes.find(q => q.asset === sel.asset && q.chain === sel.chain)
    || quotes.filter(q => q.chain === sel.chain).sort((a, b) => stable(a) - stable(b))[0]
    || quotes.slice().sort((a, b) => stable(a) - stable(b))[0];
  if (chosen) Object.assign(sel, { chain: chosen.chain, asset: chosen.asset });
  return chosen;
}
function payCard(o, q, sel) {
  const uri = paymentUri(q);
  const content = sel.qr === 'link' ? uri : q.address;
  const confirmations = q.confirmations ? ` after ${plural(q.confirmations, 'confirmation')}` : '';
  return `<div class="pay-card">
      <div class="qr-box">${qrSvg(content, `QR code to pay ${q.amount} ${q.symbol} on ${q.chain_name}`) || qrSvg(q.address)}
        <div class="seg" role="group" aria-label="QR code content"><button type="button" data-qr="link" aria-pressed="${sel.qr === 'link'}">Payment request</button><button type="button" data-qr="address" aria-pressed="${sel.qr === 'address'}">Address only</button></div>
        <span class="hint">${sel.qr === 'link' ? 'Fills in the network, token, and amount in wallets that support payment requests.' : 'Works with any wallet or exchange: enter the amount yourself.'}</span>
      </div>
      <div class="pay-fields">
        <div class="pay-field"><span class="label">Amount to send</span><div class="pay-amount">${copyable(q.amount, `${q.symbol} amount`)}<span class="pay-symbol">${esc(q.symbol)}</span><button type="button" class="small" data-copy-value="${esc(q.amount)}" data-copy-label="${esc(q.symbol)} amount">Copy</button></div>${q.rate_usd !== '1' ? `<span class="hint">Quoted at ${esc(usd(q.rate_usd))} per ${esc(q.symbol)}</span>` : `<span class="hint">1 ${esc(q.symbol)} = ${esc(usd(1))}</span>`}</div>
        <div class="pay-field"><span class="label">To this address</span>${copyable(q.address, `${q.chain_name} address`)}<div class="actions"><button type="button" class="small" data-copy-value="${esc(q.address)}" data-copy-label="${esc(q.chain_name)} address">Copy address</button>${explorerLink(q.explorer, 'address', q.address, 'View on explorer')}</div></div>
        <div class="pay-field"><span class="label">Network</span><span>${esc(q.chain_name)}${q.chain_id ? ` <span class="hint">chain ID ${esc(q.chain_id)}</span>` : ''}</span></div>
        <div class="pay-field"><span class="label">${q.contract ? `${esc(q.symbol)} token contract` : 'Asset'}</span>${contractCell(q.explorer, q.contract)}</div>
      </div>
    </div>
    <div class="callout warn" role="note"><p><strong>Send only ${esc(q.symbol)} on ${esc(q.chain_name)}.</strong> Other tokens or networks can be lost. Send the amount in one transfer${confirmations ? `. It counts${esc(confirmations)}` : ''}. Several smaller transfers or tokens add up, and anything above the price is not credited.</p></div>`;
}
function cryptoView(o, sel) {
  const state = orderState(o);
  const plan = planOfOrder(o);
  const [title, text] = orderMessage(o, state, planInfo(plan));
  // After a partial payment, every quote shrinks to what is still owed.
  const price = Math.round(Number(o.price_usd) * 1e6), owed = Math.max(0, price - Math.round(Number(o.received_usd) * 1e6));
  const quotes = (o.quotes || []).map(x => {
    if (state !== 'underpaid' || price <= 0) return x;
    const units = (BigInt(x.amount_units) * BigInt(owed) + BigInt(price) - 1n) / BigInt(price) || 1n;
    return { ...x, amount_units: units.toString(), amount: fromUnits(units.toString(), x.decimals) };
  });
  const open = quotes.length && !['paid', 'failed', 'expired', 'review'].includes(state);
  // Start on the network and token of the latest transfer, when there is one.
  const latest = (o.transfers || []).at(-1);
  if (latest && !sel.chain) Object.assign(sel, { chain: latest.chain, asset: latest.asset });
  const q = open ? pickQuote(quotes, sel) : null;
  const networks = [...new Map(quotes.map(x => [x.chain, x.chain_name]))];
  const waiting = state === 'pending' && o.expires_at;
  return `${stepper(state === 'paid' ? 3 : 1)}
    <div class="pay-state tone-${STATE_TONES[state] || 'info'}" role="status">
      <span class="pay-state-icon" aria-hidden="true">${STATE_ICONS[state]}</span>
      <div><strong>${esc(title)}</strong><p>${esc(text)}</p>${waiting ? `<p class="pay-timer">Quote valid for <strong data-countdown="${esc(o.expires_at)}">${esc(countdown(o.expires_at - nowSeconds()))}</strong> (until ${esc(date(o.expires_at))})</p>` : ''}</div>
      ${pill(state === 'confirming' ? 'Detected' : state === 'underpaid' ? 'Underpaid' : state === 'review' ? 'Needs review' : o.status)}
    </div>
    ${state === 'failed' || state === 'expired' ? '' : payTimeline(state)}
    <div class="pay-grid">
      ${orderSummary(plan, { paidAt: o.paid_at })}
      <div>
        ${q ? `<fieldset class="pay-choose"><legend>1. Choose a network</legend>
          <div class="net-tabs" role="radiogroup" aria-label="Network">${networks.map(([chain, name]) => `<button type="button" role="radio" aria-checked="${chain === q.chain}" data-pick-chain="${esc(chain)}">${esc(name)}</button>`).join('')}</div></fieldset>
        <fieldset class="pay-choose"><legend>2. Choose what to pay with</legend>
          <div class="token-options" role="radiogroup" aria-label="Token">${quotes.filter(x => x.chain === q.chain).map(x => `<button type="button" role="radio" class="token-option" aria-checked="${x.asset === q.asset}" data-pick-asset="${esc(x.asset)}"><strong>${esc(x.amount)} ${esc(x.symbol)}</strong><span class="hint">${x.rate_usd === '1' ? 'Stablecoin, 1:1 with USD' : `Native coin at ${esc(usd(x.rate_usd))}`}</span></button>`).join('')}</div></fieldset>
        <h4 class="sub-head">3. Send the payment</h4>
        ${payCard(o, q, sel)}` : ''}
        ${state === 'paid' ? '<div class="actions pay-done"><button type="button" class="primary" data-pay-done>Done</button></div>' : ''}
        ${state === 'expired' || state === 'failed' ? '<div class="actions pay-done"><button type="button" class="primary" data-pay-again>Start a new payment</button></div>' : ''}
      </div>
    </div>
    ${transfersTable(o)}`;
}
// Shows a crypto order in a dialog and keeps it fresh until it settles. Polling
// backs off after errors and stops when the dialog closes or the order is final.
function runOrder(d, initial) {
  const box = d.querySelector('[data-pay]');
  const sel = { chain: '', asset: '', qr: 'link' };
  const control = new AbortController();
  let order = initial, timer, clock, failures = 0, focus = '';
  const signature = o => JSON.stringify([o.status, o.received_usd, (o.transfers || []).map(t => [t.id, t.status, t.value_usd, t.note])]);
  const final = () => ['paid', 'failed', 'cancelled'].includes(order.status) || (order.status === 'expired' && !(order.transfers || []).length);
  const tick = () => {
    const el = box.querySelector('[data-countdown]');
    if (!el) return;
    const left = Number(el.dataset.countdown) - nowSeconds();
    el.textContent = countdown(left);
    if (left <= 0 && !el.dataset.expired) {
      el.dataset.expired = '1';
      clearTimeout(timer);
      poll();
    }
  };
  const draw = () => {
    box.innerHTML = cryptoView(order, sel);
    wireCopies(box);
    box.querySelector('[data-pay-done]')?.addEventListener('click', () => d.close());
    box.querySelector('[data-pay-again]')?.addEventListener('click', () => {
      const plan = planCatalog.plans.find(p => p.code === order.plan_code);
      d.close();
      if (plan) checkout(plan, planCatalog.methods);
    });
    const choose = (attr, apply) => box.querySelectorAll(`[${attr}]`).forEach(b => b.addEventListener('click', () => {
      apply(b.getAttribute(attr));
      focus = `[${attr}="${CSS.escape(b.getAttribute(attr))}"]`;
      draw();
    }));
    choose('data-pick-chain', chain => Object.assign(sel, { chain, asset: '' }));
    choose('data-pick-asset', asset => { sel.asset = asset; });
    choose('data-qr', mode => { sel.qr = mode; });
    if (focus) box.querySelector(focus)?.focus();
    focus = '';
    tick();
  };
  const schedule = () => {
    clearTimeout(timer);
    if (d.open && !final()) timer = setTimeout(poll, Math.min(60_000, 5_000 * 2 ** failures));
  };
  async function poll() {
    if (!d.open) return;
    try {
      const next = await api(`/v1/account/payments/${encodeURIComponent(order.id)}`, 'GET', undefined, { signal: control.signal });
      failures = 0;
      const changed = signature(next) !== signature(order);
      const wasPaid = order.status === 'paid';
      order = next;
      if (changed) draw();
      if (order.status === 'paid' && !wasPaid) {
        notice('Payment confirmed, credit added');
        if (tab === 'billing') load();
      }
    } catch (e) {
      if (e.name === 'AbortError') return;
      failures = Math.min(failures + 1, 4);
    }
    schedule();
  }
  draw();
  clock = setInterval(tick, 1000);
  schedule();
  d.addEventListener('close', () => {
    control.abort();
    clearTimeout(timer);
    clearInterval(clock);
  });
}
function cryptoOrder(initial) {
  modal('Pay with crypto', '<div data-pay></div>', d => runOrder(d, initial), 'wide');
}
// A customer's view of one of their payments, card or crypto.
async function receiptModal(id) {
  const o = await api(`/v1/account/payments/${encodeURIComponent(id)}`);
  const plan = planOfOrder(o);
  const payable = o.method === 'crypto' && o.status === 'pending';
  modal(`Payment ${shortId(o.id)}`, `
    <div class="tiles">
      ${metric('Price', usd(o.price_usd), `${esc(plan.name)}, ${esc(methodLabel(o.method))}`)}
      ${metric('Received', usd(o.received_usd), o.status === 'paid' ? `Credit of ${esc(usd(o.credit_usd))} granted` : 'Counted so far')}
      ${metric('Status', pill(o.status), o.paid_at ? `Paid ${esc(date(o.paid_at))}` : o.expires_at && o.status !== 'paid' ? `Quote ${o.status === 'expired' ? 'expired' : 'expires'} ${esc(date(o.expires_at))}` : `Created ${esc(date(o.created_at))}`, { html: true })}
    </div>
    <section class="detail-section"><h4>Receipt</h4>${facts([
      ['Payment ID', copyable(o.id, 'Payment ID')],
      ['Plan', esc(plan.name)],
      ['Method', esc(methodLabel(o.method))],
      ['Created', esc(date(o.created_at))],
      o.paid_at && ['Paid', esc(date(o.paid_at))],
      ['Credit', `${esc(usd(o.credit_usd))} for ${esc(plural(o.duration_days, 'day'))}`],
      o.paid_at && ['Credit valid until', esc(date(o.paid_at + o.duration_days * DAY))],
    ])}</section>
    ${transfersTable(o)}
    ${payable ? '<div class="actions"><button type="button" class="primary" data-continue>Continue payment</button></div>' : ''}`, d => {
    wireCopies(d);
    d.querySelector('[data-continue]')?.addEventListener('click', () => {
      d.close();
      cryptoOrder(o);
    });
  }, 'wide');
}
async function paymentDetails(id) {
  const o = await api(endpoint(`payments/${encodeURIComponent(id)}`));
  const payable = o.status === 'pending' || o.status === 'expired';
  modal(`Payment ${shortId(o.id)}`, `
    <div class="tiles">
      ${metric('Price', usd(o.price_usd), `${esc(o.plan_code)}, ${esc(methodLabel(o.method))}`)}
      ${metric('Received', usd(o.received_usd), `Grants ${esc(usd(o.credit_usd))} for ${esc(plural(o.duration_days, 'day'))}`)}
      ${metric('Status', pill(o.status), o.paid_at ? `Paid ${esc(date(o.paid_at))}${o.paid_by ? ` by ${esc(o.paid_by)}` : ''}` : `Created ${esc(date(o.created_at))}`, { html: true })}
    </div>
    <section class="detail-section"><h4>Order</h4>${facts([
      ['Order ID', copyable(o.id, 'Order ID')],
      ['Account', `${accountLink(o.account_id)} ${goButton('Account payments', 'payments', { account: o.account_id }, 'link')}`],
      ['Created', esc(date(o.created_at))],
      o.expires_at && ['Quote expires', esc(date(o.expires_at))],
    ])}</section>
    ${(o.quotes || []).length ? `<section class="detail-section"><h4>Quotes</h4>${table([
      ['Asset', q => `<strong>${esc(q.symbol)}</strong><div class="hint">${esc(q.chain_name)}</div>`],
      ['Amount', q => `<span class="num">${esc(q.amount)}</span>`],
      ['Rate', q => esc(usd(q.rate_usd))],
      ['Address', q => copyable(q.address, 'Address', shortHash(q.address))],
    ], o.quotes)}</section>` : ''}
    ${transfersTable(o)}
    ${payable ? `<div class="callout warn"><p>Use after verifying a late, underpaid, or otherwise flagged payment by hand. Credit is granted once.</p><div class="actions"><button type="button" class="primary" data-complete>Mark as paid and grant credit</button></div></div>` : ''}`, d => {
    wireCopies(d);
    d.querySelector('[data-complete]')?.addEventListener('click', async e => {
      if (!(await confirmModal('Complete payment?', 'Grant this order\'s plan credit to the account now?', 'Grant credit'))) return;
      e.target.disabled = true;
      try {
        await api(endpoint(`payments/${encodeURIComponent(id)}/complete`), 'POST', {});
        d.close();
        notice('Payment completed');
        await load();
      } catch (err) {
        notice(err.message, true);
        e.target.disabled = false;
      }
    });
  }, 'wide');
}
async function payroofSetup() {
  const s = await api(endpoint('payroof'));
  const yes = v => v ? pill('configured', 'ok') : pill('missing', 'warn');
  modal('Crypto payments (Payroof)', `
    <p class="muted">Configure <code>payroof.project</code> and <code>payroof.token</code> in Settings, then register the webhook so confirmed transfers complete orders automatically.</p>
    ${facts([
      ['Status', s.enabled ? pill('enabled', 'ok') : pill('disabled', 'warn')],
      ['API', `<code>${esc(s.api_url)}</code>`],
      ['Project', s.project ? `<code>${esc(s.project)}</code>` : pill('missing', 'warn')],
      ['Project token', yes(s.token_configured)],
      ['Webhook secret', yes(s.webhook_secret_configured)],
      ['Webhook URL', `${copyable(s.webhook_url, 'Webhook URL')} ${s.webhook_registered ? pill('registered', 'ok') : pill('not registered', 'warn')}`],
    ])}
    ${s.project_error ? `<div class="callout danger"><p>${esc(s.project_error)}</p></div>` : ''}
    ${s.webhooks_error ? `<p class="hint">Webhooks: ${esc(s.webhooks_error)}</p>` : ''}
    ${(s.chains || []).length ? `<section class="detail-section"><h4>Watched networks</h4>${table([
      ['Network', c => `<strong>${esc(c.name)}</strong><div class="hint"><code>${esc(c.id)}</code></div>`],
      ['Assets', c => esc((c.assets || []).map(x => x.symbol).join(', ')), 'wrap'],
      ['Confirmations', c => `<span class="num">${count(c.confirmations)}</span>`, 'end'],
    ], s.chains)}</section>` : ''}
    <div class="actions section"><button type="button" class="primary" data-register${s.project && s.token_configured ? '' : ' disabled'}>Register webhook</button></div>
    <p class="field-help">Registering subscribes the webhook URL to seen and confirmed events and stores the returned signing secret. Payroof only delivers to public HTTPS URLs.</p>`, d => {
    wireCopies(d);
    d.querySelector('[data-register]').onclick = async e => {
      e.target.disabled = true;
      try {
        const v = await api(endpoint('payroof/webhook'), 'POST', {});
        notice(v.notice);
        d.close();
        payroofSetup();
      } catch (err) {
        notice(err.message, true);
        e.target.disabled = false;
      }
    };
  }, 'wide');
}

/* OAuth consent ------------------------------------------------------------------
   /oauth/authorize?... is opened by an MCP client. The page shows who is asking,
   lets the user prove which account to use (stored session, pasted credential, or
   a brand-new account), and posts the decision. The server answers with the URL to
   send the browser back to. */

const maskSecret = t => `${t.slice(0, t.startsWith('dash_') ? 9 : 15)}...`;
// Redirects come from the server, but never navigate to script-capable schemes.
function safeRedirect(url) {
  try {
    const u = new URL(url, location.origin);
    return ['javascript:', 'data:', 'vbscript:', 'file:', 'blob:'].includes(u.protocol) ? '' : u.href;
  } catch {
    return '';
  }
}
const httpsUrl = v => {
  try {
    const u = new URL(v);
    return u.protocol === 'https:' ? u : null;
  } catch {
    return null;
  }
};

function consentFrame(inner) {
  main.innerHTML = `<div class="consent"><section class="card consent-card" aria-labelledby="consent-title">${inner}</section></div>`;
  main.removeAttribute('aria-busy');
  const heading = main.querySelector('#consent-title');
  heading?.setAttribute('tabindex', '-1');
  heading?.focus({ preventScroll: true });
}
function consentMessage({ ok = false, title, text, redirect = '', returnLabel = 'Return to app' }) {
  consentFrame(`<div class="consent-state" role="${ok ? 'status' : 'alert'}">
      <div class="badge${ok ? '' : ' bad'}" aria-hidden="true">${ok ? '&#10003;' : '!'}</div>
      <h1 id="consent-title">${esc(title)}</h1>
      <p>${esc(text)}</p>
      <div class="actions">${redirect
        ? `<button type="button" class="primary" data-return>${esc(returnLabel)}</button>`
        : `<a class="button ghost" href="/">Back to ${esc(brandName)}</a>`}</div>
    </div>`);
  main.querySelector('[data-return]')?.addEventListener('click', () => location.assign(redirect));
}

async function oauthConsent() {
  const query = location.search.replace(/^\?/, '');
  main.setAttribute('aria-busy', 'true');
  main.innerHTML = consentSkeleton();
  let info;
  try {
    if (!query) throw new Error('This page opens from an app that wants to connect. Start the connection from your MCP client, such as Claude, ChatGPT, Cursor, or VS Code.');
    info = await api(`/oauth/api/authorize?${query}`);
  } catch (e) {
    consentMessage({ title: 'This request cannot be completed', text: e.message, redirect: safeRedirect(e.redirect) });
    return;
  }

  const client = info.client || {};
  const clientName = client.client_name || client.client_id || 'An application';
  const clientSite = httpsUrl(client.client_uri);
  const logo = httpsUrl(client.logo_uri);
  const brandLogo = document.querySelector('.mark')?.getAttribute('src') || '';
  const host = info.redirect_host || (() => { try { return new URL(info.redirect_uri).host; } catch { return ''; } })();
  const stored = sessionStorage.getItem(CUSTOMER_STORE) || '';
  let modes = [...(stored ? [['session', 'Continue as you'] ] : []), ['paste', 'API key or token'], ['create', 'New account']];
  let mode = modes[0][0];
  let created = null;
  let busy = false;

  consentFrame(`<form class="consent-form" novalidate>
      <div class="consent-logos" aria-hidden="true">
        <span class="consent-logo">${brandLogo ? `<img src="${esc(brandLogo)}" alt="" width="52" height="52">` : esc([...brandName][0] || '')}</span>
        <span class="consent-link"></span>
        <span class="consent-logo" id="client-logo">${logo ? `<img src="${esc(logo.href)}" alt="" width="52" height="52" referrerpolicy="no-referrer">` : esc([...clientName][0].toUpperCase())}</span>
      </div>
      <h1 class="consent-title" id="consent-title"><strong>${esc(clientName)}</strong> wants to connect to your ${esc(brandName)} account</h1>
      <p class="consent-sub">${host ? `You return to <code>${esc(host)}</code> afterwards.` : ''}${clientSite ? ` App site: <a href="${esc(clientSite.href)}" target="_blank" rel="noopener noreferrer">${esc(clientSite.host)}</a>` : ''}</p>
      <div class="consent-section">
        <h2 id="perm-title">This app will be able to</h2>
        <ul class="auth-points consent-perms" aria-labelledby="perm-title">
          <li>Generate text, speech, images, and transcripts, billed to your balance</li>
          <li>View your balance, usage, and keys</li>
        </ul>
        <p class="hint">You can disconnect it at any time from your workspace.</p>
      </div>
      <div class="consent-section">
        <h2 id="auth-title">Authorize with</h2>
        <div class="seg" role="tablist" aria-labelledby="auth-title"></div>
        <div class="consent-panel" data-panel="session" role="tabpanel" hidden>
          <div class="consent-identity"><span class="avatar" aria-hidden="true">${esc([...brandName][0] || '')}</span><div><strong id="session-name">Your signed-in account</strong><span class="hint">${esc(stored ? maskSecret(stored) : '')}</span></div></div>
        </div>
        <div class="consent-panel" data-panel="paste" role="tabpanel" hidden>
          <label for="consent-credential">API key or dashboard token</label>
          <div class="pw-field">
            <input id="consent-credential" name="credential" type="password" autocomplete="off" spellcheck="false" autocapitalize="off" placeholder="sk-res-... or dash_..." aria-describedby="credential-help">
            <button type="button" class="ghost" data-toggle-secret aria-pressed="false" aria-controls="consent-credential">Show</button>
          </div>
          <p class="field-help" id="credential-help">Sent only to ${esc(brandName)} to confirm your account. It is never shared with the app.</p>
        </div>
        <div class="consent-panel" data-panel="create" role="tabpanel" hidden>
          <div data-create-form>
            <label for="consent-name">Key name</label>
            <input id="consent-name" name="name" maxlength="80" value="${esc(clientName.slice(0, 80))}" autocomplete="off">
            <label for="consent-contact">Contact (optional)</label>
            <input id="consent-contact" name="contact" type="email" placeholder="you@example.com" autocomplete="email">
            <p class="field-help">New accounts receive evaluation credit. No card needed.</p>
            <div class="pow" role="status" aria-live="polite" hidden><div class="loading"><span class="spinner" aria-hidden="true"></span><span>Verifying you're human...</span></div><progress max="100" value="0"></progress></div>
            <div class="actions"><button type="button" class="ghost" data-make-account>Create account</button></div>
          </div>
          <div data-created hidden>
            <div class="callout info"><strong>Account created</strong><p>Save your credentials, then choose Allow to finish connecting.</p><div class="actions"><button type="button" class="small" data-show-secrets>Show credentials</button></div></div>
          </div>
        </div>
      </div>
      <div class="consent-error" role="alert" hidden></div>
      <div class="consent-buttons">
        <button type="button" class="ghost" data-deny>Deny</button>
        <button type="submit" class="primary" data-allow>Allow</button>
      </div>
      <p class="consent-foot">Only approve apps you trust.</p>
    </form>`);

  const form = main.querySelector('form');
  const errorBox = form.querySelector('.consent-error');
  const allow = form.querySelector('[data-allow]');
  const deny = form.querySelector('[data-deny]');
  const input = form.querySelector('#consent-credential');
  const tablist = form.querySelector('.seg');
  form.querySelector('#client-logo img')?.addEventListener('error', e => { e.target.replaceWith(clientName[0].toUpperCase()); });

  const credential = () => mode === 'session' ? stored : mode === 'paste' ? input.value.trim() : created?.key || '';
  const showError = message => {
    errorBox.textContent = message;
    errorBox.hidden = !message;
  };
  const refresh = () => {
    allow.disabled = busy || !credential();
    deny.disabled = busy;
  };
  const paintTabs = () => {
    tablist.innerHTML = modes.map(([id, label]) => `<button type="button" role="tab" id="mode-${id}" data-mode="${id}" aria-selected="${id === mode}" aria-controls="panel-${id}" tabindex="${id === mode ? 0 : -1}">${esc(label)}</button>`).join('');
    form.querySelectorAll('[data-panel]').forEach(p => {
      p.id = `panel-${p.dataset.panel}`;
      p.setAttribute('aria-labelledby', `mode-${p.dataset.panel}`);
      p.hidden = p.dataset.panel !== mode;
    });
    tablist.querySelectorAll('[data-mode]').forEach((b, i, all) => {
      b.onclick = () => setMode(b.dataset.mode);
      b.onkeydown = e => {
        const target = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: all.length - 1 }[e.key];
        if (target === undefined) return;
        e.preventDefault();
        const next = all[(target + all.length) % all.length];
        next.focus();
        next.click();
      };
    });
  };
  function setMode(next) {
    mode = next;
    showError('');
    paintTabs();
    refresh();
  }
  paintTabs();
  refresh();
  input.addEventListener('input', () => { showError(''); refresh(); });
  form.querySelector('[data-toggle-secret]').onclick = e => {
    const show = input.type === 'password';
    input.type = show ? 'text' : 'password';
    e.currentTarget.textContent = show ? 'Hide' : 'Show';
    e.currentTarget.setAttribute('aria-pressed', String(show));
    input.focus();
  };

  // A stored session is checked in the background: show who it is, or drop it when stale.
  if (stored) {
    api('/v1/account', 'GET', undefined, { token: stored }).then(v => {
      const name = form.querySelector('#session-name');
      if (name) name.textContent = `Continue as ${v.account?.contact || shortId(v.account?.id)}`;
    }).catch(e => {
      if (e.status !== 401 && e.status !== 403) return;
      modes = modes.filter(([id]) => id !== 'session');
      if (mode === 'session') mode = 'paste';
      paintTabs();
      refresh();
      showError('Your saved session has expired. Paste a key or create an account.');
    });
  }

  // New account: reuse the in-browser signup, then authorize with the key it returns.
  const makeButton = form.querySelector('[data-make-account]');
  makeButton.onclick = async () => {
    const pow = form.querySelector('.pow');
    const bar = pow.querySelector('progress');
    showError('');
    makeButton.disabled = true;
    busy = true;
    refresh();
    pow.hidden = false;
    try {
      created = await signup({ name: form.querySelector('#consent-name').value.trim() || clientName.slice(0, 80), contact: form.querySelector('#consent-contact').value.trim() }, p => { bar.value = Math.round(p * 100); });
      form.querySelector('[data-create-form]').hidden = true;
      form.querySelector('[data-created]').hidden = false;
      secrets(created, { embedded: true });
    } catch (e) {
      showError(e.message);
    } finally {
      pow.hidden = true;
      makeButton.disabled = false;
      busy = false;
      refresh();
    }
  };
  form.querySelector('[data-show-secrets]').onclick = () => created && secrets(created, { embedded: true });

  const finish = (decision, redirect) => {
    const target = safeRedirect(redirect);
    if (decision === 'allow') {
      consentMessage({ ok: true, title: `${clientName} is connected`, text: `You can close this tab and return to ${clientName}.`, redirect: target, returnLabel: `Return to ${clientName}` });
    } else {
      consentMessage({ ok: true, title: 'Access denied', text: `${clientName} was not given access. You can close this tab.`, redirect: target, returnLabel: `Return to ${clientName}` });
    }
    if (target) location.assign(target);
  };
  const decide = async decision => {
    if (busy) return;
    showError('');
    busy = true;
    const label = (decision === 'allow' ? allow : deny);
    const original = label.textContent;
    label.textContent = decision === 'allow' ? 'Authorizing...' : 'Denying...';
    allow.disabled = deny.disabled = true;
    form.setAttribute('aria-busy', 'true');
    try {
      const v = await api('/oauth/api/authorize', 'POST', { query, decision, credential: credential() });
      finish(decision, v.redirect);
    } catch (e) {
      if (decision === 'deny' && info.redirect_uri) {
        // Denying needs no account. Fall back to the standard OAuth error redirect.
        try {
          const u = new URL(info.redirect_uri);
          u.searchParams.set('error', 'access_denied');
          const state = new URLSearchParams(query).get('state');
          if (state) u.searchParams.set('state', state);
          finish('deny', u.href);
          return;
        } catch { /* fall through to the error message */ }
      }
      label.textContent = original;
      showError(e.message);
      if (mode === 'paste' && e.status === 401) input.focus();
    } finally {
      busy = false;
      form.removeAttribute('aria-busy');
      if (form.isConnected) refresh();
    }
  };
  form.addEventListener('submit', e => {
    e.preventDefault();
    if (credential()) decide('allow');
  });
  deny.onclick = () => decide('deny');
}

/* Interaction wiring --------------------------------------------------------------- */

function wire(root) {
  root.querySelectorAll('[data-refresh]').forEach(b => b.onclick = load);
  root.querySelectorAll('[data-create]').forEach(b => b.onclick = keyForm);
  wireCharts(root);
  wireShares(root);
  wireDocs(root);
  wireCopies(root);
  wireTopUp(root);
  root.querySelectorAll('[data-scroll]').forEach(b => b.onclick = () => {
    const target = document.getElementById(b.dataset.scroll);
    if (!target) return;
    target.scrollIntoView({ behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth', block: 'start' });
    target.focus({ preventScroll: true });
  });
  root.querySelectorAll('[data-list-form]').forEach(form => {
    form.addEventListener('submit', e => {
      e.preventDefault();
      Object.assign(listStates[form.dataset.listForm], Object.fromEntries(new FormData(form)), { offset: 0 });
      syncHash();
      load();
    });
  });
  root.querySelectorAll('[data-list-clear]').forEach(button => button.onclick = () => {
    const name = button.dataset.listClear;
    Object.assign(listStates[name], LIST_DEFAULTS[name], { limit: listStates[name].limit });
    syncHash();
    load();
  });
  root.querySelectorAll('[data-chip-remove]').forEach(button => button.onclick = () => {
    const state = listStates[button.dataset.chipRemove];
    state[button.dataset.chipField] = '';
    state.offset = 0;
    syncHash();
    load();
  });
  root.querySelectorAll('[data-list-size]').forEach(select => select.onchange = () => {
    const state = listStates[select.dataset.listSize];
    state.limit = Number(select.value);
    state.offset = 0;
    load();
  });
  root.querySelectorAll('[data-list-prev]').forEach(button => button.onclick = () => {
    const state = listStates[button.dataset.listPrev];
    state.offset = Math.max(0, state.offset - state.limit);
    load();
  });
  root.querySelectorAll('[data-list-next]').forEach(button => button.onclick = () => {
    const state = listStates[button.dataset.listNext];
    state.offset += state.limit;
    load();
  });

  const action = (selector, fn) => root.querySelectorAll(selector).forEach(b => b.onclick = async () => {
    b.disabled = true;
    try {
      await fn(b);
    } catch (e) {
      notice(e.message, true);
    } finally {
      b.disabled = false;
    }
  });

  action('[data-rotate]', async b => {
    if (await rotateKey(b.dataset.rotate)) await load();
  });
  action('[data-edit-key]', async b => editKeyForm(b.dataset.editKey, b.dataset.keyName));
  action('[data-plan]', async () => planForm());
  action('[data-edit-plan]', async b => planForm({
    code: b.dataset.editPlan,
    name: b.dataset.planName,
    price_usd: b.dataset.planPrice,
    credit_usd: b.dataset.planCredit,
    duration_days: Number(b.dataset.planDays),
    active: b.dataset.planActive === 'true',
    description: b.dataset.planDescription,
  }));
  action('[data-delete-plan]', async b => {
    const code = b.dataset.deletePlan;
    if (!(await confirmModal('Delete plan?', `Delete "${code}"? This cannot be undone. Plans with subscriptions, payments, or codes can only be deactivated.`, 'Delete plan', true))) return;
    await api(endpoint(`plans/${encodeURIComponent(code)}`), 'DELETE');
    notice('Plan deleted');
    await load();
  });
  action('[data-edit-code]', async b => editCodeForm(b));
  action('[data-delete-code]', async b => {
    if (!(await confirmModal('Delete code?', 'Cancel this redeem code? Anyone holding it will no longer be able to redeem it.', 'Delete code', true))) return;
    await api(endpoint(`codes/${b.dataset.deleteCode}`), 'DELETE');
    notice('Code deleted');
    await load();
  });
  action('[data-code]', async () => codeForm());
  action('[data-checkout]', async b => {
    const plan = planCatalog.plans.find(p => p.code === b.dataset.checkout);
    if (plan) checkout(plan, planCatalog.methods);
  });
  action('[data-order]', async b => cryptoOrder(await api(`/v1/account/payments/${encodeURIComponent(b.dataset.order)}`)));
  action('[data-payment]', async b => paymentDetails(b.dataset.payment));
  action('[data-receipt]', async b => receiptModal(b.dataset.receipt));
  action('[data-payroof]', async () => payroofSetup());
  action('[data-revoke-app]', async b => {
    if (!(await confirmModal('Disconnect app?', `${b.dataset.appName} will lose access to your account right away. You can connect it again later.`, 'Disconnect', true))) return;
    // The body may be empty, so read the status instead of requiring JSON.
    const r = await fetch(`/v1/account/apps/${encodeURIComponent(b.dataset.revokeApp)}`, { method: 'DELETE', headers: { authorization: `Bearer ${token}` } });
    if (!r.ok) throw apiError(r, await r.json().catch(() => null));
    notice('App disconnected');
    await load();
  });

  root.querySelectorAll('[data-redeem]').forEach(b => b.onclick = redeemForm);
  if (root.querySelector('#settings')) bindForm(root.querySelector('#settings'), async b => {
    const updates = JSON.parse(b.json);
    await api(endpoint('settings'), 'PATCH', updates);
    if (updates.admin_token) {
      token = updates.admin_token;
      sessionStorage.setItem(store, token);
    }
    notice('Settings saved');
  });
}

// Entity links and cross-page navigation, in views and dialogs alike.
document.addEventListener('click', e => {
  const el = e.target.closest('[data-open-account],[data-open-key],[data-go],[data-request-details]');
  if (!el || el.disabled) return;
  if (el.dataset.openAccount) manageAccount(el.dataset.openAccount);
  else if (el.dataset.openKey) keyDetails(el.dataset.openKey);
  else if (el.dataset.go) go(el.dataset.go, Object.fromEntries(new URLSearchParams(el.dataset.goParams || '')));
  else if (el.dataset.requestDetails) {
    const request = requestIndex.get(el.dataset.requestDetails);
    if (request) requestDetails(request);
    else notice('Request details are no longer available. Refresh the list.', true);
  }
});

// Stripe returns to /workspace/?payment=success|cancelled#billing.
const paymentResult = new URLSearchParams(location.search).get('payment');
if (paymentResult) {
  history.replaceState(null, '', location.pathname + location.hash);
  notice(paymentResult === 'success'
    ? 'Payment received. Credit appears as soon as the provider confirms it.'
    : 'Checkout cancelled. You have not been charged.', paymentResult !== 'success');
}
renderHeader();
const boot = oauthPage ? oauthConsent
  : docsPage ? docs
  : !admin && !workspace ? landing
  : token ? shell
  : login;
const first = bootSkeleton();
if (first) main.innerHTML = first;
main.setAttribute('aria-busy', 'true');
Promise.resolve(boot()).catch(e => {
  main.innerHTML = `<div class="card error"><strong>Something went wrong</strong><p>${esc(e.message)}</p></div>`;
}).finally(() => main.removeAttribute('aria-busy'));
