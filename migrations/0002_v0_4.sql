-- v0.4.0: signup proof-of-work replay protection and the settings reshape.

-- Solved ALTCHA challenges; rows are purged once their expiry has passed.
CREATE TABLE pow_spent (
 challenge TEXT PRIMARY KEY, expires_at INTEGER NOT NULL
) STRICT;
CREATE INDEX pow_spent_expires ON pow_spent(expires_at);

-- Move the signup/key-policy settings under their new sections. Only keys
-- that exist are copied (json(value->'$.x') keeps their JSON type); anything
-- absent falls back to the built-in default when the settings are loaded.
UPDATE settings SET value=json_set(
  value,
  '$.signup', json_patch('{}', json_object(
    'free_credit_usd', json(value->'$.free_credit_usd'),
    'free_credit_expiry_days', json(value->'$.free_credit_expiry_days'),
    'ip_limit_per_month', json(value->'$.ip_limit_per_month'),
    'cooldown_seconds', json(value->'$.cooldown_seconds'),
    'global_hourly_limit', json(value->'$.global_hourly_limit'),
    'ip_overrides', json(value->'$.ip_overrides')
  )),
  '$.keys', json_patch('{}', json_object(
    'max_per_account', json(value->'$.max_keys_per_account'),
    'default_limits', json(value->'$.default_limits')
  ))
) WHERE id=1;

UPDATE settings SET value=json_remove(
  value,
  '$.free_credit_usd', '$.free_credit_expiry_days', '$.ip_limit_per_month',
  '$.cooldown_seconds', '$.global_hourly_limit', '$.max_keys_per_account',
  '$.ip_overrides', '$.default_limits'
) WHERE id=1;

-- Token counts of a request were estimated by the proxy because the upstream
-- reported no usage (1) rather than provider-reported (0).
ALTER TABLE requests ADD COLUMN estimated INTEGER NOT NULL DEFAULT 0 CHECK(estimated IN (0,1));

-- Usage breakdown behind `tokens`, so every charge can be audited: input
-- (including cached) and output tokens, cached input tokens, TTS input
-- characters, audio milliseconds, and generated images.
ALTER TABLE requests ADD COLUMN input_tokens INTEGER NOT NULL DEFAULT 0 CHECK(input_tokens>=0);
ALTER TABLE requests ADD COLUMN output_tokens INTEGER NOT NULL DEFAULT 0 CHECK(output_tokens>=0);
ALTER TABLE requests ADD COLUMN cached_tokens INTEGER NOT NULL DEFAULT 0 CHECK(cached_tokens>=0);
ALTER TABLE requests ADD COLUMN characters INTEGER NOT NULL DEFAULT 0 CHECK(characters>=0);
ALTER TABLE requests ADD COLUMN audio_ms INTEGER NOT NULL DEFAULT 0 CHECK(audio_ms>=0);
ALTER TABLE requests ADD COLUMN images INTEGER NOT NULL DEFAULT 0 CHECK(images>=0);
