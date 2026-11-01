# BlizzNux Launcher

Play Blizzard games on Linux. No Steam needed.

BlizzNux installs Blizzard's official Battle.net app on your computer and sets everything up so
your games just work: World of Warcraft, Hearthstone, Overwatch, Diablo, StarCraft, Heroes of the
Storm and more. You log in to Battle.net, install your games and play, just like on Windows.

## What it does

- **Installs Battle.net for you.** One click, and the launcher downloads it straight from
  Blizzard and gets it ready.
- **Sets everything up on its own.** Anything else your games need is downloaded and
  configured on the first start. There is nothing to tweak.
- **Checks your computer.** If something is missing for games to run, the launcher tells you
  and offers to fix it.
- **Keeps itself up to date.** You are told when a new version is ready.

## Get it

Download the file for your system from the
[releases page](https://github.com/BlizzNux/launcher/releases/latest):

| Your system | Download |
|---|---|
| Ubuntu, Linux Mint, Debian, Pop!_OS | the `.deb` file |
| Fedora, openSUSE | the `.rpm` file |
| Anything else | the `.AppImage` file |

Open the `.deb` or `.rpm` file to install it, then start **BlizzNux** from your app menu.
For the `.AppImage`, right-click it, open **Properties**, allow it to run as a program, then
double-click it.

## What works

The games made by Blizzard run. Call of Duty does not, because its anti-cheat does not allow
Linux. The full list is on the [BlizzNux wiki](https://blizznux.com/wiki).

## Testing a pull request

Every pull request in this repository gets a test build, published as a GitHub pre-release
named `dev-pr<number>` (the pull request has a comment with the link). It is rebuilt on each
push and deleted when the pull request closes. It is not signed, and the launcher's updater
never offers it.

Download the AppImage from
`https://github.com/BlizzNux/launcher/releases/download/dev-pr<number>/BlizzNux-dev-pr<number>.AppImage`
(for example [pull request 32](https://github.com/BlizzNux/launcher/releases/download/dev-pr32/BlizzNux-dev-pr32.AppImage)),
then right-click → Properties → Permissions → Is executable, and double-click it.

From a terminal:

```
gh release download dev-pr32 -R BlizzNux/launcher -p '*.AppImage'
```

## Releases

A stable release is cut automatically every Sunday by a job on the maintainer's server, and
can be run by hand with `scripts/release.sh`. If no file that ends up in the bundles changed
since the last `v*` tag (`src-tauri/`, `ui/`, `bin/`, `package.json`, `package-lock.json`), it
does nothing. Otherwise it opens a `release-v<version>` pull request with the version bump
(the next patch version), waits for its checks, squash-merges it, builds the signed AppImage,
deb and rpm plus `latest.json` in the build container, and publishes the release on the merged
commit. The notes list the pull requests merged since the last tag.

```
scripts/release.sh --dry-run            # version, notes and a full signed build; publishes nothing
scripts/release.sh [version] [notes-file]
```

The updater signing key never leaves the maintainer's machine. From inside a container the
script translates bind-mount paths for the Docker host automatically
(`BLIZZNUX_HOST_PATH_MAP=<path here>=<path on host>` overrides it).

## Need help?

Ask on the [BlizzNux forum](https://blizznux.com) or
[open an issue](https://github.com/BlizzNux/launcher/issues).

---

BlizzNux is a free community project. It is not affiliated with, endorsed by, or sponsored by
Blizzard Entertainment. Battle.net and all game titles are trademarks of their owners. BlizzNux
never shares Blizzard's software: Battle.net is downloaded from Blizzard's own servers.

License: GPL-3.0-or-later. See [LICENSE](LICENSE).
