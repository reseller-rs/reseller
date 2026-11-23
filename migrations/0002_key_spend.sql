-- Lifetime billed spend per key in micro-USD. The meter adds every billed
-- amount, so the value survives usage bucket and request log retention. An
-- account's lifetime spend is the sum over its keys (keys are never deleted).
ALTER TABLE api_keys ADD COLUMN billed_micro INTEGER NOT NULL DEFAULT 0 CHECK(billed_micro>=0);
-- Backfill from the hourly buckets, the longest-kept usage record.
UPDATE api_keys SET billed_micro=COALESCE((SELECT SUM(u.billed_micro) FROM usage_buckets u WHERE u.key_id=api_keys.id),0);
