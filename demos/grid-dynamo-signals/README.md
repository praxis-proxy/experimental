# Grid × Dynamo pluggable Frontend signals

## Video

[Watch the Grid × Dynamo signals demo][video]

Grid connects model-serving pools across sites and publishes a routing view for
the Gateway. In the video, three Dynamo Frontend + Mocker pools start ready and
idle. A request sample reaches Pool A, Pool B, and the remote pool. Direct work
then pressures Pool B and the remote pool: their Frontend active-request metrics
rise, Grid updates its scores, and new Gateway requests favor unpressured Pool
A. After the work stops and fresh metrics arrive, a final sample reaches all
three pools again. The observed shift is not an equal-share or throughput claim.

The introduction also explains Grid's operator and Gateway roles, secure peer
signal polling, and how llm-d EPP metrics differ from the Dynamo Frontend
active-request signal used in this recording. The recording does not exercise
live SWIM.

## Related source

- [Grid Dynamo integration branch][grid] — operator adapter, routing signals,
  and the two commands below.
- [Praxis Tracing dashboard on `main`][tracing] — the demo UI and presenter
  tooling.
- [Traffic Theater example on `main`][theater] — slide and recording assets.

This reproduction package separates deterministic Grid contract tests from a
real Dynamo Frontend + Mocker runtime. Neither mode, by itself, qualifies
operator overlay delivery or cross-site request routing.

## Source lock

The Grid integration branch above is pinned here at commit
`3861cce549b3989b6e603f3bcd5b2ac46152f155`. The exact source commit and
pinned runtime identities are recorded in `reproduction.yaml`.

## Modes

### Deterministic Grid-only checks

Run from the locked Grid checkout:

```bash
cargo xtask env run-grid-dynamo-pluggable-quick \
  --evidence-dir /path/to/run-owned-evidence/quick
```

This mode runs the signal contract, operator adapter/readiness policy, and
Gateway filter tests. It does not start Kind, Dynamo, or Mocker. The runner
writes a unique run directory with command logs and a machine-readable
`qualification.json`.

### Dynamo Frontend + Mocker runtime

```bash
cargo xtask env run-grid-dynamo-pluggable-full \
  --evidence-dir /path/to/run-owned-evidence/full
```

This mode starts uniquely named Kind clusters and namespaces, verifies a real
chat completion, observes Frontend pressure and recovery, captures run
identity and resource evidence, and removes only the clusters it created.
It requires Docker, Kind, network access to the pinned images and tokenizer
assets, and enough local disk and memory. The runner performs cleanup; still
inspect its evidence after any interrupted run.

The full runtime mode does not test Grid overlay distribution or live
cross-site routing. Those require separate operator, Gateway, peer-relay, and
attributed-request phases. Do not infer them from Frontend readiness or a
successful Mocker response.

## Qualification status

The live UI capture and its qualification are historical. The recording shows
run `20261108T214611Z`, associated with Grid commit
`0fc905dfae058e5c54ddebdc384e345fbfecc1a4`, source-state SHA-256
`c7bc7a45c371ea7503dda53ef39cee65700d359921f3f9dbafaed9840ff91df2`, and
the Dynamo digest in `evidence-manifest.json`. That historical result is not a
qualification of the newer source lock above. Current live qualification is
marked not run; it does not block the source or local-preview handoff.

The recording is not included in this repository; the video above is hosted as
a GitHub attachment. Synthetic Mocker responses do not measure GPU performance
or model quality. The four-request value is a comparison reference, not a queue
limit or measured serving capacity.

[video]: https://github.com/user-attachments/assets/932431ef-2f57-455e-b847-c6785fc46784
[grid]: https://github.com/nerdalert/grid/tree/feat/dynamo-pluggable-inference-signals
[tracing]: https://github.com/nerdalert/praxis-tracing/tree/main/grid-dynamo-signals
[theater]: https://github.com/nerdalert/traffic-theater/tree/main/examples/grid-dynamo-signals
