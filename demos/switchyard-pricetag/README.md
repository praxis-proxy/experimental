# Switchyard + PriceTag USD budget demo

This is a local Docker Compose integration demo that proves external USD-budget
metering can reject an exhausted budget before a paid model is called, and can
route that denial to a free target only when a separate configuration opts in.
It uses the real PriceTag metering service and PostgreSQL, but a deterministic
local Switchyard judge and OpenAI-shaped model fixtures. No Kubernetes cluster
or paid-provider credential is required.

The primary model always returns two prompt tokens and three completion tokens.
PriceTag is seeded with a `$0.01` monthly budget and a `$10,000 / MTok` price,
so it admits the first request (spend is checked before use) and rejects the
next one after the first request's `$0.05` usage event is recorded.
The harness sends the fixed local-fixture identity header
`x-tenant-model: budgeted-model`; `external_metering` consumes and strips it,
so PriceTag uses the same fixed model for the preflight and usage event.

## Run

Clone PriceTag to `~/Projects/pricetag-metering` and check out the reviewed revision:

```console
git clone https://github.com/redhat-et/pricetag-metering.git ~/Projects/pricetag-metering
git -C ~/Projects/pricetag-metering fetch origin 557ceb1cbe1316d93e0f682baa8301455956fe18
git -C ~/Projects/pricetag-metering checkout 557ceb1cbe1316d93e0f682baa8301455956fe18
cd ~/Projects/praxis-proxy-experimental/demos/switchyard-pricetag
PRICETAG_METERING_SOURCE=~/Projects/pricetag-metering ./run-demo.sh
```

The helper builds PriceTag from `git archive` at that exact commit, then builds
the local gateway image. It does not use a mutable metering image tag or copy
the PriceTag checkout's uncommitted files into the image.

The script proves, in order:

1. One request reaches Switchyard's Weak target and PriceTag records its cost.
2. The next request is rejected by the external PriceTag budget with `HTTP 429`;
   the mock model's call counter remains unchanged.
3. The same exhausted-budget request, under the separate fallback configuration,
   reaches the configured free target exactly once.

PriceTag caches a positive entitlement for 15 seconds. The script deliberately
waits 16 seconds between the allowed request and the budget-denial assertion;
this makes the result independent of timing rather than relying on a stale
allowance.

Set `KEEP_CONTAINERS=1` to retain the stack after a run. It publishes only the
gateway at `127.0.0.1:18080` and the mock counter at `127.0.0.1:18090`; PostgreSQL
and PriceTag remain on the private Compose network. The fixed `demo-user` is a
single-user fixture; its fixed model header is likewise test input, not a
multi-tenant authentication design.

## Deliberate scope

The free fallback configuration has no token-rate limiter. Its only local
`429` is the `external_metering` rejection that follows PriceTag's
`hasAccess: false` entitlement answer. This keeps the demo's fallback
classification unambiguous and prevents it from making a policy claim about
unrelated `429` responses.
