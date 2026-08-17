# Alpha M2b — gallery usability

M2b adds the first persistent library preferences to the manager:

- Favorites are stored with `QSettings` and survive catalog refreshes.
- Cards expose an accessible favorite toggle and retain keyboard navigation.
- The gallery can filter to favorites and sort deterministically by title or
  wallpaper type.
- Existing search, compatibility badges, Workshop state, and safe preview
  behavior remain unchanged.

Favorites are keyed by Workshop ID, so metadata refreshes do not lose a user's
selection and unknown catalog fields remain untouched.
