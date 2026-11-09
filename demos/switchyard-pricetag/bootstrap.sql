-- A deliberately tiny budget plus a deliberately high price make one fixed
-- mock response exhaust the account. PriceTag checks existing spend before a
-- request, so the first request is admitted and the next is rejected.
UPDATE quota_policy
SET default_monthly_usd = 0.01,
    enforced = true,
    allowed_over_limit_models = '{}',
    over_cap_ceiling_usd = NULL;

INSERT INTO model_pricing (
  model, provider, input_cost_per_mtok, output_cost_per_mtok,
  cache_write_cost_per_mtok, cache_read_cost_per_mtok
)
VALUES ('budgeted-model', 'demo', 10000, 10000, 0, 0)
ON CONFLICT (model) DO UPDATE
SET provider = EXCLUDED.provider,
    input_cost_per_mtok = EXCLUDED.input_cost_per_mtok,
    output_cost_per_mtok = EXCLUDED.output_cost_per_mtok,
    cache_write_cost_per_mtok = EXCLUDED.cache_write_cost_per_mtok,
    cache_read_cost_per_mtok = EXCLUDED.cache_read_cost_per_mtok;
