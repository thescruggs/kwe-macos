# Alpha M5j — time/day policy windows

M5j extends the pure M5i policy resolver with bounded local time/day windows.
Days use a Monday-first seven-bit mask and times use start-inclusive,
end-exclusive minutes from midnight. Cross-midnight rules correctly associate
the early-morning portion with the selected previous start day.

The caller must provide weekday and minute together. Missing clock data leaves
a time rule inactive; partial or out-of-range snapshots fail closed. Equal
start/end times and empty/invalid day masks are rejected rather than being
silently interpreted as an always-active rule.

The core does not read the system clock or perform timezone/DST conversion.
Those responsibilities remain with a future versioned daemon adapter. Tests
advance `playlist.rules` without changing a renderer or the Plasma session.
