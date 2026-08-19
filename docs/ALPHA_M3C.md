# Alpha M3c — preflight-gated supervisor launches

M3c connects static scene validation to the renderer supervisor:

- `renderer.start` and `renderer.retry` accept an optional `scene_path`.
- When supplied, the supervisor runs the bounded core preflight before spawn.
- Unsafe, missing, oversized, symlinked, or unsupported scenes are rejected
  without launching a worker.
- Existing persisted failure records still provide bounded quarantine after
  runtime failures; a manual retry remains the explicit recovery action.

The scene path is optional for existing synthetic/test renderers, preserving the
current protocol compatibility while making real scene launches preflight-gated.

*(BETA_M1a: `scene_path` was replaced by `kind`/`content`. `renderer.start`
and `renderer.retry` now take `kind: "scene"` plus a `content` path; the
versioned API rejects the removed `scene_path` field outright
(`deny_unknown_fields`), so old M3c-era clients must update their request
shape, not just their payload.)*
