# PQ Stage 1K Service Attestation Hybrid Enforcement Design

## Goal

Extend the existing `require_hybrid_validator_signatures` policy so service attestations join blocks, proposal votes, and relay receipts under strict hybrid validator-signature enforcement.

## Scope

Stage 1K stays intentionally narrow for this slice:

- enforce hybrid signatures on locally emitted `ServiceAttestation` objects when strict hybrid validator enforcement is enabled
- reject imported `ServiceAttestation` objects whose signatures are not hybrid when strict hybrid validator enforcement is enabled
- keep current permissive attestation behavior when strict hybrid validator enforcement is disabled

Out of scope:

- service aggregates
- transactions
- new policy flags
- new crypto backends
- consensus semantic changes

## Architecture

This slice should keep the policy boundary where it already exists for blocks, proposal votes, and relay receipts: the node/runtime layer.

Recommended shape:

- `crates/entangrid-node`
  - owns local service-attestation emission
  - owns imported service-attestation validation
  - applies the strict hybrid policy when `require_hybrid_validator_signatures = true`
- `crates/entangrid-consensus`
  - remains crypto-agnostic
  - continues validating committee membership and counter semantics only
- `crates/entangrid-types`
  - needs no new config or policy flag for this slice

This keeps the rule simple:

- strict hybrid enforcement on:
  - blocks must be hybrid
  - proposal votes must be hybrid
  - relay receipts must be hybrid
  - service attestations must now also be hybrid
- strict hybrid enforcement off:
  - attestation validation remains permissive with respect to signature form

## Enforcement Rules

With `require_hybrid_validator_signatures = true`:

- locally emitted `ServiceAttestation` values must carry hybrid signatures
- imported `ServiceAttestation` values must be rejected unless their signatures are hybrid
- committee membership, counter integrity, and signature verification still run exactly as today
- hybrid enforcement is an additional policy gate, not a replacement for existing attestation checks

With `require_hybrid_validator_signatures = false`:

- non-hybrid service attestations continue to be accepted
- locally emitted attestations follow whatever backend the node is using without an additional strict policy requirement

## Local Emission Semantics

Service-attestation emission should stay explicit rather than relying only on backend choice.

Recommended behavior:

- the attestation-signing path continues to sign the existing `service_attestation_signing_hash(...)`
- after signing, if strict hybrid enforcement is enabled, the node validates that the resulting attestation signature is hybrid before returning, broadcasting, or storing the attestation
- if the node is misconfigured under strict hybrid enforcement and emits a non-hybrid attestation, the node should fail the operation instead of propagating a policy-invalid attestation

This keeps local emission and remote acceptance aligned.

## Imported Attestation Validation

Attestation import should extend the existing validation path in `crates/entangrid-node`.

Recommended behavior:

- committee membership checks continue as they do now
- attestation signature verification continues as it does now
- when strict hybrid enforcement is enabled, a non-hybrid attestation signature is rejected before the attestation is stored or rebroadcast

This mirrors the existing Stage 1E behavior for blocks and proposal votes and the Stage 1K relay-receipt behavior.

## Verification

This slice should prove:

1. strict hybrid enforcement rejects non-hybrid imported service attestations
2. strict hybrid enforcement accepts hybrid service attestations
3. non-hybrid service attestations are still accepted when strict hybrid enforcement is disabled
4. locally emitted service attestations are hybrid under strict mode
5. strict mode rejects local non-hybrid attestation emission

Recommended tests:

- node tests for non-hybrid service-attestation rejection under strict mode
- node tests for hybrid service-attestation acceptance under strict mode
- node tests for permissive service-attestation acceptance when strict mode is disabled
- node tests proving local attestation creation emits hybrid signatures under strict mode
- node tests proving local deterministic attestation creation fails under strict mode

## Success Criteria

Entangrid extends the current hybrid validator-signature policy so service attestations are enforced under the same strict mode as blocks, proposal votes, and relay receipts, without adding new flags or changing consensus semantics.
