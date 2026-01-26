# Strict Validation

`greentic-flow` is strict by default for any component input schema. Missing or empty schemas are treated as **errors** and rejected during authoring, answers generation, and validation. This prevents runtime surprises by forcing pack authors to define a meaningful `operations[].input_schema` or ask questions via `dev_flows.<op>`.

Key behaviors:

1. **Strict mode (default)**  
   If the resolved schema is missing, `{}`, or contains no constraints (e.g., `{"type":"object"}` with no properties and `additionalProperties` left at its default), commands fail with `E_SCHEMA_EMPTY`. The error message mentions the component id, operation (or dev flow name), manifest path, and guidance: “Define `operations[].input_schema` with real JSON Schema or define `dev_flows.<op>` questions/schema.”

2. **Permissive mode**  
   Pass the global flag `--permissive` (anywhere on the command line) or set `GREENTIC_FLOW_STRICT=0` to opt out. The CLI prints `W_SCHEMA_EMPTY` and continues with the previous behavior (e.g., answers return `{}` and `add-step`/`update-step` insert the node without schema validation).

3. **Environment variable override**  
   - `GREENTIC_FLOW_STRICT=1` enforces strict mode even if `--permissive` is missing.  
   - `GREENTIC_FLOW_STRICT=0` enables permissive mode unless overridden by `--permissive`.  
   - Invalid values result in startup errors.

4. **Affected commands**  
   - `answers`: requires a non-empty question graph (or schema).  
   - `add-step`/`update-step`: validate payloads against `operations[].input_schema`. Config-mode flows also require non-empty question flows.  
   - `doctor`: validates existing flows; missing schemas raise `E_SCHEMA_EMPTY` unless permissive.

Use the warnings and errors to improve component manifests and dev flows instead of deferring validation to runtime.
