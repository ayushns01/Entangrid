# Entangrid PQ Signing Measurements

Generated at unix millis `1775072587965` with validator count `4` and iterations `32`.

| Scenario | Deterministic signature size | ML-DSA signature size | Deterministic message size | ML-DSA message size | Deterministic Median sign latency | ML-DSA Median sign latency | Deterministic Median verify latency | ML-DSA Median verify latency |
|---|---|---|---|---|---|---|---|---|
| block | 32 bytes | 3309 bytes | 204 bytes | 3483 bytes | 1333 ns | 7063583 ns | 1375 ns | 1763958 ns |
| proposal_vote | 32 bytes | 3309 bytes | 70 bytes | 3349 bytes | 1250 ns | 18939334 ns | 1333 ns | 1758125 ns |

## Measurement Notes

- Public identity and signature sizes come from the real configured signing backends.
- Block and proposal-vote message sizes are proxy serialized sizes, not a full live-network benchmark.
- Median timings are local-machine measurements and should be treated as indicative.