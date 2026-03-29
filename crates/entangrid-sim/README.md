# entangrid-sim

## What this crate does

This crate is the local lab for Entangrid.

It helps you:

- generate a multi-node local network
- launch validator processes
- inject traffic into that network
- summarize the latest localnet metrics
- simulate degraded validator behavior

It is called a simulator, but an important detail is:

it does **not** fake the chain in memory.

Instead, it launches real `entangrid-node` processes and manages them.

## How it currently works

### `init-localnet`

This command creates a fresh localnet directory.

It writes:

- `genesis.toml`
- `localnet-manifest.toml`
- one `node.toml` per validator
- one `node-N/` directory per validator
- `inbox/` and `processed/` folders for each node

It also:

- assigns validator ids
- assigns localhost ports
- creates validator dev secrets
- seeds initial balances
- configures all peers statically
- can enable service gating
- can set the epoch where service gating should start
- can set the service-gating threshold
- can set how many epochs are included in the rolling service-score window
- can set the service-score weight profile, including how strongly penalties affect the final score
- can mark one validator as degraded

The current recommended prototype gating profile emitted by default is:

- `service_gating_start_epoch = 3`
- `service_gating_threshold = 0.40`
- `service_score_window_epochs = 4`
- weights `[0.25 uptime, 0.50 delivery, 0.25 diversity, 1.00 penalty]`

That default is still the best current 4-validator baseline, but the live matrix now keeps healthy and degraded larger-validator cases in the suite because the remaining pre-PQ problem has shifted from basic structural reconvergence to restart-time recovery and stale-node catch-up behavior.

Current branch focus:

- `main` is now the active V2-focused line
- `codex/consensus-v1` is kept as the benchmark/control line
- `consensus_v2` is still opt-in through config so the simulator can compare baseline and V2 behavior on the same codebase

### `up`

This command reads the manifest and launches one OS process per node.

For each node it captures:

- `stdout.log`
- `stderr.log`

The nodes then run until you stop the simulator with `Ctrl+C`.

Recent improvement:

- `up` now rebuilds `entangrid-node` before launch so it does not accidentally use a stale binary from an older compile
- if the localnet is still fresh and its genesis time has already passed, `up` moves genesis slightly forward before launch so the network starts near epoch 0 instead of skipping straight past the intended warmup period
- `up` now interrupts all nodes together before falling back to hard kills, so shutdown does not artificially leave later validators a little farther ahead than earlier ones

### `load`

This command creates traffic.

Current scenarios:

- `idle`
- `steady`
- `bursty`
- `large-block`

The simulator:

- reads genesis
- uses the deterministic crypto backend
- creates signed transfer transactions
- picks recipients deterministically from the validator set
- writes those transactions into the target node's `inbox/`

Then the node process reads and handles them.

### `report`

This command reads each node's `metrics.json` and prints a quick localnet summary.

It is useful for quickly checking:

- current validator scores
- missed proposer slots
- gating rejections
- duplicate receipts that were ignored
- peer rate-limit drops and inbound session drops from the latest run
- failed-session and invalid-receipt penalties from the latest local score window
- high-level proposer and validation activity

### `matrix`

This command runs a built-in rigorous scenario suite and writes reports to disk.

Current built-in scenarios include:

- 4-validator steady baseline
- 6-validator bursty baseline
- 6-validator bursty run with service gating enabled but no degraded validator
- gated `85%` drop on one validator
- gated `95%` drop on one validator
- gated outbound-disabled validator
- threshold sweep with a lower gating threshold
- threshold sweep with a stricter gating threshold
- score-window sweep with a short window
- score-window sweep with a long window
- penalty-weight sweep with a lighter penalty profile
- penalty-weight sweep with a harsher penalty profile
- sync-control flood against one validator
- inbound connection flood against one validator

For each scenario it:

