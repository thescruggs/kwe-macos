# Alpha M4d — bounded audio analysis

M4d adds the dependency-free analysis core that a future PipeWire worker will
use. It accepts bounded stereo PCM windows, applies a bounded windowing pass,
and emits normalized 16/32/64-band frames. Raw samples are not part of the
renderer protocol. PipeWire capture and permission activation remain disabled
until the worker boundary is complete.
