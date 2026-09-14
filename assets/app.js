'use strict';

/* Boot state ----------------------------------------------------------------- */
const main = document.querySelector('#main');
const admin = location.pathname.startsWith('/admin');
const workspace = location.pathname.startsWith('/workspace');
const store = admin ? 'reseller.admin' : 'reseller.customer';
let token = sessionStorage.getItem(store) || '';
let tab = 'overview';
const fragment = new URLSearchParams(location.hash.slice(1));
if (workspace && fragment.has('token')) {
  token = fragment.get('token');
  sessionStorage.setItem(store, token);
  history.replaceState(null, '', location.pathname + location.search);
}

/* Helpers -------------------------------------------------------------------- */
const esc = v => String(v ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const usd = v => new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', minimumFractionDigits: 2, maximumFractionDigits: 6 }).format(Number(v || 0));
const date = v => v ? new Date(v * 1000).toLocaleString() : 'Never';
const count = v => Number(v || 0).toLocaleString();
const planRate = p => Number(p.price_usd) / Number(p.duration_days || 1);
const bestCode = plans => plans.slice().sort((a, b) => planRate(a) - planRate(b))[0]?.code;
const TONES = { active:'ok', paid:'ok', available:'ok', completed:'ok', blocked:'warn', suspended:'warn', expired:'warn', pending:'info', redeemed:'info', revoked:'danger', failed:'danger', cancelled:'danger' };
const pill = (value, t) => `<span class="pill tone-${t || TONES[String(value).toLowerCase()] || 'muted'}">${esc(value)}</span>`;
const requestPill = r => r.status >= 400 ? pill(r.status, 'danger') : r.status ? pill(r.status, 'ok') : pill('In progress', 'info');
const KEY_NAMES = [
  'My application', 'Production', 'Staging', 'Development', 'Mobile app',
  'Backend service', 'Data pipeline', 'Chat assistant', 'Automation', 'Playground',
  'Local tests', 'CI pipeline', 'Agent runtime', 'Voice demo', 'Translation service',
  'Analytics', 'Internal tools', 'Sandbox', 'Batch jobs', 'Scheduled tasks',
];
const suggestedName = () => KEY_NAMES[Math.floor(Math.random() * KEY_NAMES.length)];

function renderHeader() {
  const el = document.querySelector('#header-actions');
  if (!el) return;
  if (token) {
    el.innerHTML = '<button id="header-logout" class="ghost">Sign out</button>';
    el.querySelector('#header-logout').onclick = () => {
      sessionStorage.removeItem(store);
      token = '';
      history.replaceState(null, '', location.pathname);
      renderHeader();
      if (workspace || admin) login();
    };
  } else {
    el.innerHTML = '<a class="button" href="/workspace/">Workspace →</a>';
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
    notice('Copy failed — select the value manually.', true);
    return false;
  }
};
function flash(el, ms = 1400) {
  if (!el) return;
  el.classList.add('copied');
  clearTimeout(el._flashTimer);
  el._flashTimer = setTimeout(() => el.classList.remove('copied'), ms);
}
function flashCopied(button) {
  if (!button) return;
  if (!button.dataset.label) button.dataset.label = button.textContent;
  button.classList.add('copied');
  button.textContent = '✓ Copied';
  clearTimeout(button._flashTimer);
  button._flashTimer = setTimeout(() => {
    button.classList.remove('copied');
    button.textContent = button.dataset.label;
  }, 1400);
}

async function api(path, method = 'GET', data) {
  const headers = {};
  if (token) headers[admin ? 'x-admin-token' : 'authorization'] = admin ? token : `Bearer ${token}`;
  if (data !== undefined) headers['content-type'] = 'application/json';
  const r = await fetch(path, { method, headers, body: data === undefined ? undefined : JSON.stringify(data) });
  const v = await r.json();
  if (!r.ok) throw new Error(v.error?.message || `Request failed (${r.status})`);
  return v;
}
const endpoint = name => admin ? `/admin/api/${name}` : `/v1/${name}`;

function table(columns, rows) {
  if (!rows.length) return emptyState('Nothing here yet', 'Activity will appear here as it happens.');
  return `<div class="table-wrap"><table><thead><tr>${columns.map(c => `<th scope="col">${esc(c[0])}</th>`).join('')}</tr></thead><tbody>${rows.map(r => `<tr>${columns.map(c => `<td>${c[1](r)}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`;
}
function metric(label, value, hint) {
  return `<div class="card metric-card"><div class="label">${esc(label)}</div><div class="metric">${esc(value)}</div>${hint ? `<div class="hint">${esc(hint)}</div>` : ''}</div>`;
}
function emptyState(title, text) {
  return `<div class="empty"><strong>${esc(title)}</strong><br>${esc(text)}</div>`;
}
function loading(text = 'Loading…') {
  return `<div class="loading"><span class="spinner" aria-hidden="true"></span>${esc(text)}</div>`;
}

function modal(title, content, setup, className) {
  const d = document.createElement('dialog');
  if (className) d.classList.add(className);
  d.innerHTML = `<div class="row"><h2>${esc(title)}</h2><button data-close aria-label="Close dialog">✕</button></div>${content}`;
  document.body.append(d);
  d.querySelector('[data-close]').onclick = () => d.close();
  d.addEventListener('close', () => d.remove());
  d.showModal();
  setup?.(d);
  return d;
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
        <button data-cancel class="ghost">Cancel</button>
        <button data-confirm class="${danger ? 'danger-solid' : 'primary'}">${esc(confirmLabel)}</button>
      </div>`, el => {
      el.querySelector('[data-cancel]').onclick = () => d.close();
      el.querySelector('[data-confirm]').onclick = () => { d.close(); resolve(true); };
    });
    d.addEventListener('close', () => resolve(false));
  });
}

function planCard(p, best, cta) {
  const featured = p.code === best;
  return `<article class="card plan-card${featured ? ' featured' : ''}">
    ${featured ? '<span class="plan-flag">Best value</span>' : ''}
    <div class="plan-name">${esc(p.name)}</div>
    <div class="plan-price">${esc(usd(p.price_usd))}<small> / ${count(p.duration_days)} days</small></div>
    <p class="plan-desc">${esc(p.description || 'Prepaid API credit.')}</p>
    <div class="plan-cta">${cta || `<a class="button${featured ? ' primary' : ''}" href="/workspace/">Choose plan →</a>`}</div>
  </article>`;
}

function secrets(v) {
  modal('Save your credentials', `
    <p class="muted">These are shown only once. Keep the dashboard token to manage your account even if you block your API key.</p>
    <div class="cred-row">
      <span class="cred-label">API key</span>
      <code class="cred-value secret" title="${esc(v.key)}">${esc(v.key)}</code>
      <button class="small" data-copy="key">Copy</button>
    </div>
    <div class="cred-row">
      <span class="cred-label" title="Dashboard token">Dashboard</span>
      <code class="cred-value" title="${esc(v.dashboard_token)}">${esc(v.dashboard_token)}</code>
      <button class="small" data-copy="token">Copy</button>
    </div>
    <div class="actions section">
      <button data-copy-all class="primary">Copy both</button>
      ${admin ? '' : '<button data-open class="ghost">Open workspace →</button>'}
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
      sessionStorage.setItem('reseller.customer', v.dashboard_token);
      location.href = '/workspace/';
    });
  }, 'wide');
}

