# Alpha M1j — bounded Workshop download progress

M1j extends local Workshop state tracking with optional progress metadata. When
Steam exposes bounded downloaded and total byte fields (or an explicit
percentage), the catalog reports `workshop_progress` from 0 to 100 and uses the
`downloading` state until complete.

Unknown or malformed progress fields are ignored. No progress value is trusted
for filesystem access, download control, or completion beyond the local
display label. Steam remains the download authority.
