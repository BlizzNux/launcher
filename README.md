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

**v0.1 — command-line launcher.** One script that installs, imports and launches Battle.net.
A desktop application (news feed from BlizzNux, community tab, game tiles, auto-update) is
the next milestone; this script is its engine and stays usable on its own.

Tested on CachyOS with Proton-CachyOS and a hybrid AMD + NVIDIA laptop. Other distributions
are best effort; please report what you find. Guides and the compatibility list live on the
[BlizzNux wiki](https://blizznux.com).

## Requirements

- `umu-launcher` — CachyOS and Arch: `pacman -S umu-launcher`; Fedora: the
  [umu COPR](https://copr.fedorainfracloud.org/); other distros: see the
  [umu-launcher releases](https://github.com/Open-Wine-Components/umu-launcher/releases).
- `curl`, `bash`.
- A Proton build. If Proton-CachyOS or GE-Proton is already present it is used; otherwise umu
  downloads the latest GE-Proton on first run.
- Vulkan drivers for your GPU (Mesa for AMD/Intel, the proprietary driver for NVIDIA).

## Install

```sh
mkdir -p ~/.local/bin
curl -fL https://raw.githubusercontent.com/BlizzNux/launcher/main/bin/blizznux-run -o ~/.local/bin/blizznux-run
chmod +x ~/.local/bin/blizznux-run
```

Make sure `~/.local/bin` is on your `PATH`.

## Use

```sh
blizznux-run                 # first run downloads Battle.net from Blizzard and installs it
blizznux-run --game WoW      # start Battle.net and launch a game directly
blizznux-run doctor          # show which umu, Proton, prefix and GPU settings apply
```

### Already have Battle.net under Steam, Lutris or Bottles?

Point the launcher at that prefix instead of downloading hundreds of gigabytes again:

```sh
# Steam non-Steam shortcut (find the id under steamapps/compatdata)
blizznux-run import ~/.local/share/Steam/steamapps/compatdata/<appid>/pfx
# Lutris
blizznux-run import ~/Games/battlenet
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

## What works

See the game compatibility list on the [BlizzNux wiki](https://blizznux.com).
In short: the Blizzard-developed titles run; Call of Duty does not, because its Ricochet
anti-cheat does not allow Linux.

## Contributing

Issues and pull requests are welcome. Changes to `main` require review by the BlizzNux
maintainers. By contributing you agree your work is licensed under the GPL-3.0-or-later.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
