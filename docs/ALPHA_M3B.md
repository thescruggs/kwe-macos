# Alpha M3b — static scene preflight

M3b adds a renderer-independent preflight boundary:

- `kwe preflight --path <scene>` validates regular non-symlink files.
- Scene size is bounded at 512 MiB, with a separate 16 MiB JSON parsing limit.
- Only `scene.json` and `scene.pkg` entry formats are accepted.
- JSON scenes must have an object root; failures return structured reasons and
  a non-zero exit status.

No renderer process is launched by preflight. Future supervisor launches must
  require a successful report and can use its reasons as quarantine metadata.
