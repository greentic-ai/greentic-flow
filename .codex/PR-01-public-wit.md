0) Global rule for all repos (tell Codex this every time)

Use this paragraph at the top of every prompt:

Global policy: greentic:component@0.6.0 WIT must have a single source of truth in greentic-interfaces. No other repo should define or vendor package greentic:component@0.6.0 or world component-v0-v6-v0 in its own wit/ directory. Repos may keep tiny repo-specific worlds (e.g. messaging-provider-teams) but must depend on the canonical greentic component WIT via deps/ pointing at greentic-interfaces or via a published crate path, never by copying the WIT file contents.

D) greentic-flow repo prompt (component-wizard WIT stays, but no greentic:component copies)
You are working in the greentic-flow repository.

Goal
- Keep flow-specific WIT like `greentic:component-wizard@0.6.0` as-is.
- Ensure greentic-flow does not define or copy canonical `greentic:component@0.6.0` WIT worlds.
- If any flow code needs the component world, it must depend on greentic-interfaces WIT instead.

Work
- Search for `.wit` declaring `package greentic:component@0.6.0;` in this repo.
- If found, remove/replace with canonical dependency.
- Add a guard script under `ci/` (not a Rust test) that prevents adding canonical greentic component WIT definitions to this repo.
- Ensure build-time WIT lookup uses the crate API, not a path to a WIT folder.

Guard script (required)
Create `ci/check_no_duplicate_canonical_wit.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

PATTERN='^package\\s+greentic:component@'

MATCHES="$(rg -n --hidden --glob '!.git/*' --glob '*.wit' \
  --glob '!**/target/**' \
  "$PATTERN" . || true)"

if [[ -n "$MATCHES" ]]; then
  echo "ERROR: greentic-flow must not define canonical greentic:component WIT."
  echo
  echo "$MATCHES"
  exit 1
fi

echo "OK: No canonical greentic:component WIT found."
```

Add to CI before cargo build.

Dependency strategy (validation)
- Keep `greentic-interfaces = { path = "../greentic-interfaces" }` for now.
- Do not depend directly on `../greentic-interfaces/crates/greentic-interfaces/wit`.
- Use the crate API: `let wit_root = greentic_interfaces::wit_root();`
- Do not use `PathBuf::from("../greentic-interfaces/...")`.

Flow-specific WIT worlds to preserve
- `greentic:component-wizard@0.6.0`
- `greentic:component-wizard-legacy@0.5.0`

Must not exist in greentic-flow
- Any `package greentic:component@...` WIT in this repo (excluding `target/`).
- Any `world component-v0-v6-v0` under `package greentic:component`.

Deliverables
- No canonical greentic component WIT defined in greentic-flow
- Guard script added and wired into CI
- WIT resolution via `greentic_interfaces::wit_root()`

Checklist
- `rg -n '^package greentic:component@' --glob '*.wit'` returns nothing (excluding `target/`)
- `rg -n 'world component-v0-v6-v0' --glob '*.wit'` returns nothing under `package greentic:component`
- `cargo clean`
- `cargo check`
- `cargo test`
- Run `ci/check_no_duplicate_canonical_wit.sh`

After publishing `greentic-interfaces`
- Change `greentic-interfaces = { path = "../greentic-interfaces" }` to `greentic-interfaces = "0.4"`.
- Then run:
  - `cargo clean`
  - `cargo update`
  - `cargo check`

Now implement it.
