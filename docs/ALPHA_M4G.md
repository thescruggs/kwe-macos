# Alpha M4g — persistent permission controls

M4g adds explicit per-wallpaper permission controls for the three allowlisted
capabilities. Grants are stored in the user's Qt settings and can be revoked
from the detail pane. The controls are declarative only until the corresponding
sandboxed web, pointer, and PipeWire workers are enabled.
