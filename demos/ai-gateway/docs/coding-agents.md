# Pointing a coding agent at the gateway

The demo exposes OpenAI-compatible and native Anthropic traffic on different
listeners. Set both endpoints once; each recipe below uses the matching one.

```bash
export OPENAI_GATEWAY=http://localhost:8080       # compose
export ANTHROPIC_GATEWAY=http://localhost:8083
# export OPENAI_GATEWAY=http://localhost:38080     # KIND
# export ANTHROPIC_GATEWAY=http://localhost:38081
```

> **The key in these blocks is a placeholder.** Codex and Claude Code refuse to
> start without something in their key variable, and `credential_injection`
> replaces whatever they send with the gateway's own. Put the real provider key
> in the gateway's environment — `OPENAI_API_KEY` for the OpenAI-compatible
> listener or `ANTHROPIC_API_KEY` for the Anthropic listener — or store it once
> with `./demo-podman remember <provider>`, which keeps it in your OS keychain.
> The agent never holds the real provider credential.

> **That key has to be an API key, not a subscription.** A Claude Pro/Max or
> ChatGPT Plus login authenticates the vendor's own client against the vendor's
> own endpoint; it is not a credential a gateway can hold or bill. Anthropic's
> consumer terms allow automated access only with an API key, Codex gates
> several behaviours on OpenAI's own hostnames, and opencode removed its
> Anthropic subscription login in 1.3.0 after a legal request. Use a pay-as-you-go
> API key, or a cloud-provider path such as Bedrock, Vertex or Azure OpenAI.

> **Use a big model for the local OpenAI-compatible recipes.** One agent turn
> costs ~9,000 tokens, and only
> `qwen3.8:27b`, `qwen3-coder:30b` and `deepseek-r1:32b` carry an `agent-daily`
> budget. `qwen3.5:0.8b` and `qwen2.5:3b` fall through to the catch-all `free`
> tier at 5,000 tokens/min and are **429'd on the first turn** — they exist for
> the burst demo, not for agents. `up` pulls only the small one;
> `ollama pull qwen3.8:27b` gets you an agent-sized model.

## One session, several models

This is the part that surprises people. A budget rule matches `X-Model` as an
**exact string** with no wildcard, so every distinct model an agent uses needs
its own rule or falls through to the catch-all — and several agents do not use
one model.

| Agent | Model strings out of the box | What controls the rest |
| --- | --- | --- |
| Claude Code | **2–4** | `CLAUDE_CODE_SUBAGENT_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, per-agent frontmatter |
| Codex | **1** | `review_model`, `agents.default_subagent_model`, `memories.*` — all default to the session model |
| opencode | **1 here, plus one that never arrives** | `small_model` — see the warning below |
| Aider | **1** | `--weak-model`, `--editor-model` default to the main model |
| Continue.dev | **2 by design** | `roles:` — `chat` and `autocomplete` are meant to differ |

Codex and Aider are cheap to budget: one rule each unless someone has
deliberately split a role out. Claude Code and Continue.dev need several rules.
opencode needs attention for a different reason.

### Claude Code

Point it at the gateway with environment alone; it needs no config file.

```bash
ANTHROPIC_BASE_URL="$ANTHROPIC_GATEWAY" \
ANTHROPIC_AUTH_TOKEN=dummy \
ANTHROPIC_MODEL=claude-sonnet-4-6 \
  claude
```

Use `$ANTHROPIC_GATEWAY` above. This listener preserves Anthropic's native
`/v1/messages` protocol and forwards to Anthropic, so the gateway must hold an
`ANTHROPIC_API_KEY`. The current demo does not configure an
`anthropic_to_openai` chain; pointing Claude Code at the OpenAI-compatible
listener or selecting a local Qwen model will not translate the request.

<details>
<summary>All eight variables, and how they resolve</summary>

Claude Code resolves several models independently:

| Variable | What it selects |
| --- | --- |
| `ANTHROPIC_MODEL` | the current session's main model |
| `ANTHROPIC_DEFAULT_MODEL` | the default for new sessions |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` | what the `opus` alias resolves to |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | what `sonnet` resolves to |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | what `haiku` resolves to, and background tasks |
| `ANTHROPIC_DEFAULT_FABLE_MODEL` | what `fable` resolves to |
| `CLAUDE_CODE_SUBAGENT_MODEL` | every subagent |
| `CLAUDE_CODE_SUBAGENT_MODEL_FORCE` | makes the above win over everything below |

A subagent in `.claude/agents/*.md` can also pin its own `model:` in
frontmatter, taking an alias (`sonnet`, `opus`, `haiku`, `fable`, `inherit`) or
a full model ID. Resolution runs per-invocation argument, then that frontmatter,
then `CLAUDE_CODE_SUBAGENT_MODEL`, then the main model, and
`CLAUDE_CODE_SUBAGENT_MODEL_FORCE` overrides all three.

So one session typically shows the gateway **two to four** distinct model
strings: the main model, whatever subagents resolved to, and the haiku model for
background tasks. Pin them all to collapse that to one:

```bash
ANTHROPIC_BASE_URL="$ANTHROPIC_GATEWAY" \
ANTHROPIC_AUTH_TOKEN=dummy \
ANTHROPIC_MODEL=claude-sonnet-4-6 \
CLAUDE_CODE_SUBAGENT_MODEL=claude-sonnet-4-6 \
CLAUDE_CODE_SUBAGENT_MODEL_FORCE=1 \
ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-sonnet-4-6 \
  claude
```

