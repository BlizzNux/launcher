# Launcher reports: client ↔ site contract

The launcher can send two kinds of reports to blizznux.com. The site stores them in Flarum.
This file is the contract between the launcher (this repository) and the site endpoint.

## Principles

- **The launcher never holds the BlizzNux account's credentials.** It posts to an endpoint on
  blizznux.com; the endpoint does the Flarum writes with its own key.
- **Nothing leaves the machine without the user seeing it first**, unless the user has turned
  on automatic sending in Settings. Home paths are replaced with `~` before anything is sent.
- **Anonymous by default.** Each install carries a random `install_id` (UUID v4, generated once,
  stored in the launcher config) so the endpoint can rate-limit and de-duplicate.
- **Optional account link.** A user can log in with blizznux.com (see *Account link* below). The
  launcher then adds `user_token` to every report and the site attributes the report to that
  account. Only linked users can opt into automatic sharing.

## Endpoint

`POST https://blizznux.com/api/launcher/reports`
`Content-Type: application/json`

The launcher reads the URL from `BLIZZNUX_REPORT_URL` when set (used for testing against a
local stub); the default is the URL above.

### Request body

```json
{
  "schema": 1,
  "type": "bug" | "run" | "launch",
  "install_id": "4b9e…",
  "user_token": "…",            // empty string when the install is not linked
  "launcher_version": "0.2.0",
  "created_at": "2026-10-02T18:40:00Z",
  "system": {
    "distro": "CachyOS Linux",
    "kernel": "7.2.8-1-cachyos",
    "desktop": "KDE",
    "session": "wayland",
    "cpu": "AMD Ryzen 7 7840HS",
    "ram_gb": 14,
    "gpus": [ { "vendor": "nvidia", "name": "…", "driver": "615.71.09" }, { "vendor": "amd", "name": "…", "driver": "mesa 26.2.4" } ],
    "proton": "proton-cachyos-11.0-20260924",
    "umu": "1.4.3",
    "prefix_kind": "steam" | "lutris" | "blizznux" | "other"
  },
  "battlenet": { "build": "17896", "version": "2.53.4.17896" },
  "game": { "code": "WoW", "name": "World of Warcraft", "flavor": "_retail_", "version": "12.0.1.65432" },
  "target": "battlenet",        // launch reports only
  "outcome": "perfect" | "issues" | "broken" | "ok" | "failed",
  "comment": "free text from the user, may be empty",
  "log": "last ~200 lines of the launcher log, bug reports only, paths scrubbed"
}
```

`game` is absent for a bug report that is not tied to a game and for launch reports.
`outcome` and `comment` are absent for bug reports; `log` is absent for successful runs and
launches.

Report types and when the launcher sends them:

| type | when | outcome | sent |
|------|------|---------|------|
| `launch` | the user pressed Launch; Battle.net appeared within 2 min (`ok`) or did not (`failed`, with log) | `ok` / `failed` | automatically, only when the user opted into sharing; `ok` at most once per day per Battle.net build per install, `failed` always |
| `run` | a game session ended after ≥ 60 s (`ok`), or the game vanished within 60 s (`broken`) | `ok` / `broken` automatically; `perfect` / `issues` / `broken` when the user answers the prompt | automatic ones only when sharing (`ok` at most once per day per game build per install, `broken` always); prompt answers always, after a preview unless the user turned previews off |
| `bug` | the user chose "Report a problem", or a launch failed and they confirmed | — | after a preview |

### Response

`201 Created` with `{ "url": "https://blizznux.com/d/123-…" }`, the discussion the report
landed in. `429` when rate-limited (the launcher shows "try again later"), `400` with
`{ "error": "…" }` for a malformed body.

## Account link

Linking never moves a credential between origins. The site shows a one-time code; the
launcher exchanges it over HTTPS.

1. The launcher opens `https://blizznux.com/launcher/link` in its embedded view. Logged-out
   users see the normal login / sign-up there first.
2. For a logged-in user the page shows a **one-time code**: 8 characters, uppercase letters and
   digits without `0 O 1 I`, valid 10 minutes, single use, bound to that user; with a
   "Copy code" button and the sentence "Paste this code into the BlizzNux launcher".
3. The user pastes it into the launcher, which calls
   `POST https://blizznux.com/api/launcher/link` with
   `{"code": "ABCD2345", "install_id": "<uuid>", "launcher_version": "0.2.1"}`.
   Responses: `201 {"token": "<≥32 chars, [A-Za-z0-9._-]>", "username": "<display name>"}`;
   `400 {"error": "…"}` for an unknown, expired or used code; `429` when rate-limited
   (suggested: 10 attempts per minute per IP).
4. The site stores the token with the user id, the `install_id` and a creation date; the user
   can revoke it from their profile. The launcher sends it as `user_token` on every report;
   an unknown or revoked token is treated as anonymous (the report is still accepted).
   "Log out of the launcher" only deletes the token locally.

The launcher reads `BLIZZNUX_LINK_URL` as a test override for the exchange endpoint.

## What the site does with each type

- **bug**: creates a discussion in a private tag readable by staff only, posted by the
  BlizzNux account, titled `Launcher bug — <distro> — <short date>`, body = the report
  rendered as text with the log in a code block.
- **launch**: appended to the user's compatibility record (linked users only); failed launches
  are also visible to staff with their log. Not posted as forum threads.
- **run**: finds the Updates thread for that exact game build (the site already auto-posts
  one per patch, e.g. "Hearthstone patched — build 36.6.3.253932.253216"); if none exists it
  creates one. Appends a reply: outcome, setup summary, comment. Reports with the same
  `install_id` + game build within 24 h replace the previous reply instead of adding one. Linked
  reports are posted as the user (or as BlizzNux with "reported by @user"); anonymous ones as
  BlizzNux. Automatic `ok` runs may be aggregated ("12 users ran this build fine") rather than
  posted one by one.

## Rate limits (endpoint side)

Per `install_id`: 10 bug/run reports per hour; `launch` reports are counted and limited
separately so the automatic stream can never crowd out a report a person chose to send.
Per IP: 60 per hour. Body limit 256 KB.

## What the site shows

- The user's own profile gets a compatibility section listing their launches and runs (date,
  game and build or Battle.net build, outcome, launcher version), visible to the user and staff.
  Failure logs are visible to that user and staff only. Staff also get an admin page of
  failures with logs.
- Linked reports in threads carry a "by @name" byline; the Updates tag permissions stay as they are.
- Automatic `ok` runs are aggregated per build over distinct installs into one site-maintained
  reply, e.g. "Ran without problems for N Linux users so far (last report <date>)."
