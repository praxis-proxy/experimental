# Identity metadata contract

> **Status: proposed.** The key names below are a proposal and need maintainer
> sign-off before consumers build on them. Some downstream designs
> (praxis-proxy/ai#698, praxis-proxy/ai#130) are still open, so treat this as a
> living contract until those land.

This document defines the `filter_metadata` keys and value formats that
identity-*producing* filters write and that identity-*consuming* filters read
inside a single Praxis request pipeline. It exists so that Track B's
`api_key_auth` and the metering/budget consumers of the Standalone AI Gateway MVP
epic (praxis-proxy/ai#758) can be built against a stable, agreed surface.

## 1. Purpose and carrier

Identity facts travel *inside* the filter pipeline via `ctx.filter_metadata`, a
flat `HashMap<String, String>` written with `ctx.set_metadata(key, value)`. The
context enforces limits (from praxis `filter/src/context.rs`):

- key ≤ 64 bytes,
- value ≤ 256 bytes,
- ≤ 128 entries total.

Over-limit writes are **silently dropped with a `tracing::warn!`** — producers
must validate or truncate deliberately and must never rely on the silent drop.

Request **headers are the ingress representation only**. A producer that reads
identity from an inbound header MUST strip that header before the request is
forwarded upstream, so client-supplied identity headers can never be spoofed
through the proxy (the capture-and-strip semantics of praxis-proxy/ai#698).

Metadata is **not** a routing-match surface: the core `router` filter matches on
headers, not on `filter_metadata`. This contract is for observability,
inter-filter contracts, and consumer branching only.

## 2. Keys

All identity keys are dot-namespaced under `identity.`:

| Key                     | Required                 | Meaning                                                                         |
| ----------------------- | ------------------------ | ------------------------------------------------------------------------------- |
| `identity.user`         | yes, if any producer ran | Stable principal identifier (username or key ID).                               |
| `identity.group`        | optional                 | Group or team the principal belongs to.                                         |
| `identity.subscription` | optional                 | Plan / tier (metering in praxis-proxy/ai#577 expects user/group/subscription).  |
| `identity.method`       | recommended              | How identity was established: `api_key` \| `header` \| `jwt`.                   |
| `identity.attr.<name>`  | optional                 | Reserved namespace for pass-through attributes (e.g. captured `x-tenant-*`).    |

## 3. Value format

- UTF-8, ≤ 256 bytes, no control characters.
- The producer normalizes: trim surrounding whitespace; **case-preserving**.
  Consumers MUST NOT case-fold when comparing.
- Producers validate or truncate before writing; do not rely on the context's
  silent over-limit drop.

## 4. Producers

- **`api_key_auth`** (Track B, experimental): resolves a key to identity from
  inline/file/env config, following core `basic_auth`. Sets `identity.user`
  (key ID or configured principal), optionally `identity.group` /
  `identity.subscription`, and `identity.method = api_key`.
- **Future:** an identity-header guard (praxis-proxy/ai#698) mapping e.g.
  `x-tenant-username` → `identity.user` (`identity.method = header`); tenant-ID
  extraction (praxis-proxy/ai#130) mapping `X-Tenant-ID` → `identity.user`, with
  an optional JWT-claims source (`identity.method = jwt`).

Rule: **exactly one producer should be active per pipeline.** A later producer
MUST NOT overwrite an existing `identity.user`.

## 5. Consumers

- External metering (praxis-proxy/ai#577): balance check keyed by `identity.user`
  (and group/subscription).
- `token_ceiling` (Track A): per-key/per-user budget.
- Per praxis-proxy/ai#130: rate limiting, routing decisions, logging, metrics.

Consumers decide their own policy when identity is **absent**. Metering and
budget filters SHOULD be configurable between reject-vs-anonymous.

## 6. Non-goals

- Authorization semantics (what a principal may do) — this is identity only.
- Key formats / credential validation rules — producer-specific.
- **Session** identity. Session is a separate axis with its own keys
  (`x-switchyard-session-id` is a *session*, not a *principal*); see the
  `switchyard_route` docs. Do not conflate the two.

## 7. Cross-references

- Parent epic: praxis-proxy/ai#758 (Track B `api_key_auth` is the first consumer).
- praxis-proxy/ai#698 (identity-header capture-and-strip).
- praxis-proxy/ai#577 (external metering).
- praxis-proxy/ai#130 (tenant-ID extraction; rate limiting / routing / logging).
- `switchyard_route` docs (session key axis, deliberately separate).
