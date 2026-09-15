# Token budgets

`token_rate_limit` is the filter that makes this a gateway rather than a proxy.
It holds a set of rules, evaluated in order, first match wins, and a rule with no
`match` is the catch-all.

Rules match on request headers. The one that matters is `X-Model`, which
`model_to_header` promotes from the request body — so a rule applies to Codex,
opencode and Claude Code without any of them sending a custom header, which none
of them let you do.

## The shipped rule sets

`configs/laptop.yaml` and `configs/server.yaml` use the same budgets. The
deployment difference is authentication: the server validates a JWT and strips
caller-supplied `X-Tier`; it does not give each identity a separate bucket.

The OpenAI-compatible listener uses:

| Rule | Match | Budget |
| --- | --- | --- |
| `premium` | `X-Tier: premium` | 20,000 tokens/minute |
| `agent-daily`* | large local-model name | 10,000,000 tokens/day |
| `gpt-4o`, `gpt-4o-mini` | exact hosted-model name | 40,000 tokens/minute |
| `free` | catch-all | 5,000 tokens/minute |

\* plus identical rules for `qwen3-coder:30b` and `deepseek-r1:32b`. A rule
matches one exact header value, so each model needs its own; the small models are
deliberately left on the catch-all.

The native Anthropic listener gives Sonnet and Haiku 40,000 tokens/minute and
uses a 10,000-token/minute catch-all. The OpenRouter listener gives
`openai/gpt-4o` 40,000 tokens/minute and has the same 10,000-token/minute
catch-all. On the server, `premium` is deliberately unreachable until trusted
identity is mapped to a tier.

A daily budget is the same filter with a longer window — that is all a "total
budget" is here. `window` takes `ms`/`s`/`m`/`h`: a day is `"24h"`, a week
`"168h"`. **`"7d"` is rejected**; praxis logs `invalid duration '7d'`, refuses the
reload and keeps running on the previous config.

> `token_rate_limit` does **not** authenticate. A header-matched rule trusts
> whatever reached it, so a tier header must be set by an auth filter and client
> copies stripped. The `policy` filter (active in `configs/server.yaml`) now does
> publish an identity — `sub`, from the caller's JWT — but no budget rule can key
> on it yet, so everyone matching a rule still shares its bucket. That is the one
> gap between "we know who you are" and "we can meter you". Tracked at grid#101.

## Setting a cap that means something

Four numbers per rule, and each answers a different question.

| Field | The question it answers |
| --- | --- |
| `window` | over what period am I capping? |
| `capacity` | how many tokens am I willing to spend in that period? |
| `reserved_tokens` | what does one request cost, roughly? |
| `reservation_timeout` | how slow is the slowest response? |

**Pick the window from what you are protecting.** A GPU queue is a
*rate* problem — other people are waiting, so `1m` or `5m`. A provider bill is a
*total* problem — nobody is waiting, you just do not want a surprise invoice, so
`24h` or `168h`. Using a per-minute window to cap spend is the common mistake: it
throttles bursts and still lets you spend all month.

**Measure `reserved_tokens`, do not guess it.** Measured through this gateway: a
bare `curl` costs ~406 tokens, and one trivial Codex turn costs **9,471**, because
the harness ships system prompts and tool schemas you never see. Too low and every
request records overage — 7,971 tokens of it over four turns at `1500`. Too high
and you refuse requests while capacity sits unused, because the estimate is held
until the response settles.

**Then `capacity` is arithmetic.** Decide the work, multiply:

```text
one agent turn          ~9,000 tokens      (measured, not assumed)
a working day           ~100 turns
                      = 900,000 tokens/day
round up for headroom  1,000,000
```

The shipped `agent-daily` uses 10,000,000, about 1,000 turns — a heavy day and
not a demo allowance, because against a local model the cap protects a GPU queue
rather than a bill.

**For a spend cap, convert through the price.** praxis caps tokens, not money —
cost-based limiting does not exist anywhere in the project yet — so do the
division yourself and write the result in a comment next to the rule:

```text
$50/month ÷ $2.50 per 1M output tokens ≈ 20,000,000 tokens
window: "720h"   capacity: 20000000
```

Two caveats that make that number optimistic. Input and output are priced
differently and `token_rate_limit` counts them together, so size against the
*output* price to stay conservative. And a cached input token is billed at a
fraction of a fresh one but counted the same here, so a prompt-cache-heavy
workload will hit the cap before it hits the bill.

**Sanity checks before you trust a rule.**

- Is there a trailing rule with no `match`? Without one, anything unmatched is
  **not limited at all** — against a paid provider, the expensive default.
- Does every model you actually use have a rule? `match.headers` is an exact
  string with no wildcard, so an unlisted model silently lands on the catch-all.
  One harness session can produce two to four distinct model names; see
  [coding-agents.md](coding-agents.md).
- Is `reservation_timeout` longer than your slowest response? The default is 30s,
  and a 27B model takes 34–78s, so the reservation is reaped as orphaned and the
  request is charged the flat estimate.
- Is `capacity` at least a few times `reserved_tokens`? A rule that admits three
  requests before refusing is a broken rule, not a strict one.

<details>
<summary>Why a budget only trips under concurrent load</summary>

`token_rate_limit` reserves an estimate at admission and refunds the unused part
when the response completes. One request at a time is refunded faster than the
window fills, so a sequential loop never hits the limit no matter how long it
runs. `scripts/rate-limit-demo.sh` and `scripts/verify.sh` both drive concurrency
(`xargs -P 8`) for this reason.

A consequence worth knowing before tuning: with a sliding window, capacity frees
continuously. If anything ever switched models on exhaustion and switched back
as
soon as capacity appeared, it would oscillate roughly every
`reserved_tokens / capacity x window` — about 86 seconds at the numbers above.

</details>
