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

## Option B: SteamCMD on the Mac

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
