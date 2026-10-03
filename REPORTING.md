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
  "log": "last ~200 lines of the launcher log (repeats folded as \"(×N) …\"), bug reports only, paths scrubbed"
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

## Account link (pairing)

Logging in links the launcher automatically, inside the launcher's embedded view, with
Flarum's own login dialog. No code is typed, no credential crosses between pages, and no
URL can approve a pairing: the page learns which pairing to approve only from a cookie that
the launcher plants in its own cookie store.

1. **Start.** The launcher calls `POST https://blizznux.com/api/launcher/link/start` with
   `{"install_id": "<uuid>", "launcher_version": "0.2.1", "distro": "CachyOS Linux"}`.
   Response `201 {"pair_id": "<public id>", "pair_secret": "<≥32 chars>", "expires_in": 600}`.
   CSRF-exempt (desktop app). Rate-limit 10 per minute per IP.
2. **Cookie.** The launcher writes `bz_pair=<pair_id>; Domain=blizznux.com; Path=/; Secure;
   HttpOnly; SameSite=None; Max-Age=600` into its own WebKit cookie store, then loads
   `https://blizznux.com/launcher/link` (no parameters) in its embedded view. Only the
   launcher can plant that cookie; page script cannot read it; a crafted link, another site or
   a phishing page cannot set cookies for blizznux.com in anyone's browser.
3. **Login.** For a logged-out visitor the page opens Flarum's standard LogInModal at once,
   "Remember me" pre-ticked (sign-up and password reset as on the site); a "Log in to link the
   launcher" button reopens the modal if dismissed.
4. **Approve.** Once `app.session.user` exists (after the forum app has booted, so
   `app.session.csrfToken` is defined), the page calls `POST /api/launcher/link/approve` with
   an empty JSON body and the `X-CSRF-Token` header. The endpoint is browser-only and
   CSRF-protected and must stay that way; it reads `pair_id` from the `bz_pair` cookie and
   ignores any body. It approves **automatically** when the pairing is unexpired and unused,
   binding it to the logged-in user; single use. Responses: `2xx` approved; `204` no cookie
   (ordinary visitors see nothing); `404` unknown pair; `410` expired; `409` already used.
   The page shows "Linked to the BlizzNux launcher as <username>" on success.
5. **Collect.** From the moment it opened the page, the launcher polls
   `POST /api/launcher/link/poll` with `{"pair_id": "<id>", "pair_secret": "<secret>"}` every
   two seconds: `202 {"status": "pending"}`; `201 {"token": "<≥32 chars [A-Za-z0-9._-]>",
   "username": "<display name>"}` once approved (pairing consumed); `410 {"error":
   "expired"}` after 10 minutes; `404` for an unknown pair or wrong secret (one opaque answer
   for both). CSRF-exempt; allow ~60 per minute per IP. On success the launcher deletes the
   cookie and returns the embedded view to the forum, which is now logged in.
6. The site stores the token with the user id, `install_id` and creation date, revocable from
   the user's profile. The launcher sends it as `user_token` on every report; an unknown or
   revoked token is treated as anonymous (the report is still accepted). "Log out of the
   launcher" only deletes the token locally.

After linking, the launcher shows the sharing choices once (both off by default): share
verification reports (launches and game sessions) and send bug reports automatically.

Test overrides: `BLIZZNUX_LINK_URL` (API base) and `BLIZZNUX_LINK_PAGE` (page URL; its host
is also used for the cookie). The one-time-code exchange from an earlier revision is unused.

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

## Profile fields (Masquerade)

Site side only. On each accepted report from a linked account the site writes the Masquerade
fields `Distro`, `Kernel`, `GPU`, `GPU driver` and `Proton / Wine` from `system`, matched by
name; missing values never blank a field and errors never fail the report. The launcher has no
profile writer (an earlier one was removed: it needed the Masquerade `have-profile` permission,
which members do not have).
