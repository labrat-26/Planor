# Delegated Task Brief Template (9.4)

Use this template whenever delegating implementation or verification work to a subagent (or drafting a unit of work).

```markdown
## Task: <short imperative title>

### Model & Escalation Policy
- Start with the cheapest capable model tier (`flash_lite` / `flash` / Haiku for mechanical edits, log digestion, or suspect image review; `inherit` / Sonnet for standard implementation).
- Escalate up one tier after **two failed attempts** on the same issue; never brute-force a third identical retry.

### Owned Files (only edit these)
- `<path/to/file_or_module>`

### Read-Only Context
- `CLAUDE.md`
- `docs/ARCHITECTURE.md`
- `docs/TOOLING_PLAN.md` (relevant section)

### Goal & Design Constraints
- <concise description of what must work and why>
- PowerShell scripts must run cleanly under Windows PowerShell 5.1 (no PS 7 operators).
- No `println!` in daemon/render/win32 code; use `diag::trace`.
- Never weaken or delete test assertions to make a test pass.

### Acceptance Criteria
1. <observable command, test, or output requirement>
2. <performance/budget or schema requirement if applicable>

### Progressive Verification Commands
1. Unit check (before & after):
   `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Tier unit`
2. Targeted fast/smoke check (after unit passes):
   `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Only <test_name>`
3. Digest results on failure (never dump raw logs):
   `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\digest.ps1`
   `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\digest.ps1 -Log <path>`

### Required Output Format
- Commit hash / `git diff --stat`
- Exact commands run and observed pass/fail/flake counts + durations
- **What was NOT verified** (explicit list of skipped tiers, hardware paths, or topologies)
```