function keyForm() {
  modal('Create an API key', `
    <form>
      <label for="key-name">Key name</label>
      <input id="key-name" name="name" maxlength="80" value="${esc(suggestedName())}" placeholder="My application" required>
      <label for="contact">Contact (optional)</label>
      <input id="contact" name="contact" type="email" placeholder="you@example.com">
      <p class="section">New accounts receive evaluation credit. Additional keys share your existing balance.</p>
      <div class="actions"><button class="primary" type="submit">Create key →</button></div>
    </form>`, d => bindForm(d.querySelector('form'), async b => {
    const v = await api('/v1/keys', 'POST', b);
    d.close();
    secrets(v);
    if (document.querySelector('#content')) await load();
  }));
}

/* Account management modals -------------------------------------------------- */

function manageAccount(aid) {
  const d = modal('Manage account', `<div class="manage-body">${loading('Loading account…')}</div>`, null, 'wide');
  const body = d.querySelector('.manage-body');
  const refresh = async () => {
    try {
      render(await api(endpoint(`accounts/${aid}`)));
    } catch (e) {
      body.innerHTML = `<div class="card error">${esc(e.message)}</div>`;
    }
  };
  const render = v => {
    const a = v.account;
    const sub = a.subscription;
    const keys = v.keys || [];
    const credits = v.credits || [];
    const billed = (v.usage?.series || []).reduce((sum, s) => sum + Number(s.billed_cost_usd), 0);
    const now = Date.now() / 1000;
    body.innerHTML = `
      <div class="account-head">
        <div class="account-id">
          <span class="cred-label">Account</span>
          <code title="${esc(a.id)} — click to copy" data-copy-id="${esc(a.id)}">${esc(a.id.slice(0, 8))}…</code>
        </div>
        <div class="actions">
          ${pill(a.status)}
          ${a.status === 'active' ? '<button class="danger small" data-suspend>Suspend</button>' : '<button class="small" data-activate>Reactivate</button>'}
        </div>
      </div>
      <div class="account-summary">
        <span><strong>${esc(usd(a.balance_usd))}</strong> balance</span>
        ${a.debt_usd !== '0' ? `<span class="warn"><strong>${esc(usd(a.debt_usd))}</strong> debt</span>` : ''}
        <span><strong>${count(keys.length)}</strong> keys</span>
        <span><strong>${sub ? esc(sub.plan_code) : '—'}</strong> plan${sub ? ` · expires ${esc(date(sub.expires_at))}` : ''}</span>
      </div>
      <div class="account-meta">
        <span>Contact <strong>${esc(a.contact || '—')}</strong></span>
        <span>Created <strong>${esc(date(a.created_at))}</strong></span>
        <span>Billed · 30d <strong>${esc(usd(billed))}</strong></span>
        <span>Markup <strong>${a.markup_pct ? `${esc(a.markup_pct)}%` : 'global'}</strong></span>
        <span>Key creation <strong>${a.can_create_keys ? 'allowed' : 'guarded'}</strong></span>
      </div>
      ${a.note ? `<p class="account-note">Note: ${esc(a.note)}</p>` : ''}
      <div class="actions account-actions">
        <button class="primary small" data-credit>Add credit</button>
        <button class="small" data-sub>Grant plan</button>
        <button class="small" data-key>Create key</button>
        <button class="small" data-edit>Edit account</button>
      </div>
      <div class="account-lists">
        <section>
          <h3>Keys · ${count(keys.length)}</h3>
          ${keys.length
            ? `<ul class="mini-list">${keys.slice(0, 5).map(k => `<li>${pill(k.status)}<strong>${esc(k.name)}</strong><code>${esc(k.key_prefix)}…</code></li>`).join('')}${keys.length > 5 ? `<li class="hint">+${count(keys.length - 5)} more</li>` : ''}</ul>`
            : '<p class="hint">No keys.</p>'}
        </section>
        <section>
          <h3>Credit lots · ${count(credits.length)}</h3>
          ${credits.length
            ? `<ul class="mini-list">${credits.slice(0, 3).map(c => `<li>${pill(c.source, c.expires_at && c.expires_at < now ? 'warn' : 'muted')}<strong>${esc(usd(c.remaining_usd))}</strong><span class="hint">of ${esc(usd(c.amount_usd))}${c.expires_at ? ` · ${esc(date(c.expires_at))}` : ' · no expiry'}</span></li>`).join('')}${credits.length > 3 ? `<li class="hint">+${count(credits.length - 3)} more</li>` : ''}</ul>`
            : '<p class="hint">No credits.</p>'}
        </section>
      </div>`;
    body.querySelector('[data-copy-id]').onclick = () => copyText(a.id, 'Account ID').then(ok => ok && flash(body.querySelector('[data-copy-id]')));
    body.querySelector('[data-credit]').onclick = () => creditForm(aid, refresh);
    body.querySelector('[data-sub]').onclick = () => grantForm(aid, refresh);
    body.querySelector('[data-key]').onclick = () => createAccountKeyForm(aid, refresh);
    body.querySelector('[data-edit]').onclick = () => editAccountForm(a, refresh);
    body.querySelector('[data-suspend]')?.addEventListener('click', async () => {
      if (!(await confirmModal('Suspend account?', 'All of its keys stop proxying and workspace sign-in is blocked until the account is reactivated.', 'Suspend', true))) return;
      try {
        await api(endpoint(`accounts/${aid}`), 'PATCH', { status: 'suspended' });
        notice('Account suspended');
        refresh();
      } catch (e) {
        notice(e.message, true);
      }
    });
    body.querySelector('[data-activate]')?.addEventListener('click', async () => {
      try {
        await api(endpoint(`accounts/${aid}`), 'PATCH', { status: 'active' });
        notice('Account reactivated');
        refresh();
      } catch (e) {
        notice(e.message, true);
      }
    });
  };
  refresh();
}

