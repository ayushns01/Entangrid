# Entangrid Policy Review 2

This review summarizes the current consensus/gating stabilization result from:

- [rigorous-matrix-1774286789875.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1774286789875.md)
- [rigorous-matrix-1774286789875.json](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1774286789875.json)

## Result

The current prototype policy should stay at:

- `service_gating_start_epoch = 3`
- `service_gating_threshold = 0.40`
- `service_score_window_epochs = 4`
- `service_score_weights = [0.25 uptime, 0.50 delivery, 0.25 diversity, 1.00 penalty]`

This is the best current pre-PQ baseline.

## Why These Defaults

### Gating start epoch

Use `3`.

Reason:

- the gated and policy scenarios were already stabilized around a 3-epoch warmup
- this gives the network enough time to accumulate early receipt history before proposer gating turns on
- it also matches the localnet examples and the matrix cases that are currently passing consistently

### Threshold

Keep `0.40`.

Reason:

- `0.25` is viable, but it has not shown a clear advantage over the current midpoint
- `0.55` is also viable and still safe in the current matrix
  - degraded validator had `4` gating rejections
  - honest fallout stayed at `0`
- `0.40` remains the middle ground
  - harsh degraded runs still gated the target validator repeatedly
  - honest fallout stayed at `0`

### Score window

Keep `4`.

Reason:

- `1` epoch was the most reactive
  - degraded validator fell to `0.000`
  - but it also only produced `1` gating rejection in the latest confirmatory pass
- `8` epochs was smoother
  - degraded validator still got gated
  - but degradation is reflected more slowly, with a final lowest score of `0.330`
- `4` epochs remains the best compromise between responsiveness and stability

### Score weights

Keep the current profile:

- uptime `0.25`
- delivery `0.50`
- diversity `0.25`
- penalty `1.00`

Reason:

- the delivery-heavy weighting still gives the degraded validator clear separation in the harsh cases
- lighter penalty `0.50` still gated the target validator, but did not provide a cleaner separation than the current midpoint
- harsher penalty `1.50` also gated the target validator more aggressively, but that is not yet enough evidence to adopt a stricter default
- `1.00` remains the clearest neutral default while more weight sweeps are still pending

## Evidence Snapshot

Selected matrix outcomes:

- `gated-drop95`
  - lowest `v3=0.000`
  - gating rejections `2`
  - honest below threshold `0`
- `gated-outbound-disabled`
  - lowest `v3=0.000`
  - gating rejections `2`
  - honest below threshold `0`
- `policy-threshold-025`
  - lowest `v3=0.225`
  - gating rejections `3`
- `policy-threshold-055`
  - lowest `v3=0.300`
  - gating rejections `4`
- `policy-window-1`
  - lowest `v3=0.000`
  - gating rejections `1`
- `policy-window-8`
  - lowest `v3=0.330`
  - gating rejections `4`
- `policy-penalty-050`
  - lowest `v3=0.392`
  - gating rejections `2`
- `policy-penalty-150`
  - lowest `v3=0.000`
  - gating rejections `3`

Across the reviewed matrix run:

- all `13/13` scenarios passed
- honest below-threshold fallout stayed at `0`
- honest gating fallout stayed at `0`
- abuse-control scenarios still passed alongside the policy sweeps

## Conclusion

Entangrid now has a justified prototype gating policy, not just a working mechanism.

That means the project is ready for the next milestone after one final confirmatory matrix pass on the locked defaults:

- keep the selected policy as the shared default
- treat it as the pre-PQ consensus baseline
- move next toward post-quantum crypto/session integration on top of this stabilized policy layer