- creates a fresh localnet directory
- launches the validators
- runs the configured load
- waits for convergence during a bounded settle window
- captures the structural/localnet report at that converged moment
- checks scenario-specific expectations such as "the degraded validator had the lowest score", "that validator was actually gated" in the harshest cases, and "baseline validators stayed above a minimum score floor"
- checks policy-side expectations such as "how many non-target validators fell below threshold" and "how much honest gating fallout happened" under threshold/window sweeps
- checks abuse-control expectations such as "peer rate limits were actually triggered" and "inbound session caps were actually exercised" in the dedicated adversarial cases
- shuts the nodes down
- verifies structural chain health
- writes Markdown and JSON summaries under the chosen output directory

Recent improvement:

- matrix shutdown now interrupts all validators together before a hard kill, which makes the generated reports much less likely to trip over truncated final JSONL appends or shutdown-induced chain skew
- matrix/report JSONL readers now ignore only a truncated trailing line, so interrupted runs are easier to inspect without hiding mid-file corruption
- localnet and matrix summaries now expose the penalty counters behind each validator's current score, which makes threshold and score-window tuning much easier to compare across runs
- localnet and matrix summaries now also expose the active score-weight profile, which makes penalty-weight tuning comparable from one run to the next
- localnet and matrix summaries now also expose peer-rate-limit drops and inbound-session drops, which makes abuse-control tuning visible without digging into raw metrics files
- the matrix can now generate protocol-level abuse traffic itself, so we can regression-test the new per-peer rate limits and inbound listener caps without needing manual socket scripts
- the matrix now also exposes non-target below-threshold counts and non-target gating rejections, so threshold/window sweeps tell us whether a policy is only punishing the degraded validator or harming honest ones too
- the matrix now also keeps larger-validator healthy and degraded bursty cases in the suite, which helps us catch both reconvergence regressions and the newer stale-restart recovery edge cases instead of only proving the 4-validator path

### Fault and degradation controls

The simulator can configure a degraded validator using:

- added delay
- outbound drop probability
- outbound disablement

That is how we currently test service scoring and proposer gating behavior.

Recent improvement:

- `init-localnet` now accepts `--service-gating-start-epoch`
- `init-localnet` now accepts `--service-gating-threshold`
- `init-localnet` now accepts `--service-score-window-epochs`
- `init-localnet` now accepts score-weight flags like `--service-score-penalty-weight`
- `matrix` now gives us a repeatable test harness for the harsh baseline and degraded-validator cases we were previously running by hand
- this makes it easier to keep the first few epochs as warmup before proposer gating is enforced
- this also makes it possible to tune how strict gating should be without recompiling the node
- this also makes it easier to trade off score stability, responsiveness, and penalty harshness in local experiments
- the latest matrix review currently keeps the shared defaults at that same profile, so fresh localnet experiments start from the same policy we are using as the pre-PQ baseline
- branch-comparison helpers now also exist so the simulator can mark `v1 degraded/4` and `v1 degraded/5` as benchmark cases when comparing V2 against the older line

## What it is today

Today this crate is a local orchestration and workload tool.

It is already very useful because it gives us:

- repeatable local experiments
- easy multi-validator startup
- transaction generation without an RPC layer
- degraded-node testing

It is not yet a full benchmarking harness or a full network simulator.

## Why this crate matters

Without this crate, testing the chain would be slow and manual.

This crate is what lets us quickly answer questions like:

- does the network start correctly?
- are blocks being produced?
- do receipts appear?
- does service gating visibly reject a degraded proposer?
- do larger healthy gated localnets still reconverge after bursty traffic?

## Where we want to take it

This crate should grow into a much stronger experimentation tool.

Future direction:

- add richer traffic patterns and scenarios
- add better automated verification after runs
- expand fault injection beyond simple drop/delay
- emit clearer benchmark summaries
- support more realistic network topologies
- help compare protocol changes across repeatable local experiments

In short: today this crate is the easiest way to run and stress the project locally, and later it should become the main experimentation harness for Entangrid.
