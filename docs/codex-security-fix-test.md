# Codex Security Fix (test workflow)

This repository now includes `.github/workflows/codex-security-fix-test.yml`.

## What it does
- Manually runs on a chosen branch (`workflow_dispatch`).
- Reads open Dependabot alerts from the current repository.
- Passes those alerts to Codex CLI for automated remediation.
- Commits and pushes fixes back to the same branch.
- Uploads run artifacts (`dependabot-alerts.json`, prompt, optional report).

## Required secrets
- `OPENAI_API_KEY`: API key used by Codex CLI.

`GITHUB_TOKEN` is provided automatically by GitHub Actions.

## Safe first run
1. Create a branch, e.g. `codex-security-lab`.
2. Push it to origin.
3. Run workflow **Codex Security Fix (Test)** from Actions.
4. Set `branch=codex-security-lab`.
5. Review commit(s) produced by the workflow before opening/merging PR.

## Notes
- Workflow only targets open Dependabot alerts for the repository.
- Initial mode is branch-based test automation; PR-targeted mode can be added next.