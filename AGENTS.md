# AGENTS.md

Project-specific standing instructions for agents working in this repo (complements `CLAUDE.md`).

## Commits

- Do not amend or rewrite existing commits (e.g. `git commit --amend`, rebases) unless explicitly asked — when a fix is needed, make a new commit instead.
- Every agent-made commit must carry a `Co-Authored-By:` trailer naming the agent and the model, in the same style as the Claude trailers already in history. For opencode sessions:

  ```
  Co-Authored-By: opencode[Qwen3.8-27B-FP8]
  ```

  (Swap in the actual model name for other models/sessions.)
