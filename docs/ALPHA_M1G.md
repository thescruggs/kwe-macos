# Alpha M1g — Steam Workshop fallback

M1g adds a safe native fallback for Workshop discovery: the manager can open a
selected item in the Steam client using its canonical `steam://` community-file
URL. It does not implement Steam authentication, scrape credentials, or call
the Steam Web API.

`WorkshopClient` accepts only a non-zero decimal Workshop ID of at most 20
digits. Invalid IDs are rejected before any external launch. Steam remains the
owner of subscription, download, and authentication state; the existing local
catalog rescan observes the resulting files afterward.

The fallback is intentionally separate from the renderer and Plasma package.
If Steam is closed or unavailable, the manager reports the failure and keeps
the gallery usable.
