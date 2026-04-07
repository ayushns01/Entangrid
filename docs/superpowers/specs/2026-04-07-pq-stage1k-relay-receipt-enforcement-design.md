# PQ Stage 1K Relay Receipt Hybrid Enforcement Design

## Goal

Extend the existing `require_hybrid_validator_signatures` policy so relay receipts join blocks and proposal votes under strict hybrid validator-signature enforcement.

## Scope

Stage 1K stays intentionally narrow:

- enforce hybrid signatures on locally emitted `RelayReceipt` objects when strict hybrid validator enforcement is enabled
- reject imported `RelayReceipt` objects whose signatures are not hybrid when strict hybrid validator enforcement is enabled
- keep current permissive receipt behavior when strict hybrid validator enforcement is disabled

Out of scope:

- service attestations
- service aggregates
- transactions
- new policy flags
- new crypto backends

## Architecture

Stage 1K should keep the policy boundary where it already exists for blocks and proposal votes: the node/runtime layer.

Recommended shape:

- `crates/entangrid-node`
  - owns local receipt emission
  - owns imported receipt validation
  - applies the strict hybrid policy when `require_hybrid_validator_signatures = true`
- `crates/entangrid-consensus`
  - remains crypto-agnostic
  - continues validating receipt semantics such as assignment correctness and witness/source/destination consistency
- `crates/entangrid-types`
  - needs no new config or policy flag for this slice

This keeps the rule simple:

- strict hybrid enforcement on:
  - blocks must be hybrid
  - proposal votes must be hybrid
  - relay receipts must now also be hybrid
- strict hybrid enforcement off:
  - receipt validation remains permissive with respect to signature form

## Enforcement Rules

With `require_hybrid_validator_signatures = true`:

- locally emitted `RelayReceipt` values must carry hybrid signatures
- imported `RelayReceipt` values must be rejected unless their signatures are hybrid
- receipt semantic validation still runs exactly as today
- hybrid enforcement is an additional policy gate, not a replacement for existing receipt checks

With `require_hybrid_validator_signatures = false`:

- non-hybrid relay receipts continue to be accepted
- locally emitted receipts follow whatever backend the node is using without an additional strict policy requirement

## Local Emission Semantics

Relay receipt emission should stay explicit rather than relying only on backend choice.

Recommended behavior:

- the receipt-signing path continues to sign the existing `receipt_signing_hash(...)`
- after signing, if strict hybrid enforcement is enabled, the node validates that the resulting receipt signature is hybrid before broadcasting or persisting the receipt
- if the node is misconfigured under strict hybrid enforcement and emits a non-hybrid receipt, the node should fail the operation instead of gossiping a policy-invalid receipt

This keeps local emission and remote acceptance aligned.

## Imported Receipt Validation

Receipt import should extend the existing validation path in `crates/entangrid-node`.

Recommended behavior:

- receipt semantic checks continue as they do now
- receipt signature verification continues as it does now
- when strict hybrid enforcement is enabled, a non-hybrid receipt signature is rejected before the receipt is stored or rebroadcast

This mirrors the existing Stage 1E behavior for blocks and proposal votes.

## Verification

Stage 1K should prove:

1. strict hybrid enforcement rejects non-hybrid relay receipts
2. strict hybrid enforcement accepts hybrid relay receipts
3. non-hybrid relay receipts are still accepted when strict hybrid enforcement is disabled
4. locally emitted receipts are hybrid under strict mode

Recommended tests:

- node tests for non-hybrid receipt rejection under strict mode
- node tests for hybrid receipt acceptance under strict mode
- node tests for permissive receipt acceptance when strict mode is disabled
- node tests proving local receipt creation emits hybrid signatures under strict mode

## Success Criteria

Entangrid extends the current hybrid validator-signature policy so relay receipts are enforced under the same strict mode as blocks and proposal votes, without adding new flags or changing consensus semantics.
