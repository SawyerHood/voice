# SWARM.md

Repository operating policy (applies to all swarm agents):

1. All development work must be done in git worktrees located under `~/worktrees`.
2. The manager must route implementation tasks to a dedicated **merger agent**.
3. The merger agent is solely responsible for merging completed changes into `main`.

Additional guidance:
- Non-merger agents should not merge branches into `main`.
- If a task is not already in a `~/worktrees` worktree, create/use one before making code changes.
- This is a voice-to-text desktop app project (like WhisperFlow / SuperWhisper / Monologue).
- Main repo lives at `~/voice`.
