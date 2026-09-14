CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL CHECK(json_valid(value))) STRICT;
CREATE TABLE accounts (
 id TEXT PRIMARY KEY, contact TEXT NOT NULL DEFAULT '', note TEXT NOT NULL DEFAULT '',
 status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','suspended')),
 can_create_keys INTEGER NOT NULL DEFAULT 0 CHECK(can_create_keys IN (0,1)),
 markup_pct TEXT, debt_micro INTEGER NOT NULL DEFAULT 0 CHECK(debt_micro>=0),
 created_at INTEGER NOT NULL, created_ip TEXT NOT NULL
) STRICT;
CREATE TABLE api_keys (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), parent_key_id TEXT REFERENCES api_keys(id),
 key_hash TEXT NOT NULL UNIQUE, dash_hash TEXT NOT NULL UNIQUE, key_prefix TEXT NOT NULL,
 name TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','blocked','revoked')),
 limits TEXT NOT NULL CHECK(json_valid(limits)), markup_pct TEXT, expires_at INTEGER,
 created_at INTEGER NOT NULL, created_ip TEXT NOT NULL, bypass_ip INTEGER NOT NULL DEFAULT 0,
 last_used_at INTEGER
) STRICT;
CREATE INDEX keys_account ON api_keys(account_id);
CREATE INDEX keys_ip_time ON api_keys(created_ip,created_at);
CREATE TABLE credits (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), source TEXT NOT NULL,
 amount_micro INTEGER NOT NULL CHECK(amount_micro>0), remaining_micro INTEGER NOT NULL CHECK(remaining_micro>=0 AND remaining_micro<=amount_micro),
 expires_at INTEGER, created_at INTEGER NOT NULL, note TEXT NOT NULL DEFAULT ''
) STRICT;
CREATE INDEX credits_account ON credits(account_id,expires_at);
CREATE TABLE plans (
 code TEXT PRIMARY KEY, name TEXT NOT NULL, price_micro INTEGER NOT NULL CHECK(price_micro>0),
 credit_micro INTEGER NOT NULL CHECK(credit_micro>0), duration_days INTEGER NOT NULL CHECK(duration_days BETWEEN 1 AND 3650),
 active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1)), description TEXT NOT NULL DEFAULT ''
) STRICT;
INSERT INTO plans VALUES ('monthly','Monthly',20000000,20000000,30,1,'Thirty days of API credit'),('extended','Extended',100000000,100000000,180,1,'Six months of API credit'),('year','Yearly',160000000,160000000,360,1,'360 days of API credit');
CREATE TABLE subscriptions (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), plan_code TEXT NOT NULL REFERENCES plans(code),
 started_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
) STRICT;
CREATE INDEX subscriptions_account ON subscriptions(account_id,expires_at);
CREATE TABLE redeem_codes (
 id TEXT PRIMARY KEY, code_hash TEXT NOT NULL UNIQUE, code_prefix TEXT NOT NULL,
 plan_code TEXT REFERENCES plans(code), credit_micro INTEGER,
 expires_at INTEGER, redeemed_by TEXT REFERENCES accounts(id), redeemed_at INTEGER,
 created_at INTEGER NOT NULL, note TEXT NOT NULL DEFAULT '',
 CHECK((plan_code IS NOT NULL AND credit_micro IS NULL) OR (plan_code IS NULL AND credit_micro>0))
) STRICT;
CREATE TABLE requests (
 id TEXT PRIMARY KEY, account_id TEXT REFERENCES accounts(id), key_id TEXT REFERENCES api_keys(id),
 created_at INTEGER NOT NULL, method TEXT NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL,
 model TEXT NOT NULL, ip TEXT NOT NULL, status INTEGER NOT NULL DEFAULT 0,
 duration_ms INTEGER NOT NULL DEFAULT 0, bytes_in INTEGER NOT NULL DEFAULT 0, bytes_out INTEGER NOT NULL DEFAULT 0,
 tokens INTEGER NOT NULL DEFAULT 0 CHECK(tokens>=0), upstream_micro INTEGER NOT NULL DEFAULT 0 CHECK(upstream_micro>=0),
 billed_micro INTEGER NOT NULL DEFAULT 0 CHECK(billed_micro>=0), error TEXT, finished INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX requests_key_time ON requests(key_id,created_at);
CREATE INDEX requests_account_time ON requests(account_id,created_at);
CREATE INDEX requests_time ON requests(created_at);
CREATE TABLE usage_buckets (
 hour INTEGER NOT NULL, key_id TEXT NOT NULL REFERENCES api_keys(id), account_id TEXT NOT NULL REFERENCES accounts(id), model TEXT NOT NULL,
 requests INTEGER NOT NULL DEFAULT 0, tokens INTEGER NOT NULL DEFAULT 0, upstream_micro INTEGER NOT NULL DEFAULT 0, billed_micro INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(hour,key_id,model)
) STRICT;
CREATE INDEX usage_account_time ON usage_buckets(account_id,hour);
CREATE TABLE totals (id INTEGER PRIMARY KEY CHECK(id=1), requests INTEGER NOT NULL DEFAULT 0, tokens INTEGER NOT NULL DEFAULT 0, upstream_micro INTEGER NOT NULL DEFAULT 0, billed_micro INTEGER NOT NULL DEFAULT 0) STRICT;
INSERT INTO totals(id) VALUES (1);
CREATE TABLE payments (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), plan_code TEXT NOT NULL REFERENCES plans(code),
 price_micro INTEGER NOT NULL, credit_micro INTEGER NOT NULL, duration_days INTEGER NOT NULL,
 session_id TEXT UNIQUE, status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','paid')),
 created_at INTEGER NOT NULL, paid_at INTEGER
) STRICT;
CREATE TABLE payment_events (id TEXT PRIMARY KEY, created_at INTEGER NOT NULL) STRICT;
