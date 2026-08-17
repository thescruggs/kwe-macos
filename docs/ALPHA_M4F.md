# Alpha M4f — permission policy

M4f adds a single policy object for future web, pointer, and audio workers.
Requested permissions are filtered to the known allowlist, and grants are
intersected with requests so a stale or malicious grant cannot enable a new
capability. No permissions are automatically granted.
