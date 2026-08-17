# Alpha M3a — renderer capability manifest

M3a makes renderer support explicit before scene execution begins. The Vulkan
probe now emits a stable capability manifest alongside device and extension
information:

- Backend identity and version are reported.
- Scene2D, Scene3D, shader, particle, pointer, and audio capabilities are
  individually declared.
- The current probe intentionally reports those execution capabilities as
  unsupported because it does not render Wallpaper Engine scenes yet.
- Device probing still verifies graphics queues, logical-device creation, and
  external-memory/semaphore support on each adapter.

This prevents the manager or future supervisor from treating a successful
Vulkan device probe as blanket Wallpaper Engine compatibility.