Leave them unpinned instead and the token-budget dashboard shows one series per
model. `laptop.yaml` has explicit Sonnet and Haiku rules; other model strings
land on its catch-all rule.

</details>

## Codex

Adds a provider named after the gateway. Safe to re-run.

```bash
mkdir -p ~/.codex
PROVIDER="praxis-${OPENAI_GATEWAY##*:}"
grep -q "^\[model_providers.$PROVIDER\]" ~/.codex/config.toml 2>/dev/null || cat >> ~/.codex/config.toml <<CODEX

[model_providers.$PROVIDER]
name = "Praxis ($OPENAI_GATEWAY)"
base_url = "$OPENAI_GATEWAY/v1"
wire_api = "responses"
env_key = "OPENAI_API_KEY"
CODEX
echo "provider: $PROVIDER"
```

```bash
OPENAI_API_KEY=dummy codex \
  -c model_provider="praxis-${OPENAI_GATEWAY##*:}" \
  -c review_model=qwen3.8:27b \
  -c agents.default_subagent_model=qwen3.8:27b \
  --model qwen3.8:27b
```

The last two are the secondary roles. Both default to the session model, so they
are only worth naming if you want `/review` or spawned subagents on a different
one — set them to a distinct model and the gateway sees three strings instead of
one. `-c` is used rather than the file because appending a bare key after a
`[model_providers.…]` header would land it inside that table.

If you turn on `memories.generate_memories`, `memories.extract_model` and
`memories.consolidation_model` join the list on the same terms.

`OPENAI_BASE_URL` alone does **not** redirect Codex: it reads the endpoint from
the provider block and the key from `~/.codex/auth.json`, so the provider is
required. A custom provider id is also required — the built-in `openai`,
`ollama` and `lmstudio` ids ignore TOML overrides.

`wire_api = "responses"` sends Codex traffic to `/v1/responses`. The current
gateway route forwards that wire format; it does not translate Responses API
requests into Chat Completions. Select an upstream that implements Responses.

Dropping `env_key` entirely makes Codex send no `Authorization` header at all,
which is tidier than a dummy if your gateway does not check one. The block above
keeps it so the same recipe works against a gateway that does.

Codex uses one model per session unless someone sets `review_model`,
`agents.default_subagent_model`, or the `memories.*` models — each defaults to
the session model, and memories are off by default.

## opencode

Writes the provider and preselects it, so opencode opens ready to use. It
consolidates into whichever config file already exists; having both
`opencode.json` and `opencode.jsonc` is ambiguous and makes providers silently
fail to appear.

> Unlike the Codex block this **rewrites** the target file: it re-serialises it
> as plain JSON, so comments in a `.jsonc` are lost, and it deletes the other
> file once merged. Back up an existing config first.

```bash
python3 - <<OPENCODE
import json, pathlib
GATEWAY = "$OPENAI_GATEWAY"
d = pathlib.Path.home() / ".config" / "opencode"
d.mkdir(parents=True, exist_ok=True)
jsonc, plain = d / "opencode.jsonc", d / "opencode.json"
target = jsonc if jsonc.exists() else plain
cfg = {}
for f in (plain, jsonc):
    if f.exists() and f.read_text().strip():
        try: cfg.update(json.loads(f.read_text()))
        except ValueError: pass
models = {m: {"name": m} for m in ["qwen3.8:27b", "qwen3-coder:30b", "deepseek-r1:32b"]}
cfg["\$schema"] = "https://opencode.ai/config.json"
cfg.setdefault("provider", {})["praxis-local"] = {
    "npm": "@ai-sdk/openai-compatible",
    "options": {"baseURL": GATEWAY + "/v1"}, "models": models}
M = "praxis-local/qwen3.8:27b"
cfg["model"] = cfg["small_model"] = M
cfg.setdefault("agent", {})
for a in ("plan", "build"):
    cfg["agent"].setdefault(a, {})["model"] = M
target.write_text(json.dumps(cfg, indent=2) + "\n")
for f in (plain, jsonc):
    if f != target and f.exists(): f.unlink()
print("wrote", target, "| model:", M)
OPENCODE
```

```bash
OPENAI_API_KEY=dummy opencode run --model praxis-local/qwen3.8:27b "reply with exactly: PRAXIS_OK"
```

> **`small_model` is not optional here.** Left unset, opencode does not fall
> back to your main model for title generation — it uses its own hosted
> infrastructure, model string `opencode/gpt-5-nano`, and that traffic **never
> reaches your gateway at all**. The upstream docs make the same point in their
> self-hosted section, where pinning `small_model` to your own provider is the
> stated way to "lock OpenCode to only use your own instance". So this is an
> egress question before it is a budget question: unpinned, some of your prompts
> leave your machine regardless of what the gateway is doing.

`agent.plan.model` and `agent.build.model` are pinned to the same model for the
same reason: they default to the global `model`, so they cost nothing to set,
and setting them makes the gateway's view match the config rather than depend on
a default.

opencode also runs three hidden agents — Compaction, Title and Summary. Title's
behaviour is the one documented above; whether the other two share it is not
documented either way.

<details>
<summary>Did it actually reach the gateway?</summary>

An agent that appears to work may just be talking to the vendor. Watch the
gateway's own counters:

```bash
# compose
curl -s http://localhost:9901/metrics | grep 'requests_total{decision='
# KIND
kubectl exec deploy/praxis-proxy -n default -- \
  wget -qO- http://127.0.0.1:9901/metrics | grep 'requests_total{decision='
```

`admitted` climbing means praxis served it, `denied` climbing means praxis
rejected it on budget, and neither moving means the agent never arrived.

</details>
