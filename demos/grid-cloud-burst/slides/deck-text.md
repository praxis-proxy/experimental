# Praxis Grid Intelligent Overflow Routing

## Slide 1 — Keep the request path local

### Control plane

- Observe provider health and capacity
- Consume Kubernetes inference queue and capacity metrics
- Apply admission, locality, grouping, and placement policy
- Publish a versioned routing snapshot

### Fast request path

- Authenticate
- Apply token policy
- Read the local Grid snapshot
- Preserve valid affinity
- Select locally from the active provider group
- Forward to the selected provider

**No request-time call to Grid, Kubernetes, metrics, or inference schedulers.**

## Slide 2 — Reactive burst: rebalance locally before using external capacity

- Local provider pools are preferred
- Grid first evaluates aggregate local headroom
- One pressured local pool does not immediately send all traffic externally
- Available local pools absorb traffic first
- Only residual demand leaves the preferred tier

| State | Local | External |
| --- | ---: | ---: |
| Healthy | 100% | 0% |
| Moderate pressure | 90% | 10% |
| Higher pressure | 70% | 30% |
| No new local capacity | 0% | 100% |
| Recovery | ramps back | ramps down |

## Slide 3 — Independent policies: burst amount and destination

1. **Admission** — Can this provider receive traffic?
2. **Grouping** — Which providers actively compete together?
3. **Local placement** — How is retained local traffic distributed?
4. **Burst policy** — How much traffic leaves the preferred tier?
5. **Overflow policy** — Where does the burst traffic go?

Example:

```text
Burst policy: 30% external

External distribution:
  Provider A  50%
  Provider B  25%
  Provider C  15%
  Provider D  10%

Result:
  70% local
  15% Provider A
   7.5% Provider B
   4.5% Provider C
   3% Provider D
```

**Grid computes policy → the request gateway executes final weights locally.**

## Slide 4 — Soft token limits and burst routing

```text
Request
  ↓
Authenticate user or workload
  ↓
Shared token ledger
  ↓
Soft allocation status
  ↓
Local or external provider
  ↓
Actual usage reconciled
```

### Soft allocation example

- Allocation: 10,000 tokens
- Usage: 8,500 — within allocation
- Usage: 10,500 — over allocation, recorded
- Request may continue under soft governance

### Combined proof

- Shared token usage across consumer gateways
- One distributed quota can span sites, regions, and Kubernetes clusters
- Usage survives provider changes
- Local queue pressure changes routing
- External burst activates only when required
- Recovery returns traffic locally
- Token policy changes do not reset routing
- Routing changes do not reset token usage