function creditForm(aid, done) {
  modal('Add credit', `
    <form>
      <label for="credit-amount">Amount (USD)</label>
      <input id="credit-amount" name="amount_usd" inputmode="decimal" placeholder="10" required>
      <label for="credit-source">Source</label>
      <select id="credit-source" name="source">
        <option value="admin" selected>Admin grant</option>
        <option value="refund">Refund</option>
        <option value="purchase">Purchase</option>
      </select>
      <label for="credit-expiry">Expires (optional)</label>
      <input id="credit-expiry" name="expires_at" type="datetime-local">
      <label for="credit-note">Note (optional)</label>
      <input id="credit-note" name="note" placeholder="Goodwill credit">
      <div class="actions"><button class="primary" type="submit">Add credit</button></div>
    </form>`, d => bindForm(d.querySelector('form'), async b => {
    const payload = { amount_usd: b.amount_usd, source: b.source, note: b.note };
    if (b.expires_at) payload.expires_at = Math.floor(new Date(b.expires_at).getTime() / 1000);
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
        ${plans.map(p => `<option value="${esc(p.code)}">${esc(p.name)} — ${esc(usd(p.credit_usd))} credit · ${count(p.duration_days)} days</option>`).join('')}
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
      <label for="account-contact">Contact</label>
      <input id="account-contact" name="contact" type="email" value="${esc(a.contact)}" placeholder="ops@example.com">
      <label for="account-note">Note</label>
      <input id="account-note" name="note" value="${esc(a.note)}" placeholder="VIP customer">
      <label for="account-status">Status</label>
      <select id="account-status" name="status">
        <option value="active"${a.status === 'active' ? ' selected' : ''}>Active</option>
        <option value="suspended"${a.status === 'suspended' ? ' selected' : ''}>Suspended</option>
      </select>
      <label for="account-markup">Markup % (blank = global default)</label>
      <input id="account-markup" name="markup_pct" inputmode="decimal" value="${a.markup_pct ? esc(a.markup_pct) : ''}" placeholder="30">
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
  }));
}

/* Views ---------------------------------------------------------------------- */

