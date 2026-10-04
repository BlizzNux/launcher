# BlizzNux Launcher

Battle.net games on Linux, launched through Proton, with **no Steam required**.

This is the launcher project of [BlizzNux.com](https://blizznux.com). It runs Blizzard's
official Battle.net client inside a Proton prefix using
[umu-launcher](https://github.com/Open-Wine-Components/umu-launcher), the same runtime
Lutris and Heroic use, so games get the same compatibility fixes they would under Steam
without Steam being installed.

> BlizzNux is a community project. It is not affiliated with, endorsed by, or sponsored by
> Blizzard Entertainment. Battle.net and all game titles are trademarks of their owners.
> This project never redistributes Blizzard software; the installer is downloaded from
> Blizzard's own servers on your machine.

## Status

**v0.1 — command-line launcher.** One script, `bin/blizznux-run`, that installs and
launches Battle.net. It is the engine for everything else and stays usable on its own.

**v0.2 — desktop app (in development).** A Tauri 2 application in `src-tauri/` and `ui/`:
BlizzNux.com fills the window, with a control bar underneath: Launch Battle.net, Settings
(GPU offload, display scaling, diagnostics) and the Disclaimer. An update check against GitHub releases shows in the bar when a newer version
exists. Launching goes through the same script.

### Building the desktop app

Requires Rust (stable), Node, and WebKitGTK 4.1 with GTK 3 development files.

```sh
npm install
npm run dev      # run with live reload
npm run build    # AppImage, deb and rpm under src-tauri/target/release/bundle/
```

Released bundles are built with `scripts/container-build.sh`, inside an Ubuntu 22.04 container
(needs Docker), so that they also run on distributions older than the build machine.

Tested on CachyOS with a hybrid AMD + NVIDIA laptop. Other distributions
are best effort; please report what you find. Guides and the compatibility list live on the
[BlizzNux wiki](https://blizznux.com/wiki).

## Supported setup

One setup is supported: Battle.net installed by the launcher into the launcher's own prefix,
running on the launcher's own Proton and umu-launcher. The launcher downloads both on first
use, and they are the same versions on every machine: the ones Blizzard's games are tested
with here. They move to newer versions with a launcher release.

There is no setting for another Proton build or another umu-launcher, and prefixes from Steam,
Lutris or Bottles are not imported. A prefix that an earlier version of the launcher was
pointed at keeps working, but it is not a supported setup.

## Requirements

- `curl`, `bash`, `python3`, `xz`.
- Vulkan drivers for your GPU (Mesa for AMD/Intel, the proprietary driver for NVIDIA).

The launcher fetches the rest on first use, into its data folder and without root:
umu-launcher's own [release](https://github.com/Open-Wine-Components/umu-launcher/releases)
and Proton-CachyOS's [release](https://github.com/CachyOS/proton-cachyos/releases) for any
distribution (about 320 MB).

## Install

Download the desktop app from the [releases page](https://github.com/BlizzNux/launcher/releases):

| File | For |
|------|-----|
| `BlizzNux_x.y.z_amd64.AppImage` | Any distribution. `chmod +x` it and run it. |
| `BlizzNux_x.y.z_amd64.deb` | Debian, Ubuntu, Mint and derivatives |
| `BlizzNux-x.y.z-1.x86_64.rpm` | Fedora, openSUSE and derivatives |

The first run checks for the GPU drivers Proton needs and offers to install what's missing.

### From source

```sh
git clone https://github.com/BlizzNux/launcher.git
cd launcher
npm install && npm run build      # desktop app: AppImage, deb, rpm under src-tauri/target/release/bundle/
./install.sh                      # or: user-level install of the app (if built) or the script alone
```

`install.sh` puts the app (or just `blizznux-run`) in `~/.local/bin` and adds a **BlizzNux**
entry to your application menu, so it can be pinned to the taskbar or dock. Remove it with
`./install.sh --uninstall`. Launches made from the menu log to `~/.cache/blizznux/blizznux.log`,
and errors show as a desktop notification.

## Use

```sh
blizznux-run                 # first run downloads Battle.net from Blizzard and installs it
blizznux-run --game WoW      # start Battle.net and launch a game directly
blizznux-run doctor          # show which umu, Proton, prefix and GPU settings apply
```

### Hybrid laptops (NVIDIA + integrated GPU)

Render offload to the NVIDIA GPU is enabled automatically when a hybrid setup is detected.
Force it with `--offload on` or disable it with `--offload off`.

### HiDPI screens

Battle.net ignores Linux scaling. Set the Windows DPI inside the prefix instead:

```sh
blizznux-run dpi 144   # 150 %
```

### Game codes for `--game`

| Code | Game |
|------|------|
| `WoW` | World of Warcraft |
| `WTCG` | Hearthstone |
| `Hero` | Heroes of the Storm |
| `Pro` | Overwatch |
| `D3` | Diablo III |
| `Fen` | Diablo IV |
| `OSI` | Diablo II: Resurrected |
| `S2` | StarCraft II |
| `W3` | Warcraft III: Reforged |

## Your BlizzNux account

Optional. Until the launcher is linked to a [blizznux.com](https://blizznux.com) account, each
start opens the site's login page in the embedded view; logging in there links this install. With
sharing on, launches and game sessions are reported with your setup so the site can track what
runs where; failures include the launcher log with home paths removed. Switches and "Log out of
the launcher" are under Settings. Details: [REPORTING.md](REPORTING.md) and the site's wiki.

## World of Warcraft addons

When World of Warcraft is installed in the prefix, an **Addons** button appears in the bar.
It opens the AddOns folder, installs addons from a ZIP or a link (GitHub repositories and
direct downloads), and offers a one-click install of [WowUp](https://wowup.io), the
CurseForge-enabled addon manager, with your WoW installs already registered in it. Nothing
addon-related is shown or installed unless WoW is present.

## What works

See the game compatibility list on the [BlizzNux wiki](https://blizznux.com/wiki).
In short: the Blizzard-developed titles run; Call of Duty does not, because its Ricochet
anti-cheat does not allow Linux.

## Releasing

`scripts/release.sh <version> [notes.md]` bumps the version, builds signed bundles, writes the
updater manifest and publishes the GitHub release. It needs the updater signing key (kept
outside the repository; the matching public key is in `tauri.conf.json`). AppImage users get
in-app updates from that manifest; deb, rpm and Flatpak users update through their package
manager. The launcher also tells users when a newer Battle.net client is out.

## Contributing

Issues and pull requests are welcome. Changes to `main` require review by the BlizzNux
maintainers. By contributing you agree your work is licensed under the GPL-3.0-or-later.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
