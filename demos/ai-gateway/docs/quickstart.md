# Quickstart

An AI gateway in front of whichever providers you use, so your coding agents get
budgets, token counting and traces without holding provider keys.

## Choose the image

After this change is merged, use the published image:

```sh
export PRAXIS_IMAGE=ghcr.io/praxis-proxy/experimental:main
podman pull "$PRAXIS_IMAGE"
```

To test this PR before merge, build from the repository root instead:

```sh
podman build --build-arg FEATURES=otel \
  -t praxis-experimental:ai-gw -f Containerfile .
export PRAXIS_IMAGE=praxis-experimental:ai-gw
```

The PR's `Containerfile` embeds the configs used below under
`/usr/share/praxis/demo`; no config bind mount is needed.

Two paths. Pick by **who can reach the port**.

| | [Laptop](#laptop) | [Server](#server) |
| --- | --- | --- |
| Who reaches it | only you, loopback | other people, over a network |
| Who holds provider keys | **the gateway** | **the gateway** |
| How callers authenticate | they don't — loopback is the boundary | **a JWT per person**, validated by the `policy` filter |
| To start you need | Podman; Ollama for local models | a checked-out `issue-token.sh`; provider variables may be empty |
| Config | `configs/laptop.yaml` | `configs/server.yaml` + `configs/policy.yaml` |

Both give you the same three ports:

| Port | Serves | Pick it by |
| --- | --- | --- |
| `8080` | **Ollama and OpenAI** | the model you name |
| `8081` | Anthropic | pointing at the port |
| `8082` | OpenRouter | pointing at the port |

One container, one praxis process. Ports you never call cost nothing.

---

## Laptop

Pull the small local model once and leave Ollama running on its default
`localhost:11434` listener:

```sh
ollama pull qwen3.5:0.8b
```

Then start the gateway. Export a real hosted-provider key before this command only
when you intend to call that provider; otherwise the empty value keeps it optional.

```sh
export OPENAI_API_KEY="${OPENAI_API_KEY:-}"
export ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}"
export OPENROUTER_API_KEY="${OPENROUTER_API_KEY:-}"

podman run --replace -d --name praxis \
  -p 127.0.0.1:8080:8080 -p 127.0.0.1:8081:8081 \
  -p 127.0.0.1:8082:8082 -p 127.0.0.1:9901:9901 \
  -e OPENAI_API_KEY -e ANTHROPIC_API_KEY -e OPENROUTER_API_KEY \
  "$PRAXIS_IMAGE" \
  -c /usr/share/praxis/demo/configs/laptop.yaml
```

Check it:

```sh
curl --fail --silent --show-error localhost:9901/healthy
# {"status":"ok"}
```

`credential_injection` replaces a client placeholder with the gateway's provider
credential. An empty value still lets Praxis start; calling that provider then returns
its own authentication error. Budgets, token counting and traces apply either way.

### One port, several providers

`:8080` serves Ollama **and** OpenAI. The model you name decides which:

```sh
# Goes to your local Ollama; this should return HTTP 200 without a provider key.
curl --fail-with-body localhost:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3.5:0.8b","messages":[{"role":"user","content":"hi"}]}'

# Goes to OpenAI. Run only when OPENAI_API_KEY was exported before podman run.
curl --fail-with-body localhost:8080/v1/chat/completions \
  -H 'Content-Type: application/json' -H 'Authorization: Bearer placeholder' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}'
```

> [!IMPORTANT]
> Model names are matched **exactly** — there is no wildcard. A model that matches
> no route goes to **Ollama**, deliberately: an unrecognised name then fails
> locally and free, rather than being billed to a hosted provider because of a
> typo. To send a new hosted model out, add a route for it in `configs/laptop.yaml`
> next to `gpt-4o`.

### Point your agent at it

<details>
<summary><b>Claude Code</b></summary>

Claude Code speaks the Anthropic API, so use `:8081`:

```sh
export ANTHROPIC_BASE_URL=http://localhost:8081
export ANTHROPIC_AUTH_TOKEN=placeholder
export ANTHROPIC_MODEL=claude-sonnet-4-6
claude
```

This needs `ANTHROPIC_API_KEY` in the gateway container. Export it and re-run the
`podman run --replace` command if the gateway was started with an empty value.

It resolves **several** models in one session — subagents and the built-in
Explore and Plan agents each pick their own. Give each a budget rule or let the
catch-all cover them; see [coding-agents.md](coding-agents.md).
</details>

<details>
<summary><b>Codex</b></summary>

```toml
# ~/.codex/config.toml
model = "gpt-4o"
model_provider = "praxis"

[model_providers.praxis]
name = "praxis"
base_url = "http://localhost:8080/v1"
wire_api = "responses"
env_key = "PRAXIS_TOKEN"
```

```sh
export PRAXIS_TOKEN=placeholder
codex
```

This route forwards the Responses API unchanged, so the selected upstream must
implement `/v1/responses`. It does not translate Responses to Chat Completions.

</details>

<details>
<summary><b>opencode</b></summary>

```json
{
  "model": "praxis/gpt-4o",
  "small_model": "praxis/gpt-4o",
  "provider": {
    "praxis": {
      "npm": "@ai-sdk/openai-compatible",
      "options": {
        "baseURL": "http://localhost:8080/v1",
        "apiKey": "{env:PRAXIS_TOKEN}"
      },
      "models": { "gpt-4o": {}, "qwen3.5:0.8b": {} }
    }
  }
}
```

Run with `PRAXIS_TOKEN=placeholder`. Pinning `small_model` prevents title
generation from using opencode's hosted infrastructure instead of this gateway.
</details>

---

## Server

A shared host — a RHEL box or a VM. Two things change from the laptop path: **the
gateway holds the provider keys**, and **each caller needs a token**.

> [!WARNING]
> The listeners are plaintext HTTP. `--network host` exposes ports 8080–8082 on
> the host, so restrict them with a firewall and terminate TLS before allowing
> network clients. The admin endpoint remains on host loopback.

### 1. Generate the signing keypair, once

```sh
cd demos/ai-gateway
export PRAXIS_JWT_DIR="${PRAXIS_JWT_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/praxis-ai-gateway/jwt}"
./scripts/issue-token.sh init
```

The private half stays on this machine and mints tokens. The gateway only ever
gets the public half, so it can verify tokens but not issue them.

### 2. Start the gateway

Export the provider credentials you use and explicit empty values for the others.
Passing the variable names to Podman keeps their values out of this command:

```sh
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=
export OPENROUTER_API_KEY=

podman run --replace -d --name praxis --network host \
  -e OPENAI_API_KEY -e ANTHROPIC_API_KEY -e OPENROUTER_API_KEY \
  -v "$PRAXIS_JWT_DIR/public.pem:/etc/praxis/jwt-public.pem:ro" \
  "$PRAXIS_IMAGE" \
  -c /usr/share/praxis/demo/configs/server.yaml
```

It **fails closed** twice over — verified. A missing provider variable:

```text
fatal: credential_injection: environment variable 'OPENAI_API_KEY' not set for cluster 'openai'
```

and a missing public key, so it can never start with authentication silently off:

```text
fatal: policy: … decoding-key file '/etc/praxis/jwt-public.pem' unreadable
```

All three variables must exist. An empty value lets Praxis start but calls to that
provider receive its authentication error. Delete an unused chain only when making
your own config.

### 3. Issue each person a token

```sh
./scripts/issue-token.sh alice          # 30 days by default
./scripts/issue-token.sh ci-runner 30
```

### Use it directly

The token goes in the `Authorization` header — exactly where an API key would:

```sh
TOKEN=$(./scripts/issue-token.sh alice)

curl http://gateway.example.com:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}'
```

**Verified** against a running server:

| Request | Result |
| --- | --- |
| no token | `401` · `X-Policy-Violation: auth.malformed_header` |
| `Bearer not-a-jwt` | `401` · `auth.malformed_header: JWT not well-formed` |
| expired token | `401` · `X-Policy-Violation: auth.token_expired` |
| **valid token** | forwarded upstream; `200` when that upstream is healthy, and it sees the gateway's key rather than Alice's token |

### Use it from a harness

Your users hold a token, never a provider key. It goes wherever the API key went:

<details>
<summary><b>Claude Code</b> — Anthropic API, port 8081</summary>

```sh
export ANTHROPIC_BASE_URL=http://gateway.example.com:8081
export ANTHROPIC_AUTH_TOKEN=<the token you were issued>
claude
```

Use `ANTHROPIC_AUTH_TOKEN`, not `ANTHROPIC_API_KEY` — the former is sent as
`Authorization: Bearer`, which is what the policy filter reads.
</details>

<details>
<summary><b>Codex</b> — port 8080</summary>

```toml
# ~/.codex/config.toml
model = "gpt-4o"
model_provider = "praxis"

[model_providers.praxis]
name = "praxis"
base_url = "http://gateway.example.com:8080/v1"
wire_api = "responses"
env_key = "PRAXIS_TOKEN"
```

```sh
export PRAXIS_TOKEN=<the token you were issued>
```

</details>

<details>
<summary><b>opencode</b> — port 8080</summary>

```json
{
  "provider": {
    "praxis": {
      "npm": "@ai-sdk/openai-compatible",
      "options": {
        "baseURL": "http://gateway.example.com:8080/v1",
        "apiKey": "{env:PRAXIS_TOKEN}"
      },
      "models": { "gpt-4o": {}, "qwen3.5:0.8b": {} }
    }
  }
}
```

</details>

Rotating a provider key is now a restart on the server, not a message to every
user.

> [!WARNING]
> **There is no revocation.** Validation is offline — signature, issuer, audience,
> expiry — so a token stands until it expires. Withdrawing one person means
> rotating the keypair and reissuing everyone else. An IdP's `jwks_url` fixes key
> *rotation*, not per-token revocation; that needs introspection or a denylist,
> which this filter does not do. Tokens default to 30 days for this reason — keep
> them short.

### Authentication, honestly

| | Works with agents? | What it gives you |
| --- | --- | --- |
| **`policy`** — **active** | **yes** — the documented harness setup sends its JWT as `Bearer` | Per-caller identity. One token per developer or CI job. This is the gate. |
| `ip_acl` — optional, commented | yes, never reads a header | Allows whole CIDRs wholesale and cannot distinguish, meter or revoke an individual. A perimeter at best, not caller authentication. |
| `basic_auth` — avoid on these listeners | **no, with the documented setup** | Expects `Authorization: Basic …`, while the harness JWT occupies that same single header as `Bearer …`. Enabling both filters on one chain makes `basic_auth` reject the Bearer request. Use Basic only on a separate listener with a client configured to send it. |

> [!WARNING]
> If you enable `ip_acl`, know that with default podman/docker networking praxis
> sees the **bridge** address, not your caller's — verified, an outside request
> arrived as `client.address=10.88.0.117`. Inside `10.0.0.0/8`, so RFC1918 ranges
> would admit **everyone**. It means something only with `--network host` (used
> above) or behind a proxy whose `X-Forwarded-For` you trust.

> [!NOTE]
> **What this still does not give you: a per-caller budget.** `token_rate_limit`
> rules match request headers, not an identity, so every caller shares a rule's
> bucket. The policy filter now publishes `sub`; nothing consumes it yet.

<details>
<summary><b>Running it under systemd (Quadlet)</b></summary>

`/etc/containers/systemd/praxis.container`:

```ini
[Unit]
Description=Praxis AI gateway

[Container]
Image=ghcr.io/praxis-proxy/experimental:main
Network=host
Exec=-c /usr/share/praxis/demo/configs/server.yaml
EnvironmentFile=/etc/praxis/keys.env
Volume=/etc/praxis/jwt-public.pem:/etc/praxis/jwt-public.pem:ro

[Service]
Restart=always

[Install]
WantedBy=multi-user.target
```

Put the provider keys in `/etc/praxis/keys.env`, owned by root, mode `600`. Then
`systemctl daemon-reload && systemctl start praxis`.
</details>

---

## Dashboards

Both paths above are just the gateway. To add Prometheus, Tempo and the Perses
dashboards, see [podman.md](podman.md) (compose) or [kind.md](kind.md)
(Kubernetes). Budget behaviour is walked through in [budgets.md](budgets.md).

## Where your keys are stored

Never in this repository. The demo scripts use, in order: an exported variable
(used as-is, never stored), then the OS keychain (macOS Keychain or libsecret),
then a mode-600 file under `$XDG_CONFIG_HOME`. `./demo-podman forget openai`
deletes a stored key.
