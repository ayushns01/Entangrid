# PQ Stage 1H Session Bootstrap Design

## Goal

Extend the existing strict hybrid localnet bootstrap path so simulator-generated hybrid localnets include separate ML-KEM session key material and session identity wiring, not just ML-DSA signing material.

## Scope

Stage 1H is intentionally narrow:

- extend `init-localnet --hybrid-enforcement`
- generate one separate ML-KEM session key file per node
- populate `session_public_identity` for every validator in genesis
- populate `session_backend` and `session_key_path` in every node config
- fail clearly when strict hybrid bootstrap is requested without a `pq-ml-kem` build

Out of scope:

- encrypted framing
- session rotation
- new CLI modes
- broad hybrid boot/performance matrices

## Architecture

Stage 1H should extend the existing strict all-hybrid bootstrap path instead of adding a second bootstrap mode. When the simulator is built with `pq-ml-dsa` and `pq-ml-kem`, `init-localnet --hybrid-enforcement` should generate:

- hybrid signing identities for validator-authenticated protocol objects
- separate session identities for the Stage 1G per-stream handshake
- one ML-DSA signing key file per node
- one ML-KEM session key file per node

The node config remains the operator-facing source of local private material, while genesis remains the network-facing source of public validator identity.

## Identity And Config Model

Signing and session roles stay separate.

`ValidatorConfig`:

- `public_identity` remains the signing identity
- `session_public_identity` becomes populated for strict hybrid localnets

`NodeConfig`:

- `signing_backend = HybridDeterministicMlDsaExperimental`
- `signing_key_path = "node-N/ml-dsa-key.json"`
- `session_backend = HybridDeterministicMlKemExperimental`
- `session_key_path = "node-N/ml-kem-session-key.json"`

This keeps Stage 1H aligned with Stage 1G:

- signing identity authenticates objects and handshake transcripts
- session identity provides the transport/KEM public material

## Generated File Layout

Recommended generated output per node:

- `node-N/node.toml`
- `node-N/ml-dsa-key.json`
- `node-N/ml-kem-session-key.json`

The simulator should not bundle both roles into one file. Separate files make future rotation, validation, and operator reasoning much cleaner.

## Feature-Gated Behavior

Strict hybrid bootstrap must not silently generate partial configuration.

When `--hybrid-enforcement` is used:

- if the simulator build lacks `pq-ml-dsa`, startup already fails
- if the simulator build lacks `pq-ml-kem`, Stage 1H should now also fail clearly

The simulator should emit an actionable error that the strict hybrid session bootstrap path requires a build with `pq-ml-kem`.

## Verification

Stage 1H should prove:

1. `init-localnet --hybrid-enforcement` under `pq-ml-kem` generates:
   - `session_public_identity` for each validator
   - `session_backend = HybridDeterministicMlKemExperimental`
   - `session_key_path` for each node
   - one `ml-kem-session-key.json` per node
2. the same command without `pq-ml-kem` fails clearly instead of generating broken config

Recommended tests:

- simulator tests for generated node config contents
- simulator tests for generated genesis session identities
- simulator tests for generated session key files
- simulator tests for feature-mismatch failure behavior

## Success Criteria

Entangrid can automatically generate a strict hybrid localnet where both:

- validator signing material
- validator session/KEM material

are correctly produced and wired into genesis plus node configs, with clear feature-gated behavior.
