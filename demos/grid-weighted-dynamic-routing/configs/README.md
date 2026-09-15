# Configuration materialization

The files in this directory are the checked-in Praxis configuration templates
used by the Grid llm-d pool-metrics Forge topology. Provider endpoint captures,
cluster names, certificates, and other run-scoped values are materialized by
the Grid xtask/Forge run; credentials and generated certificates are never
stored here.

Run the demo through `run.sh` with `GRID_REPO` pointing at the pinned Grid
checkout. Do not apply these templates directly to a cluster without the
Forge substitutions and generated Secret material.
