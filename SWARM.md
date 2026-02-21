# SWARM.md

Repository operating policy (applies to all swarm agents):

1. All development work must be done in git worktrees located under `~/worktrees`.
2. The manager must route implementation tasks to a dedicated **merger agent**.
3. The merger agent is solely responsible for merging completed changes into `main`.

## Quality gates (every worker must follow)

- **Build must pass** before committing: run `pnpm build` (frontend) and `cargo build` (Rust) and fix any errors.
- **Write tests** for the code you produce. Unit tests for logic, integration tests where appropriate.
- **Tests must pass** before committing: run `cargo test` (Rust) and any frontend test runner.
- **Validate manually** where possible: describe in your report what you tested and how (e.g., "ran `cargo tauri dev`, pressed hotkey, confirmed recording started").
- If a test or build fails, fix it before reporting done. Don't leave broken branches.

## Additional guidance
- Non-merger agents should not merge branches into `main`.
- If a task is not already in a `~/worktrees` worktree, create/use one before making code changes.
- This is a voice-to-text desktop app project (like WhisperFlow / SuperWhisper / Monologue).
- Main repo lives at `~/voice`.
