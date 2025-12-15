# Migration Status — Public Launch Secrets Program

## Summary
- No flow-level secrets hooks were added. YGTC flow schema/IR/bundles remain unchanged.
- Secrets requirements are sourced from pack/component metadata and handled by `greentic-secrets` tooling.
- Documentation now points users to `greentic-secrets init --pack <pack>` for secrets setup.
- Dependencies updated to `greentic-types` 0.4.18 / `greentic-telemetry` 0.4.1; enabled the `schema` feature on `greentic-types` so telemetry-autoinit builds against the new telemetry stack.

## What broke
- Nothing observed; no functional changes to flow parsing or schema.

## Next repos to update
- Upstream packs/components must emit `secret_requirements` in their metadata (handled in other repos).
- Runner/deployer/dev tooling will preflight and remediate via `greentic-secrets` (handled in other repos).
