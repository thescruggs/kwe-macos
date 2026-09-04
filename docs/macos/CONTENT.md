# Getting Wallpaper Engine content onto a Mac (MP-1, gate G2)

Wallpaper Engine itself is Windows-only. On Linux, Steam installs it under
Proton and keeps subscribed Workshop items up to date; on macOS the Steam
client will neither install the app nor fetch its Workshop items. The
supported contract for this fork is therefore **bring-your-own-folder**:
the daemon indexes any Steam library layout you point it at, and how the
files get there is up to you. Nothing here downloads content.

You must own Wallpaper Engine on the Steam account you use. This project
does not redistribute Wallpaper Engine assets or Workshop items.

## What the daemon looks for

The scanner probes, in order: `$STEAM_ROOT`, then
`~/Library/Application Support/Steam`. Inside a root it reads
`steamapps/libraryfolders.vdf` and, for every library, indexes

```
<library>/steamapps/workshop/content/431960/<item id>/project.json   (Workshop items)
<library>/steamapps/common/wallpaper_engine/assets/                  (shared assets, needed by scene wallpapers)
```

`kwe scan` prints what it found. `kwe-daemon --steam-root <dir>` and
`--wallpaper-engine-assets <dir>` override discovery.

## Option A: copy from a machine that has it

Copy the two directories above from a Windows or Linux Steam library into
a folder on the Mac that mirrors the layout, for example

```
~/WallpaperEngine/steamapps/workshop/content/431960/...
~/WallpaperEngine/steamapps/common/wallpaper_engine/assets/...
```

and run the daemon with `STEAM_ROOT=~/WallpaperEngine` (the LaunchAgent
inherits it from the plist; add it to `EnvironmentVariables` or pass
`--steam-root`). A `libraryfolders.vdf` is not required for `STEAM_ROOT`
itself, only for additional libraries.

## Option B: `kwe workshop-sync` (SteamCMD, automated)

Syncs your subscribed Workshop items with SteamCMD into a
Steam-library-shaped root the daemon indexes. One-time setup:

```sh
brew install --cask steamcmd
steamcmd +login <steam account name> +quit     # interactive once: password + Steam Guard; SteamCMD caches the session
```

Then, whenever you want to sync:

```sh
kwe workshop-sync --user <steam account name> --manifest-root <where the subscriptions come from> [--assets]
```

Where the subscription list comes from (SteamCMD itself cannot list
subscriptions):

- **A Steam manifest.** `steamapps/workshop/appworkshop_431960.acf`
  lists every subscribed item (`WorkshopItemDetails`); it exists in the
  Steam library that holds Wallpaper Engine on your Linux box (the
  library folder, not necessarily `~/.local/share/Steam`). Copy that file
  to the Mac into a folder of its own — NOT the sync root, whose manifest
  SteamCMD rewrites — e.g.
  `~/Library/Application Support/kwe/subscriptions/steamapps/workshop/`,
  and pass `--manifest-root ~/Library/Application Support/kwe/subscriptions`.
  (With no source flags the tool reads every discovered Steam library
  except the sync root.) Re-copy it after subscribing to new items.
- **A public Workshop collection** you curate on any device:
  `--collection <id or URL>` (repeatable). No credentials involved.
- **Explicit items:** `--item <id or URL>` (repeatable).
- **The Steam Web API** with your own key: `--api-key <key> --steamid
  <SteamID64>`. Valve may restrict that call to publisher keys; the tool
  says so plainly if refused.

What it does: merges the sources, drops ids that are not Wallpaper Engine
items (via the key-less `GetPublishedFileDetails`), runs SteamCMD in
batches of 25 with `@NoPromptForPassword` against its cached session,
reports every item, optionally installs the app's Windows build for
`assets/` (`--assets`, about 1 GB, one time), and asks the running daemon
to rescan. Items land in `<root>/steamapps/workshop/content/431960/<id>`
where `<root>` is `STEAM_ROOT` or `~/Library/Application Support/kwe/steam`
(a default scan root on macOS). `--dry-run` lists without downloading;
`--json` for scripts. Unsubscribing does not delete: remove the item's
folder yourself. Re-running updates changed items.

If the cached session expired the run stops with `steamcmd login failed`;
repeat the interactive login once.

## Option C: SteamCMD by hand

SteamCMD runs natively on macOS and can download Workshop items for an app
the account owns, and the app's Windows depot for the `assets/` folder:

```sh
brew install --cask steamcmd            # or download from Valve
steamcmd +login <account> \
  +workshop_download_item 431960 <item id> \
  +quit
# items land in ~/Library/Application Support/Steam/steamapps/workshop/content/431960/

steamcmd +@sSteamCmdForcePlatformType windows +login <account> \
  +force_install_dir ~/WallpaperEngine/steamapps/common/wallpaper_engine \
  +app_update 431960 validate +quit
```

Only the `assets/` subfolder of the app install is used. Whether pulling a
Windows depot through SteamCMD for use outside the app is acceptable under
Steam's Subscriber Agreement is the user's responsibility; the maintainer
has not obtained a legal opinion (plan gate G2). This option is documented,
not automated, for that reason.

## Verifying

```sh
kwe scan                                   # lists items with kind/compatibility
kwe scan --json | jq '.items | length'
```

Scene wallpapers need the `assets/` folder; without it they are refused
at apply time with an actionable message.
