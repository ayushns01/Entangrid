# PQ Stage 1K Service Aggregate Hybrid Enforcement Closeout Design

## Goal

Close Stage 1K by proving that service aggregates inherit strict hybrid validator-signature enforcement transitively through their embedded service attestations.

## Scope

This closeout slice stays intentionally narrow:

- add focused aggregate tests showing strict hybrid enforcement rejects aggregates with non-hybrid embedded attestations
- add focused aggregate tests showing strict hybrid enforcement accepts aggregates whose embedded attestations are hybrid
- add focused aggregate tests showing permissive mode still accepts deterministic aggregates
- add a focused local-build test proving strict-mode aggregates are composed only from hybrid attestations

Out of scope:

- adding a new aggregate signature field
- adding a new aggregate-specific hybrid policy flag
- widening enforcement to transactions
- changing consensus aggregate semantics
- introducing new crypto backends

## Architecture

`ServiceAggregate` is not a signed object. It is an unsigned container whose trust comes from the embedded `ServiceAttestation` set.

Recommended shape:

- `crates/entangrid-node`
  - keeps aggregate validation where it already is
  - relies on `validate_service_aggregate(...)` calling `validate_service_attestation(...)` for every embedded attestation
  - adds tests proving the strict hybrid policy flows through that existing validation path
- `crates/entangrid-consensus`
  - remains crypto-agnostic
  - continues validating aggregate well-formedness, committee membership, and threshold semantics only
- `crates/entangrid-types`
  - needs no new config or policy surface

This keeps the model honest:

- direct strict enforcement applies to signed validator-originated objects
- aggregate enforcement is transitive because aggregates are only valid when their embedded signed attestations are valid

## Enforcement Rules

With `require_hybrid_validator_signatures = true`:

- imported `ServiceAggregate` values must be rejected if any embedded `ServiceAttestation` fails strict hybrid validation
- locally built `ServiceAggregate` values should only contain attestations that already satisfied strict hybrid validation on import or local creation

With `require_hybrid_validator_signatures = false`:

- deterministic aggregates remain accepted as long as the embedded attestations are otherwise valid

## Local Aggregate Semantics

Local aggregate construction should not gain a separate hybrid policy helper.

Recommended behavior:

- `build_service_aggregate(...)` continues to collect the canonical stored attestations for `(subject_validator_id, epoch)`
- because strict mode already prevents non-hybrid service attestations from entering local state, strict-mode locally built aggregates automatically contain only hybrid attestations
- the closeout work should prove that property with tests rather than duplicate the rule in a second place

## Imported Aggregate Validation

Imported aggregate validation should continue through the existing path:

- `validate_service_aggregate(...)` checks aggregate well-formedness through consensus
- then validates each embedded service attestation through `validate_service_attestation(...)`
- strict hybrid enforcement therefore applies transitively to aggregates through the attestation validator

This mirrors the current architecture rather than layering redundant policy checks on top.

## Verification

This closeout slice should prove:

1. strict hybrid enforcement rejects aggregates with non-hybrid embedded attestations
2. strict hybrid enforcement accepts aggregates with hybrid embedded attestations
3. permissive mode still accepts deterministic aggregates
4. strict-mode locally built aggregates contain only hybrid attestations

Recommended tests:

- node tests for aggregate rejection under strict mode when one or more embedded attestations are deterministic
- node tests for aggregate acceptance under strict mode when all embedded attestations are hybrid
- node tests for permissive aggregate acceptance when enforcement is disabled
- node tests for local aggregate construction in strict mode using previously imported or emitted hybrid attestations

## Success Criteria

Stage 1K is complete when blocks, proposal votes, relay receipts, and service attestations are directly hybrid-enforced, and service aggregates are proven to inherit that enforcement transitively through embedded attestation validation, without adding redundant aggregate-specific policy code.
