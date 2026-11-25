-- Initial schema for reseller (single migration).
-- Money columns are whole micro-USD (1 USD = 1000000) stored as INTEGER.

-- Single row holding the JSON-encoded settings (id is always 1).
CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL CHECK(json_valid(value))) STRICT;
-- Customer accounts. status is active or suspended; suspended_reason says why
-- ('' for an administrator, refund for an automatic store refund block).
-- markup_pct overrides the global markup when set. debt_micro never goes below 0.
-- store_token is the UUID native apps pass to the stores (App Store appAccountToken,
-- Google Play obfuscatedAccountId). It is created on first use.
-- consumption_consent records the customer's consent to share consumption data
-- with Apple when Apple asks about a refund request.
CREATE TABLE accounts (
 id TEXT PRIMARY KEY, contact TEXT NOT NULL DEFAULT '', note TEXT NOT NULL DEFAULT '',
 status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','suspended')),
 suspended_reason TEXT NOT NULL DEFAULT '',
 can_create_keys INTEGER NOT NULL DEFAULT 0 CHECK(can_create_keys IN (0,1)),
 markup_pct TEXT, debt_micro INTEGER NOT NULL DEFAULT 0 CHECK(debt_micro>=0),
 store_token TEXT UNIQUE,
 consumption_consent INTEGER NOT NULL DEFAULT 0 CHECK(consumption_consent IN (0,1)), consent_at INTEGER,
 created_at INTEGER NOT NULL, created_ip TEXT NOT NULL
) STRICT;
CREATE INDEX accounts_status_created ON accounts(status,created_at DESC);
-- API keys. Only HMAC hashes of the secret and dashboard token are stored.
-- parent_key_id links child keys. limits holds the per-key limits as JSON.
CREATE TABLE api_keys (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), parent_key_id TEXT REFERENCES api_keys(id),
 key_hash TEXT NOT NULL UNIQUE, dash_hash TEXT NOT NULL UNIQUE, key_prefix TEXT NOT NULL,
 name TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','blocked','revoked')),
 limits TEXT NOT NULL CHECK(json_valid(limits)), markup_pct TEXT, expires_at INTEGER,
 created_at INTEGER NOT NULL, created_ip TEXT NOT NULL, bypass_ip INTEGER NOT NULL DEFAULT 0,
 last_used_at INTEGER,
 -- Lifetime billed spend in micro-USD, added by the meter on every billed amount. It survives
 -- usage bucket and request log retention. An account's lifetime spend is the sum over its keys.
 billed_micro INTEGER NOT NULL DEFAULT 0 CHECK(billed_micro>=0)
) STRICT;
CREATE INDEX keys_account ON api_keys(account_id);
CREATE INDEX keys_ip_time ON api_keys(created_ip,created_at);
CREATE INDEX keys_status_created ON api_keys(status,created_at DESC);
-- Plans. kind is subscription or topup.
-- A subscription runs for months (1 to 12) and includes credit_micro of usage
-- per month: each month's quota expires at the end of that month.
-- A topup adds credit_micro once, and top-up credit never expires.
-- apple_product_id and google_product_id map store products to plans.
CREATE TABLE plans (
 code TEXT PRIMARY KEY, name TEXT NOT NULL,
 kind TEXT NOT NULL DEFAULT 'subscription' CHECK(kind IN ('subscription','topup')),
 price_micro INTEGER NOT NULL CHECK(price_micro>0), credit_micro INTEGER NOT NULL CHECK(credit_micro>0),
 months INTEGER,
 apple_product_id TEXT UNIQUE, google_product_id TEXT UNIQUE,
 active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1)), description TEXT NOT NULL DEFAULT '',
 CHECK((kind='subscription' AND months BETWEEN 1 AND 12) OR (kind='topup' AND months IS NULL))
) STRICT;
-- Built-in plans: price and credit in micro-USD (20000000 = 20 USD). Subscription
-- credit is the monthly quota. Store product ids are set by the operator.
INSERT INTO plans(code,name,kind,price_micro,credit_micro,months,description) VALUES
 ('monthly','Monthly','subscription',20000000,20000000,1,'$20 of usage every month. Cancel any time.'),
 ('halfyear','6 months','subscription',96000000,17000000,6,'$17 of usage every month for six months.'),
 ('year','12 months','subscription',144000000,12500000,12,'$12.50 of usage every month for a year.'),
 ('topup-10','Top-up $10','topup',10000000,10000000,NULL,'Extra usage that never expires.'),
 ('topup-25','Top-up $25','topup',25000000,25000000,NULL,'Extra usage that never expires.');
