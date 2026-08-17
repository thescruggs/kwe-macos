# Alpha M1i — automatic Workshop state refresh

M1i adds a bounded five-second manager refresh loop. The manager asks the
isolated daemon for a fresh catalog, compares the compact catalog snapshot, and
shows a dismissible notification when local Workshop state changes.

The loop does not talk to Steam directly, write subscription state, or retry
while a request is already loading. Steam remains the authority for downloads;
the notification points users to a rescan or the existing Steam fallback.
