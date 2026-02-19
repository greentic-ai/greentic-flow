# Deployment Flows (Events-Based)

Greentic treats deployment automation as a first-class flow pattern without
creating a new flow kind. Deployment flows are simply `type: events` graphs that
operate on a host-provided `DeploymentPlan` and typically render
infrastructure-as-code (IaC) artifacts.

## Characteristics

- **Flow type**: always `events`. There is no special FlowKind for deployment.
- **Ingress**: same implicit ingress rule as other flows — the first node
  receives the `EventEnvelope`.
- **Plan access**: components import the `greentic:deploy-plan@1.0.0` world to
  fetch the current `DeploymentPlan` as JSON and optionally emit status updates.
- **Components**: any component can implement deployment behaviour by declaring
  the appropriate host capabilities (notably `host.iac`) and importing the
  deploy-plan world.
- **Connectors**: packs map deployment flows the same way they map regular event
  flows (for example, triggered by CI/CD or manual actions) — no new connector
  concept is required.

## Scaffolding Helpers

`greentic-flow` exposes a CLI scaffolder to speed up authoring:

```bash
greentic-flow new --flow flows/deploy_stack.ygtc --id deploy-stack --type events
```

This command:

1. Writes a minimal YGTc v2 flow document.
2. Marks the flow as `type: events`.
3. Lets you add deployment-oriented nodes using `add-step`/`update-step` while
   keeping routing and sidecar bindings consistent.

## Authoring Guidelines

- Keep node kinds/ids opaque: `deploy.renderer`, `plan.emit`, and similar names
  are just strings.
- Reference deployment components by their component ID and profile like any
  other node.
- Let components handle provider specifics: the flow only orchestrates logic and
  routing.
- Secrets are not declared in flows; packs aggregate `secret_requirements` and
  should be satisfied via `greentic-secrets init --pack <pack>`.
- Capture IaC output paths via component config or pack metadata; the flow itself
  does not embed filesystem layout.

By keeping deployment flows indistinguishable from other events flows, authoring
stays approachable for small LLMs and humans alike, while runtimes retain the
flexibility to route plans through any deployment strategy.
