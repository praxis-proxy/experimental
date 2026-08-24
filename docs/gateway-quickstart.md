# Experimental AI Gateway — build and run

How to build the experimental gateway image.

Everything below was run against the image built from this repo.

## What is it?

This is a version of Praxis with experimental features and filters enabled
by default. To allow users to play with our feature-gated work without
needing to dive into the codebase.

## Build

```console
docker build -t praxis-experimental:local -f Containerfile .
```

Podman works too, substitute `podman` throughout if it's your preferred
container engine.

## Run

`examples/configs/gateway.yaml` is an annotated sample config meant to be
used with the experimental release.

```console
docker run --rm -p 8080:8080 \
  -v "$PWD/examples/configs/gateway.yaml:/etc/praxis/praxis.yaml:ro" \
  praxis-experimental:local
```

The config is read from `/etc/praxis/praxis.yaml`, which is the image's
working directory.

Check it is alive, the admin listener binds loopback, so query it from inside
the container:

```console
docker exec <container> wget -qO- http://127.0.0.1:9901/healthy
{"status":"ok"}
```

Send a request:

```console
curl -X POST http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}]}'
```

A `via: 1.1 praxis` response header confirms it went through the gateway.

### Pointing at a real provider

Replace the endpoint in the `provider` cluster with your provider's host, add
a `tls` block, and drop `insecure_options` (it is only there because the
default endpoint is a private address):

```yaml
      - filter: load_balancer
        clusters:
          - name: provider
            endpoints:
              - "api.openai.com:443"
            tls:
              sni: api.openai.com
```

Then uncomment the `credential_injection` block and pass the key in:

```console
docker run --rm -p 8080:8080 \
  -e OPENAI_API_KEY="$OPENAI_API_KEY" \
  -v "$PWD/examples/configs/gateway.yaml:/etc/praxis/praxis.yaml:ro" \
  praxis-experimental:local
```

## Current Features

Each commented block in the sample config has been verified to load in this
image. Uncomment, supply what it needs, restart.

### Provider fallback

Uncomment the `failover` chain, then point the listener at it by changing
`filter_chains: [main]` to `[failover]`. Traffic goes to the primary; on 429,
502, 503, or 504 it retries against the secondary. Each step keeps its own
cluster and credentials.

Verified by killing the primary mid-run: requests continued to be served from
the secondary with no client-visible error.

### Guardrails (Lakera Guard)

Uncomment the `safety-check` chain, then add it to the listener ahead of
`main`: `filter_chains: [safety-check, main]`. Set `LAKERA_API_KEY` in the
environment. Request bodies are screened before they reach the provider;
flagged requests get a 403 and never touch the upstream.

This one runs as its own chain rather than an inline filter, so it can
terminate a request early.

Verified against a local stub standing in for the Lakera API: a `flagged`
verdict returned 403 with the backend untouched, and a clean verdict passed
through to the backend.

### Guardrails (NeMo)

Uncomment the `ai_guardrails` block and point it at a reachable NeMo endpoint.
Blocked and modified verdicts return 403.

Request-side only. Setting `phase.response: true` is rejected at startup on
purpose — response-side evaluation is not implemented yet
([ai#580](https://github.com/praxis-proxy/ai/issues/580)).

### Upstream credentials

Uncomment `credential_injection` to attach a provider key to outbound
requests and strip whatever the client sent. Prefer `env_var` over an inline
value.