-- Subscriptions. The current paid period runs from period_start to expires_at.
-- Each month of it grants one quota lot of quota_micro (granted_months counts
-- them), which expires at the end of that month. Store renewals start a new
-- period. status is active or refunded. provider says where it came from.
-- original_transaction_id (App Store) and purchase_token (Google Play) identify
-- store subscriptions. idempotency_key deduplicates administrator grants.
CREATE TABLE subscriptions (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), plan_code TEXT NOT NULL REFERENCES plans(code),
 quota_micro INTEGER NOT NULL CHECK(quota_micro>0), months INTEGER NOT NULL CHECK(months BETWEEN 1 AND 12),
 started_at INTEGER NOT NULL, period_start INTEGER NOT NULL, expires_at INTEGER NOT NULL CHECK(expires_at>period_start),
 granted_months INTEGER NOT NULL DEFAULT 0 CHECK(granted_months>=0),
 status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','refunded')),
 auto_renew INTEGER NOT NULL DEFAULT 0 CHECK(auto_renew IN (0,1)),
 provider TEXT NOT NULL CHECK(provider IN ('admin','code','stripe','payroof','apple','google')),
 original_transaction_id TEXT, purchase_token TEXT, idempotency_key TEXT
) STRICT;
CREATE INDEX subscriptions_account ON subscriptions(account_id,expires_at);
CREATE INDEX subscriptions_due ON subscriptions(status,expires_at);
CREATE UNIQUE INDEX subscriptions_apple ON subscriptions(original_transaction_id) WHERE original_transaction_id IS NOT NULL;
CREATE UNIQUE INDEX subscriptions_google ON subscriptions(purchase_token) WHERE purchase_token IS NOT NULL;
CREATE UNIQUE INDEX subscriptions_idempotency ON subscriptions(account_id,idempotency_key) WHERE idempotency_key IS NOT NULL;
-- Credit lots per account. remaining_micro counts down as credit is spent.
-- A NULL expires_at means the credit never expires. subscription_id links a
-- monthly quota lot to its subscription; payment_id links a purchased top-up
-- to its payment. idempotency_key deduplicates administrator top-ups.
CREATE TABLE credits (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), source TEXT NOT NULL,
 amount_micro INTEGER NOT NULL CHECK(amount_micro>0), remaining_micro INTEGER NOT NULL CHECK(remaining_micro>=0 AND remaining_micro<=amount_micro),
 expires_at INTEGER, created_at INTEGER NOT NULL, note TEXT NOT NULL DEFAULT '',
 subscription_id TEXT REFERENCES subscriptions(id), payment_id TEXT REFERENCES payments(id), idempotency_key TEXT
) STRICT;
CREATE INDEX credits_account ON credits(account_id,expires_at);
CREATE INDEX credits_subscription ON credits(subscription_id) WHERE subscription_id IS NOT NULL;
CREATE INDEX credits_payment ON credits(payment_id) WHERE payment_id IS NOT NULL;
CREATE UNIQUE INDEX credits_idempotency ON credits(account_id,idempotency_key) WHERE idempotency_key IS NOT NULL;
-- Redeem codes. Each code grants either a plan or a credit amount, not both.
-- code_hash is an HMAC of the code, so the plain code is never stored.
CREATE TABLE redeem_codes (
 id TEXT PRIMARY KEY, code_hash TEXT NOT NULL UNIQUE, code_prefix TEXT NOT NULL,
 plan_code TEXT REFERENCES plans(code), credit_micro INTEGER,
 expires_at INTEGER, redeemed_by TEXT REFERENCES accounts(id), redeemed_at INTEGER,
 created_at INTEGER NOT NULL, note TEXT NOT NULL DEFAULT '',
 CHECK((plan_code IS NOT NULL AND credit_micro IS NULL) OR (plan_code IS NULL AND credit_micro>0))
) STRICT;
-- Request log, one row per proxied request.
-- estimated is 1 when token usage was estimated instead of reported.
CREATE TABLE requests (
 id TEXT PRIMARY KEY, account_id TEXT REFERENCES accounts(id), key_id TEXT REFERENCES api_keys(id),
 created_at INTEGER NOT NULL, method TEXT NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL,
 model TEXT NOT NULL, ip TEXT NOT NULL, status INTEGER NOT NULL DEFAULT 0,
 duration_ms INTEGER NOT NULL DEFAULT 0, bytes_in INTEGER NOT NULL DEFAULT 0, bytes_out INTEGER NOT NULL DEFAULT 0,
 tokens INTEGER NOT NULL DEFAULT 0 CHECK(tokens>=0), upstream_micro INTEGER NOT NULL DEFAULT 0 CHECK(upstream_micro>=0),
 billed_micro INTEGER NOT NULL DEFAULT 0 CHECK(billed_micro>=0), error TEXT, finished INTEGER NOT NULL DEFAULT 0,
 estimated INTEGER NOT NULL DEFAULT 0 CHECK(estimated IN (0,1)),
 input_tokens INTEGER NOT NULL DEFAULT 0 CHECK(input_tokens>=0), output_tokens INTEGER NOT NULL DEFAULT 0 CHECK(output_tokens>=0),
 cached_tokens INTEGER NOT NULL DEFAULT 0 CHECK(cached_tokens>=0), characters INTEGER NOT NULL DEFAULT 0 CHECK(characters>=0),
 audio_ms INTEGER NOT NULL DEFAULT 0 CHECK(audio_ms>=0), images INTEGER NOT NULL DEFAULT 0 CHECK(images>=0),
 -- Worst-case billed micro-USD and tokens held at admission. Holds of unfinished
 -- rows count against balance and limits until settlement closes the row.
 hold_micro INTEGER NOT NULL DEFAULT 0 CHECK(hold_micro>=0), hold_tokens INTEGER NOT NULL DEFAULT 0 CHECK(hold_tokens>=0)
) STRICT;
CREATE INDEX requests_key_time ON requests(key_id,created_at);
CREATE INDEX requests_account_time ON requests(account_id,created_at);
CREATE INDEX requests_time ON requests(created_at);
CREATE INDEX requests_in_flight ON requests(account_id,finished,created_at);
-- Hourly usage totals per key and model, used for limits and statistics.
CREATE TABLE usage_buckets (
 hour INTEGER NOT NULL, key_id TEXT NOT NULL REFERENCES api_keys(id), account_id TEXT NOT NULL REFERENCES accounts(id), model TEXT NOT NULL,
 requests INTEGER NOT NULL DEFAULT 0, tokens INTEGER NOT NULL DEFAULT 0, upstream_micro INTEGER NOT NULL DEFAULT 0, billed_micro INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(hour,key_id,model)
) STRICT;
CREATE INDEX usage_account_time ON usage_buckets(account_id,hour);
CREATE INDEX usage_key_time ON usage_buckets(key_id,hour);
-- Running totals across all requests (id is always 1).
CREATE TABLE totals (id INTEGER PRIMARY KEY CHECK(id=1), requests INTEGER NOT NULL DEFAULT 0, tokens INTEGER NOT NULL DEFAULT 0, upstream_micro INTEGER NOT NULL DEFAULT 0, billed_micro INTEGER NOT NULL DEFAULT 0) STRICT;
INSERT INTO totals(id) VALUES (1);
-- Orders for plans and top-ups. provider is stripe, payroof, apple, or google.
-- status goes from pending to paid, and to refunded when a store refunds it.
-- kind, price, credit, and months snapshot the plan at checkout.
-- external_id is the provider's identifier of the charge: Stripe session,
-- App Store transactionId, or Google Play orderId (each renewal has its own).
-- Store orders also record platform (ios or android), product_id, and
-- original_transaction_id (App Store) or purchase_token (Google Play).
CREATE TABLE payments (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), plan_code TEXT NOT NULL REFERENCES plans(code),
 provider TEXT NOT NULL CHECK(provider IN ('stripe','payroof','apple','google')),
 platform TEXT CHECK(platform IN ('ios','android')),
 kind TEXT NOT NULL CHECK(kind IN ('subscription','topup')),
 price_micro INTEGER NOT NULL CHECK(price_micro>0), credit_micro INTEGER NOT NULL CHECK(credit_micro>0),
 months INTEGER CHECK(months BETWEEN 1 AND 12),
 external_id TEXT, original_transaction_id TEXT, purchase_token TEXT, product_id TEXT, environment TEXT,
 status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','paid','refunded')),
 received_micro INTEGER NOT NULL DEFAULT 0 CHECK(received_micro>=0),
 subscription_id TEXT REFERENCES subscriptions(id),
 created_at INTEGER NOT NULL, expires_at INTEGER, paid_at INTEGER, paid_by TEXT, refunded_at INTEGER,
 CHECK((kind='subscription') = (months IS NOT NULL)),
 CHECK((provider='apple') = (platform IS 'ios')),
 CHECK((provider='google') = (platform IS 'android')),
 CHECK(provider<>'apple' OR (external_id IS NOT NULL AND original_transaction_id IS NOT NULL)),
 CHECK(provider<>'google' OR (external_id IS NOT NULL AND purchase_token IS NOT NULL))
) STRICT;
-- A provider charge id can be recorded only once per provider, so a store
-- receipt or notification delivered twice cannot credit twice.
CREATE UNIQUE INDEX payments_external ON payments(provider,external_id) WHERE external_id IS NOT NULL;
-- A Google Play one-time purchase token credits once.
CREATE UNIQUE INDEX payments_google_topup ON payments(purchase_token) WHERE provider='google' AND kind='topup';
CREATE INDEX payments_original ON payments(original_transaction_id) WHERE original_transaction_id IS NOT NULL;
CREATE INDEX payments_token ON payments(purchase_token) WHERE purchase_token IS NOT NULL;
CREATE INDEX payments_created ON payments(created_at DESC);
CREATE INDEX payments_account_created ON payments(account_id,created_at DESC);
-- Crypto quotes per payment and asset: address, amount, and USD rate.
CREATE TABLE payment_quotes (
 payment_id TEXT NOT NULL REFERENCES payments(id), chain TEXT NOT NULL, chain_name TEXT NOT NULL,
 asset TEXT NOT NULL, symbol TEXT NOT NULL, decimals INTEGER NOT NULL CHECK(decimals BETWEEN 0 AND 36),
 address TEXT NOT NULL, amount TEXT NOT NULL, rate_usd TEXT NOT NULL,
 PRIMARY KEY(payment_id,asset)
) STRICT;
-- On-chain transfers matched to a payment. status goes from seen to confirmed.
CREATE TABLE payment_transfers (
 id TEXT PRIMARY KEY, payment_id TEXT NOT NULL REFERENCES payments(id),
 chain TEXT NOT NULL, asset TEXT NOT NULL, address TEXT NOT NULL, tx_hash TEXT NOT NULL, amount TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('seen','confirmed')),
 value_micro INTEGER NOT NULL DEFAULT 0 CHECK(value_micro>=0), note TEXT NOT NULL DEFAULT '',
 seen_at INTEGER NOT NULL, confirmed_at INTEGER
) STRICT;
CREATE INDEX payment_transfers_payment ON payment_transfers(payment_id,seen_at);
-- Standing Payroof deposit addresses: one row per account and chain. The
-- Payroof reference of an account is "acct-" followed by its id.
CREATE TABLE deposit_addresses (
 account_id TEXT NOT NULL REFERENCES accounts(id), chain TEXT NOT NULL, address TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 PRIMARY KEY(account_id,chain)
) STRICT;
-- On-chain transfers to a standing deposit address. status goes from seen to
-- confirmed. credited_micro is the USD value credited to the account, which is
-- 0 when the transfer was not credited. note says why (for example
-- unsupported_asset).
CREATE TABLE deposit_transfers (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id),
 chain TEXT NOT NULL, asset TEXT NOT NULL, address TEXT NOT NULL, tx_hash TEXT NOT NULL, amount TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('seen','confirmed')),
 credited_micro INTEGER NOT NULL DEFAULT 0 CHECK(credited_micro>=0), note TEXT NOT NULL DEFAULT '',
 seen_at INTEGER NOT NULL, confirmed_at INTEGER
) STRICT;
CREATE INDEX deposit_transfers_account ON deposit_transfers(account_id,seen_at);
-- Processed payment webhook event ids. Repeated deliveries are ignored.
CREATE TABLE payment_events (id TEXT PRIMARY KEY, created_at INTEGER NOT NULL) STRICT;
-- Identifiers blocked after a store refund: kind ip (no self-service signup
-- from that address until expires_at) or apple_original_transaction (a later
-- purchase with that App Store original transaction suspends its account).
CREATE TABLE refund_blocks (
 kind TEXT NOT NULL CHECK(kind IN ('ip','apple_original_transaction')), value TEXT NOT NULL,
 account_id TEXT NOT NULL REFERENCES accounts(id), created_at INTEGER NOT NULL, expires_at INTEGER,
 PRIMARY KEY(kind,value)
) STRICT;
-- Solved proof-of-work challenges, so each one counts only once until it expires.
CREATE TABLE pow_spent (challenge TEXT PRIMARY KEY, expires_at INTEGER NOT NULL) STRICT;
CREATE INDEX pow_spent_expires ON pow_spent(expires_at);
-- OAuth 2.1 for the remote MCP endpoint. Clients register dynamically (public clients, RFC 7591).
CREATE TABLE oauth_clients (
 id TEXT PRIMARY KEY, name TEXT NOT NULL, client_uri TEXT, logo_uri TEXT,
 redirect_uris TEXT NOT NULL CHECK(json_valid(redirect_uris)),
 created_at INTEGER NOT NULL, created_ip TEXT NOT NULL
) STRICT;
CREATE INDEX oauth_clients_created ON oauth_clients(created_at);
-- A user's authorization of one client for one API key. Deleting a grant revokes its tokens.
CREATE TABLE oauth_grants (
 id TEXT PRIMARY KEY, client_id TEXT NOT NULL REFERENCES oauth_clients(id) ON DELETE CASCADE,
 key_id TEXT NOT NULL REFERENCES api_keys(id), scope TEXT NOT NULL, resource TEXT NOT NULL,
 created_at INTEGER NOT NULL, last_used_at INTEGER
) STRICT;
CREATE INDEX oauth_grants_key ON oauth_grants(key_id);
CREATE INDEX oauth_grants_client ON oauth_grants(client_id);
-- Single-use authorization codes (HMAC digests only), removed when exchanged.
CREATE TABLE oauth_codes (
 code_hash TEXT PRIMARY KEY, client_id TEXT NOT NULL REFERENCES oauth_clients(id) ON DELETE CASCADE,
 key_id TEXT NOT NULL REFERENCES api_keys(id), redirect_uri TEXT NOT NULL, code_challenge TEXT NOT NULL,
 resource TEXT NOT NULL, scope TEXT NOT NULL, expires_at INTEGER NOT NULL
) STRICT;
CREATE INDEX oauth_codes_expires ON oauth_codes(expires_at);
-- Access and refresh tokens (HMAC digests only). used=1 marks a rotated refresh token:
-- presenting it again revokes the whole grant.
CREATE TABLE oauth_tokens (
 hash TEXT PRIMARY KEY, grant_id TEXT NOT NULL REFERENCES oauth_grants(id) ON DELETE CASCADE,
 kind TEXT NOT NULL CHECK(kind IN ('access','refresh')), expires_at INTEGER NOT NULL,
 used INTEGER NOT NULL DEFAULT 0 CHECK(used IN (0,1))
) STRICT;
CREATE INDEX oauth_tokens_grant ON oauth_tokens(grant_id);
CREATE INDEX oauth_tokens_expires ON oauth_tokens(expires_at);
