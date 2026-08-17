# Alpha M5a — bounded playlist core

M5a introduces the playlist data contract independently of display application:

- Playlist IDs and titles are bounded.
- Entries are deduplicated and capped at 1,024 wallpapers.
- Ordered repeat/no-repeat selection is deterministic.
- Shuffle selection uses a deterministic seed and remains bounded.

Persistence, transitions, policies, and per-output assignment will build on this
contract in later M5 slices.
