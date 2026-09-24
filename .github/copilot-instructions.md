# GitHub Copilot repository instructions

Read and follow /AGENTS.md before making repository changes.

AGENTS.md is the canonical machine-oriented development contract. In particular:

- Rust under crates/ is the production-target core; TypeScript under src/ is reference/prototype unless an issue says otherwise.
- Use tgrep for repository-content search.
- Follow GitHub Flow; do not develop directly on main.
- Preserve bounded state, deterministic replay, provider-neutral boundaries, and non-serializable authority/security gates.
- Use committed lockfiles with --locked / --frozen-lockfile.
- Run targeted checks while iterating and the full relevant validation before a code PR.
- Read docs/agent-development.adoc for the detailed playbook.

If a tool suggestion conflicts with docs/security-threat-model.adoc, docs/domain-contract.adoc, or docs/maintenance.adoc, those repository policies take precedence.