async function landing() {
  main.innerHTML = `
    <section class="hero">
      <div>
        <div class="eyebrow">ONE ENDPOINT · ENDLESS POSSIBILITIES</div>
        <h1>Intelligence,<br>on your terms<span class="accent">.</span></h1>
        <p>Language, speech, and audio through one familiar API. Create a key, connect your app, and stay in control of every request.</p>
        <div class="actions section">
          <button class="primary" id="create">Get an API key ↗</button>
          <a class="button ghost" href="/workspace/">Open workspace</a>
        </div>
        <p class="section"><small>OpenAI-compatible · Usage-based billing · Live statistics</small></p>
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
    <section class="section">
      <div class="row">
        <div><div class="eyebrow">ROOM TO BUILD</div><h2>Choose your credit plan</h2></div>
        <span class="muted">Simple, prepaid access.</span>
      </div>
      <div class="plans section" id="plans">${loading('Loading plans…')}</div>
    </section>
    <section class="steps section">
      <div class="card step"><div class="eyebrow">01 / CONNECT ONCE</div><p>Use your existing OpenAI SDK. Change the base URL and API key.</p></div>
      <div class="card step"><div class="eyebrow">02 / SEE EVERY REQUEST</div><p>Track tokens, request history, and spending in your workspace.</p></div>
      <div class="card step"><div class="eyebrow">03 / STAY IN CONTROL</div><p>Rotate keys, add credit, and keep your applications running.</p></div>
    </section>`;
  document.querySelector('#create').onclick = keyForm;
  try {
    const v = await api('/v1/plans');
    const best = bestCode(v.plans);
    document.querySelector('#plans').innerHTML = v.plans.length
      ? v.plans.map(p => planCard(p, best)).join('')
      : emptyState('No plans yet', 'An operator has not published any plans.');
  } catch (e) {
    notice(e.message, true);
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
          <div class="eyebrow">${admin ? 'ADMINISTRATOR' : 'SIGN IN'}</div>
          <h2>Welcome back<span class="accent">.</span></h2>
          <label for="login-token">${admin ? 'Administrator token' : 'API key or dashboard token'}</label>
          <input id="login-token" name="token" type="password" autocomplete="off" required>
          <p class="field-help">Credentials stay in this browser tab for the session.</p>
          <div class="actions full"><button class="primary" type="submit">Open ${admin ? 'console' : 'workspace'} →</button></div>
        </form>
        ${admin ? '' : `
        <div class="auth-alt">
          <div>
            <div class="label">New here?</div>
            <div class="hint">Create an account and get evaluation credit — no card required.</div>
          </div>
          <button id="create" class="button ghost">Create a new account →</button>
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

async function shell() {
  const tabs = admin ? ['overview', 'keys', 'accounts', 'plans', 'codes', 'payments', 'requests', 'settings'] : ['overview', 'keys', 'usage', 'requests', 'billing'];
  const requested = location.hash.slice(1);
  if (tabs.includes(requested)) tab = requested;
  else history.replaceState(null, '', `#${tab}`);
  main.innerHTML = `
    <div class="page-head">
      <div class="eyebrow">${admin ? 'OPERATOR CONSOLE' : 'YOUR WORKSPACE'}</div>
      <h2>${admin ? 'Make every request count.' : 'Build more. Stay in control.'}</h2>
    </div>
    <div class="tabs" role="tablist">
      ${tabs.map(t => `<button role="tab" data-tab="${t}" aria-selected="${t === tab}">${t[0].toUpperCase() + t.slice(1)}</button>`).join('')}
    </div>
    <section id="content" aria-live="polite">${loading()}</section>`;
  const select = () => main.querySelectorAll('[data-tab]').forEach(t => t.setAttribute('aria-selected', String(t.dataset.tab === tab)));
  main.querySelectorAll('[data-tab]').forEach(b => b.onclick = () => {
    if (tab === b.dataset.tab) return;
    tab = b.dataset.tab;
    location.hash = tab;
    select();
    load();
  });
  window.onhashchange = () => {
    const next = location.hash.slice(1);
    if (tabs.includes(next) && next !== tab) {
      tab = next;
      select();
      load();
    }
  };
  await load();
}

let generation = 0;
async function load() {
  const gen = ++generation;
  const content = document.querySelector('#content');
  content.innerHTML = loading();
  try {
    const html = await views[tab]();
    if (gen !== generation) return;
    content.innerHTML = html;
    wire(content);
  } catch (e) {
    if (gen === generation) {
      content.innerHTML = `<div class="card error"><strong>Something went wrong</strong><p>${esc(e.message)}</p><div class="actions"><button data-refresh class="ghost">Try again</button></div></div>`;
      wire(content);
    }
  }
}

let chartSeq = 0;
function chart(series) {
  const days = new Map();
  for (const r of series) days.set(r.day, (days.get(r.day) || 0) + Number(r.billed_cost_usd));
  const entries = [...days.entries()];
  if (!entries.length) return emptyState('No usage yet', 'Make your first API request to see usage here.');
  const max = Math.max(...entries.map(([, v]) => v), 0.000001);
  const W = 600, H = 150, base = H - 10, top = 16;
  const points = entries.map(([, v], i) => [
    entries.length === 1 ? W / 2 : (i / (entries.length - 1)) * W,
    base - (v / max) * (base - top)
  ]);
  const line = points.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(' ');
  const area = `${points[0][0].toFixed(1)},${base} ${line} ${points[points.length - 1][0].toFixed(1)},${base}`;
  const id = `usage-${++chartSeq}`;
  const last = entries[entries.length - 1][1];
  return `<div class="chart-wrap">
    <svg class="chart" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="Daily billed usage">
      <defs><linearGradient id="${id}" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stop-color="#c7f590" stop-opacity=".32"/>
        <stop offset="100%" stop-color="#c7f590" stop-opacity="0"/>
      </linearGradient></defs>
      <line x1="0" y1="${base}" x2="${W}" y2="${base}"/>
      <polygon points="${area}" fill="url(#${id})"/>
      <polyline points="${line}"/>
    </svg>
    <div class="chart-foot"><span>${entries.length} active day${entries.length === 1 ? '' : 's'}</span><span>Last billed ${esc(usd(last))}</span></div>
  </div>`;
}

const requestIndex = new Map();
const requestTable = rows => {
  rows.forEach(r => requestIndex.set(r.id, r));
  return table([
    ['Time', r => esc(date(r.created_at))],
    ['Route', r => `<code>${esc(r.path)}</code>`],
    ['Model', r => esc(r.model || '—')],
    ['Status', r => requestPill(r)],
    ['Tokens', r => `<span class="num">${count(r.tokens)}</span>`],
    ['Billed', r => `<span class="num">${esc(usd(r.billed_usd))}</span>`],
    ['Latency', r => `<span class="num">${count(r.duration_ms)} ms</span>`],
    ['', r => `<button class="small" data-request-details="${esc(r.id)}">View</button>`]
  ], rows);
};

function requestDetails(r) {
  const cell = (label, value) => `<div><span class="label">${esc(label)}</span><div class="kv-value">${value}</div></div>`;
  const ref = value => value
    ? `<code title="${esc(value)} — click to copy" data-copy-value="${esc(value)}">${esc(value.slice(0, 13))}…</code>`
    : '<span class="muted">—</span>';
  modal('Request details', `
    <div class="request-head">
      <div>${requestPill(r)}<div class="hint">${esc(date(r.created_at))}</div></div>
      <code>${esc(r.method)} ${esc(r.path)}</code>
    </div>
    <div class="kv section">
      ${cell('Request ID', ref(r.id))}
      ${cell('Model', esc(r.model || '—'))}
      ${cell('Kind', esc(r.kind || '—'))}
      ${cell('Account', ref(r.account_id))}
      ${cell('Key', ref(r.key_id))}
      ${cell('IP', esc(r.ip || '—'))}
      ${cell('Tokens', `<span class="num">${count(r.tokens)}</span>`)}
      ${cell('Upstream cost', `<span class="num">${esc(usd(r.upstream_usd))}</span>`)}
      ${cell('Billed', `<span class="num">${esc(usd(r.billed_usd))}</span>`)}
      ${cell('Duration', `<span class="num">${count(r.duration_ms)} ms</span>`)}
      ${cell('Bytes in', `<span class="num">${count(r.bytes_in)}</span>`)}
      ${cell('Bytes out', `<span class="num">${count(r.bytes_out)}</span>`)}
      ${cell('Finished', r.finished ? 'Yes' : '<span class="pill tone-info">In progress</span>')}
      ${cell('Error', r.error ? `<span class="error">${esc(r.error)}</span>` : '<span class="muted">—</span>')}
    </div>`, d => {
    d.querySelectorAll('[data-copy-value]').forEach(el => {
      el.onclick = () => copyText(el.dataset.copyValue, 'Value').then(ok => ok && flash(el));
    });
  }, 'wide');
}

const views = {
  async overview() {
    if (admin) {
      const [v, u] = await Promise.all([api(endpoint('overview')), api(endpoint('usage'))]);
      return `<div class="stats">
          ${metric('Total requests', count(v.requests))}
          ${metric('API revenue', usd(v.billed_cost_usd))}
          ${metric('Gross margin', usd(v.margin_usd))}
          ${metric('Active keys', count(v.active_keys))}
        </div>
        <div class="card">
          <div class="card-head"><h3>Usage at a glance</h3><button data-refresh class="ghost small">Refresh</button></div>
          ${chart(u.series)}
        </div>
        <div class="stats">
          ${metric('Accounts', count(v.accounts))}
          ${metric('Tokens', count(v.tokens))}
          ${metric('Upstream cost', usd(v.upstream_cost_usd))}
          ${metric('Outstanding credit', usd(v.outstanding_usd))}
        </div>`;
    }
    const [v, u, r] = await Promise.all([api('/v1/account'), api('/v1/account/usage'), api('/v1/account/requests?limit=5')]);
    const sub = v.account.subscription;
    return `<div class="banner">
        <div>
          <div class="label">Available balance</div>
          <div class="metric">${esc(usd(v.account.balance_usd))}</div>
          <div class="hint">${v.account.debt_usd !== '0' ? `Outstanding debt ${esc(usd(v.account.debt_usd))} settles from the next top-up.` : 'Ready to spend on any key on this account.'}</div>
        </div>
        <div class="actions"><button class="primary" data-create>Create key</button></div>
      </div>
      <div class="stats">
        ${metric('Account status', v.account.status)}
        ${metric('Current plan', sub ? sub.plan_code : 'Evaluation', sub ? `Expires ${date(sub.expires_at)}` : 'No active plan')}
        ${metric('Credit lots', count(v.credits.length), 'Soonest-expiring credit is spent first')}
      </div>
      <div class="card">
        <div class="card-head"><h3>Your usage</h3><button data-refresh class="ghost small">Refresh</button></div>
        ${chart(u.series)}
      </div>
      <div class="card">
        <div class="card-head"><h3>Recent requests</h3><button data-refresh class="ghost small">Refresh</button></div>
        ${requestTable(r.requests)}
      </div>`;
  },
  async keys() {
    const v = await api(endpoint('keys'));
    const limits = l => {
      const parts = [];
      if (l.rpm) parts.push(`${count(l.rpm)} rpm`);
      if (l.requests) parts.push(`${count(l.requests.max)} req / ${count(l.requests.window_hours)}h`);
      if (l.tokens) parts.push(`${count(l.tokens.max)} tokens / ${count(l.tokens.window_hours)}h`);
      if (l.spend) parts.push(`${esc(usd(l.spend.max_usd))} / ${count(l.spend.window_hours)}h`);
      if (l.allowed_models?.length) parts.push(`${count(l.allowed_models.length)} model${l.allowed_models.length === 1 ? '' : 's'}`);
      if (l.max_children) parts.push(`${count(l.max_children)} children`);
      return parts.length ? `<span class="hint">${parts.join('<br>')}</span>` : '<span class="muted">Account default</span>';
    };
    const used = r => (r.usage_30d?.series || []).reduce((a, s) => a + Number(s.billed_cost_usd), 0);
    const columns = [
      ['Key', r => `<strong>${esc(r.name)}</strong><br><code class="muted">${esc(r.key_prefix)}…</code>`],
      ['Status', r => `${pill(r.status)}${r.expires_at ? `<br><span class="hint">expires ${esc(date(r.expires_at))}</span>` : ''}`],
    ];
    if (admin) columns.push(['Account', r => `<code title="${esc(r.account_id)}">${esc(r.account_id.slice(0, 8))}…</code>`]);
    columns.push(
      ['Limits', r => limits(r.limits)],
      ['Used · 30d', r => `<span class="num">${esc(usd(used(r)))}</span>`],
      ['Last used', r => esc(date(r.last_used_at))],
      ['Actions', r => `<div class="actions">
        <button data-edit-key="${r.id}" data-key-name="${esc(r.name)}">Edit</button>
        <button data-rotate="${r.id}"${r.status !== 'active' ? ' disabled' : ''}>Rotate</button>
        <button class="danger" data-block="${r.id}"${r.status !== 'active' ? ' disabled' : ''}>Block</button>
        ${admin ? `<button class="danger" data-revoke="${r.id}"${r.status === 'revoked' ? ' disabled' : ''}>Revoke</button>` : ''}
      </div>`]
    );
    return `<div class="card-head"><h3>API keys</h3>${admin ? '' : '<button class="primary" data-create>+ Create key</button>'}</div>
      ${table(columns, v.keys)}`;
  },
  async accounts() {
    const v = await api(endpoint('accounts'));
    return `<div class="card-head"><h3>Customer accounts</h3><span class="muted">${count(v.accounts.length)} shown</span></div>
      ${table([
        ['Account', r => `<code title="${esc(r.id)}">${esc(r.id.slice(0, 8))}…</code>`],
        ['Contact', r => esc(r.contact || '—')],
        ['Balance', r => `<strong class="num">${esc(usd(r.balance_usd))}</strong>`],
        ['Status', r => pill(r.status)],
        ['Created', r => esc(date(r.created_at))],
        ['Actions', r => `<button data-account="${r.id}">Manage</button>`]
      ], v.accounts)}`;
  },
  async plans() {
    const v = await api(endpoint('plans'));
    const best = bestCode(v.plans.filter(p => p.active));
    return `<div class="card-head"><h3>Plans</h3><button class="primary" data-plan>+ Create plan</button></div>
      <p class="muted">Deactivate a plan to hide it from customers. Plans referenced by subscriptions, payments, or redeem codes cannot be deleted, only retired.</p>
      ${table([
        ['Code', r => `<code>${esc(r.code)}</code>`],
        ['Name', r => esc(r.name)],
        ['Price', r => `<span class="num">${esc(usd(r.price_usd))}</span>`],
        ['Credit', r => `<span class="num">${esc(usd(r.credit_usd))}</span>`],
        ['Days', r => count(r.duration_days)],
        ['Status', r => r.active ? pill(r.code === best ? 'best value' : 'active', 'ok') : pill('inactive', 'muted')],
        ['Actions', r => `<div class="actions">
          <button data-edit-plan="${esc(r.code)}" data-plan-name="${esc(r.name)}" data-plan-price="${esc(r.price_usd)}" data-plan-credit="${esc(r.credit_usd)}" data-plan-days="${r.duration_days}" data-plan-active="${r.active}" data-plan-description="${esc(r.description)}">Edit</button>
          <button class="danger" data-delete-plan="${esc(r.code)}">Delete</button>
        </div>`]
      ], v.plans)}`;
  },
  async codes() {
    const v = await api(endpoint('codes'));
    const now = Date.now() / 1000;
    const state = r => r.redeemed_at ? pill('redeemed', 'info') : r.expires_at && r.expires_at < now ? pill('expired', 'warn') : pill('available', 'ok');
    return `<div class="card-head"><h3>Prepaid redeem codes</h3><button class="primary" data-code>+ Issue code</button></div>
      <p class="muted">Codes are shown once and stored hashed. Unredeemed codes can be edited or cancelled; redeemed codes are kept as audit records.</p>
      ${table([
        ['Code prefix', r => `<code>${esc(r.code_prefix)}…</code>`],
        ['Plan', r => r.plan_code ? `<code>${esc(r.plan_code)}</code>` : 'Credit'],
        ['Credit', r => r.credit_usd ? `<span class="num">${esc(usd(r.credit_usd))}</span>` : '—'],
        ['Status', state],
        ['Redeemed', r => esc(date(r.redeemed_at))],
        ['Expires', r => esc(date(r.expires_at))],
        ['Actions', r => `<div class="actions">
          <button data-edit-code="${r.id}" data-code-expires="${r.expires_at ?? ''}" data-code-note="${esc(r.note)}"${r.redeemed_at ? ' disabled' : ''}>Edit</button>
          <button class="danger" data-delete-code="${r.id}"${r.redeemed_at ? ' disabled' : ''}>Delete</button>
        </div>`]
      ], v.codes)}`;
  },
  async payments() {
    const v = await api(endpoint('payments'));
    return `<div class="card-head"><h3>Payment orders</h3><span class="muted">Stripe Checkout history</span></div>
      ${table([
        ['Created', r => esc(date(r.created_at))],
        ['Account', r => `<code title="${esc(r.account_id)}">${esc(r.account_id.slice(0, 8))}…</code>`],
        ['Plan', r => `<code>${esc(r.plan_code)}</code>`],
        ['Amount', r => `<span class="num">${esc(usd(r.price_usd))}</span>`],
        ['Credit', r => `<span class="num">${esc(usd(r.credit_usd))}</span>`],
        ['Status', r => pill(r.status)]
      ], v.payments)}`;
  },
  async requests() {
    let models = [];
    if (admin) {
      try {
        models = (await api(endpoint('requests/groups?field=model&limit=200'))).groups.map(g => g.label).filter(Boolean);
      } catch {
        models = [];
      }
    }
    return `<div class="card-head"><h3>Request history</h3><button data-refresh class="ghost small">Refresh</button></div>
      <form id="request-filter" class="filters">
        <input name="q" aria-label="Search model, route, or request id" placeholder="Search model, route, or request id…">
        <select name="kind" aria-label="Kind">
          <option value="">All kinds</option>
          <option value="llm">LLM</option>
          <option value="tts">TTS</option>
          <option value="stt">STT</option>
        </select>
        <select name="status" aria-label="Status">
          <option value="">All statuses</option>
          <option value="2xx">2xx success</option>
          <option value="4xx">4xx client error</option>
          <option value="5xx">5xx upstream error</option>
          <option value="pending">In progress</option>
        </select>
        <input name="model" list="request-models" aria-label="Exact model" placeholder="Exact model">
        <datalist id="request-models">${models.map(m => `<option value="${esc(m)}"></option>`).join('')}</datalist>
        ${admin ? '<input name="account" aria-label="Account id" placeholder="Account id"><input name="key" aria-label="Key id" placeholder="Key id">' : ''}
        <button type="submit">Apply</button>
        <button type="button" class="ghost" data-clear>Clear</button>
      </form>
      <div id="request-results">${loading('Loading requests…')}</div>
      <div class="pager section">
        <span class="hint" id="request-range"></span>
        <div class="actions">
          <label class="inline" for="request-size">Rows</label>
          <select id="request-size" aria-label="Rows per page"><option>20</option><option>50</option><option>100</option></select>
          <button type="button" data-prev class="ghost">← Previous</button>
          <button type="button" data-next class="ghost">Next →</button>
        </div>
      </div>`;
  },
  async usage() {
    const v = await api('/v1/account/usage');
    return `<div class="card"><div class="card-head"><h3>Usage · last 30 days</h3></div>${chart(v.series)}</div>
      ${table([
        ['Day', r => esc(new Date(r.day * 1000).toLocaleDateString())],
        ['Model', r => `<code>${esc(r.model)}</code>`],
        ['Requests', r => `<span class="num">${count(r.requests)}</span>`],
        ['Tokens', r => `<span class="num">${count(r.tokens)}</span>`],
        ['Billed', r => `<span class="num">${esc(usd(r.billed_cost_usd))}</span>`]
      ], v.series)}`;
  },
  async billing() {
    const [v, p] = await Promise.all([api('/v1/account'), api('/v1/plans')]);
    const now = Date.now() / 1000;
    const best = bestCode(p.plans);
    return `<div class="split">
        <div>
          <div class="card-head"><h3>Add API credit</h3><span class="muted">Prepaid, non-renewing packages</span></div>
          <div class="plans">${p.plans.length
            ? p.plans.map(plan => planCard(plan, best, `<button class="primary" data-buy="${esc(plan.code)}">Pay with Stripe ↗</button>`)).join('')
            : emptyState('No plans available', 'Ask the operator for a redeem code instead.')}</div>
        </div>
        <form id="redeem" class="card redeem-card">
          <h3>Have a credit code?</h3>
          <label for="redeem-code">Redeem code</label>
          <input id="redeem-code" name="code" required placeholder="RES-…">
          <div class="actions"><button type="submit" class="primary">Redeem credit</button></div>
        </form>
      </div>
      <h3 class="section">Your credit ledger</h3>
      ${table([
        ['Source', r => esc(r.source)],
        ['Amount', r => `<span class="num">${esc(usd(r.amount_usd))}</span>`],
        ['Remaining', r => `<span class="num">${esc(usd(r.remaining_usd))}</span>`],
        ['Expires', r => r.expires_at && r.expires_at < now ? pill('expired', 'warn') : esc(date(r.expires_at))]
      ], v.credits)}`;
  },
  async settings() {
    const v = await api(endpoint('settings'));
    return `<div class="card-head"><h3>Runtime settings</h3><span class="muted">Stored in SQLite</span></div>
      <p class="muted">Secret fields are redacted; leave them blank to keep their current values. USD amounts and percentages are decimal strings. CORS changes require a restart.</p>
      <form id="settings" class="card">
        <label for="settings-json">Configuration JSON</label>
        <textarea id="settings-json" name="json" rows="30" spellcheck="false">${esc(JSON.stringify(v.settings, null, 2))}</textarea>
        <div class="actions"><button class="primary" type="submit">Save settings</button></div>
      </form>`;
  }
};

function jsonEditor(title, initial, save, hint) {
  modal(title, `<form>${hint ? `<p class="muted">${esc(hint)}</p>` : ''}<label for="editor">Fields (JSON)</label><textarea id="editor" name="json" rows="14" spellcheck="false">${esc(JSON.stringify(initial, null, 2))}</textarea><div class="actions"><button class="primary" type="submit">Save</button></div></form>`, d => bindForm(d.querySelector('form'), async b => {
    const result = await save(JSON.parse(b.json));
    d.close();
    notice('Saved');
    if (result?.key) secrets(result);
    await load();
  }));
}

/* Interaction wiring --------------------------------------------------------- */

function wire(root) {
  root.querySelectorAll('[data-refresh]').forEach(b => b.onclick = load);
  root.querySelectorAll('[data-create]').forEach(b => b.onclick = keyForm);

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

  action('[data-block]', async b => {
    await api(endpoint(`keys/${b.dataset.block}`), 'PATCH', { status: 'blocked' });
    notice('Key blocked');
    await load();
  });
  action('[data-rotate]', async b => {
    if (!(await confirmModal('Rotate key?', 'Replace this key? Applications using it will need the new key.', 'Rotate key'))) return;
    secrets(await api(endpoint(`keys/${b.dataset.rotate}/rotate`), 'POST', {}));
    await load();
  });
  action('[data-revoke]', async b => {
    if (!(await confirmModal('Revoke key?', 'Permanently revoke this key and its dashboard token? This cannot be undone.', 'Revoke key', true))) return;
    await api(endpoint(`keys/${b.dataset.revoke}/revoke`), 'POST', {});
    notice('Key revoked');
    await load();
  });
  action('[data-edit-key]', async b => {
    let initial = { name: b.dataset.keyName || 'My key' };
    if (admin) {
      const v = await api(endpoint(`keys/${b.dataset.editKey}`));
      initial = { name: v.key.name, status: v.key.status, limits: v.key.limits, markup_pct: v.key.markup_pct, expires_at: v.key.expires_at };
    }
    jsonEditor('Edit key', initial, v => api(endpoint(`keys/${b.dataset.editKey}`), 'PATCH', v));
  });
  action('[data-account]', async b => manageAccount(b.dataset.account));
  action('[data-plan]', async () => jsonEditor('Create plan', { code: 'starter', name: 'Starter', price_usd: '10', credit_usd: '10', duration_days: 30, active: true, description: '' }, b => api(endpoint('plans'), 'POST', b)));
  action('[data-edit-plan]', async b => jsonEditor(`Edit plan · ${b.dataset.editPlan}`, {
    code: b.dataset.editPlan,
    name: b.dataset.planName,
    price_usd: b.dataset.planPrice,
    credit_usd: b.dataset.planCredit,
    duration_days: Number(b.dataset.planDays),
    active: b.dataset.planActive === 'true',
    description: b.dataset.planDescription,
  }, v => api(endpoint('plans'), 'POST', v)));
  action('[data-delete-plan]', async b => {
    const code = b.dataset.deletePlan;
    if (!(await confirmModal('Delete plan?', `Delete "${code}"? This cannot be undone. Plans with subscriptions, payments, or codes can only be deactivated.`, 'Delete plan', true))) return;
    await api(endpoint(`plans/${encodeURIComponent(code)}`), 'DELETE');
    notice('Plan deleted');
    await load();
  });
  action('[data-edit-code]', async b => jsonEditor('Edit redeem code', {
    expires_at: b.dataset.codeExpires ? Number(b.dataset.codeExpires) : null,
    note: b.dataset.codeNote,
  }, v => api(endpoint(`codes/${b.dataset.editCode}`), 'PATCH', v)));
  action('[data-delete-code]', async b => {
    if (!(await confirmModal('Delete code?', 'Cancel this redeem code? Anyone holding it will no longer be able to redeem it.', 'Delete code', true))) return;
    await api(endpoint(`codes/${b.dataset.deleteCode}`), 'DELETE');
    notice('Code deleted');
    await load();
  });
  action('[data-request-details]', async b => {
    const r = requestIndex.get(b.dataset.requestDetails);
    if (r) requestDetails(r);
  });
  action('[data-code]', async () => jsonEditor('Issue redeem code', { credit_usd: '10', expires_at: null, note: '' }, async b => {
    const v = await api(endpoint('codes'), 'POST', b);
    if (v.codes) {
      const list = v.codes.join('\n');
      modal('Save your redeem codes', `
        <p class="muted">${count(v.codes.length)} single-use codes — shown only once. Copy them now.</p>
        <pre class="code-list secret">${esc(list)}</pre>
        <div class="actions section"><button class="primary" data-copy-codes>Copy all</button></div>`, d => {
        const button = d.querySelector('[data-copy-codes]');
        button.onclick = () => copyText(list, 'Redeem codes').then(ok => ok && flashCopied(button));
      }, 'wide');
    } else {
      modal('Save your redeem code', `<p class="muted">Shown once. Share with the intended customer.</p><div class="cred-row"><span class="cred-label">Code</span><code class="cred-value secret" title="${esc(v.code)}">${esc(v.code)}</code><button class="small" data-copy-code>Copy</button></div>`, d => {
        const button = d.querySelector('[data-copy-code]');
        const row = button.closest('.cred-row');
        const copy = () => copyText(v.code, 'Redeem code').then(ok => {
          if (ok) {
            flashCopied(button);
            flash(row);
          }
        });
        button.onclick = copy;
        row.querySelector('.cred-value').onclick = copy;
      }, 'wide');
    }
    return v;
  }, 'One object issues one code. Paste an array of objects to issue up to 100 single-use codes in one batch.'));
  action('[data-buy]', async b => {
    const v = await api('/v1/account/checkout', 'POST', { plan_code: b.dataset.buy });
    const url = new URL(v.url);
    if (url.protocol !== 'https:' || url.hostname !== 'checkout.stripe.com') throw new Error('Unexpected checkout URL');
    location.href = url.href;
  });

  if (root.querySelector('#redeem')) bindForm(root.querySelector('#redeem'), async b => {
    await api('/v1/account/redeem', 'POST', b);
    notice('Credit applied');
    await load();
  });
  if (root.querySelector('#settings')) bindForm(root.querySelector('#settings'), async b => {
    const updates = JSON.parse(b.json);
    await api(endpoint('settings'), 'PATCH', updates);
    if (updates.admin_token) {
      token = updates.admin_token;
      sessionStorage.setItem(store, token);
    }
    notice('Settings saved');
  });
  if (root.querySelector('#request-filter')) {
    const form = root.querySelector('#request-filter');
    let offset = 0;
    let limit = 20;
    let filters = {};
    let sequence = 0;
    const refresh = async () => {
      const seq = ++sequence;
      const params = new URLSearchParams({ limit: String(limit), offset: String(offset) });
      for (const [key, value] of Object.entries(filters)) if (value) params.set(key, value);
      try {
        const v = await api(endpoint(admin ? 'requests' : 'account/requests') + `?${params}`);
        if (seq !== sequence) return;
        root.querySelector('#request-results').innerHTML = requestTable(v.requests);
        const total = Number(v.total || 0);
        const shown = v.requests.length;
        root.querySelector('#request-range').textContent = total === 0
          ? 'No matching requests.'
          : `Showing ${count(offset + 1)}–${count(offset + shown)} of ${count(total)}`;
        root.querySelector('[data-prev]').disabled = offset === 0;
        root.querySelector('[data-next]').disabled = offset + shown >= total;
      } catch (e) {
        notice(e.message, true);
      }
    };
    bindForm(form, async b => {
      filters = Object.fromEntries(Object.entries(b).filter(([, value]) => value !== ''));
      offset = 0;
      await refresh();
    });
    root.querySelector('[data-clear]').onclick = () => {
      form.reset();
      filters = {};
      offset = 0;
      refresh();
    };
    root.querySelector('#request-size').onchange = e => {
      limit = Number(e.target.value);
      offset = 0;
      refresh();
    };
    root.querySelector('[data-prev]').onclick = () => {
      offset = Math.max(0, offset - limit);
      refresh();
    };
    root.querySelector('[data-next]').onclick = () => {
      offset += limit;
      refresh();
    };
    refresh();
  }
}

renderHeader();
if (!admin && !workspace) landing();
else if (token) shell();
else login();
