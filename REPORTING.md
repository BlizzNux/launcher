# Launcher reports: client ↔ site contract

The launcher can send two kinds of reports to blizznux.com. The site stores them in Flarum.
This file is the contract between the launcher (this repository) and the site endpoint.

## Principles

- **The launcher never holds the BlizzNux account's credentials.** It posts to an endpoint on
  blizznux.com; the endpoint does the Flarum writes with its own key.
- **Nothing leaves the machine without the user seeing it first**, unless the user has turned
  on automatic sending in Settings. Home paths are replaced with `~` before anything is sent.
- **Every report needs a login.** The launcher itself works without an account, but it sends a
  report only once the user has logged in with blizznux.com (see *Account link* below). It adds
  `user_token` to every report and the site attributes the report to that account, so a report
  can be followed up and "works" counts cannot be inflated anonymously. Only linked users can
  opt into automatic sharing.
- Each install also carries a random `install_id` (UUID v4, generated once, stored in the
  launcher config) so the endpoint can rate-limit and de-duplicate.

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
  "user_token": "…",            // the account link; the site answers 401 without a valid one
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
    "proton": "cachyos-11.0-20260703-slr",
    "umu": "1.4.4",
    "runtime": "steamrt4 4.0.20260928.262390",
    "prefix_kind": "steam" | "lutris" | "blizznux" | "other"
  },
  "battlenet": { "build": "17896", "version": "2.53.4.17896" },
  "game": { "code": "WoW", "name": "World of Warcraft", "flavor": "_retail_", "version": "12.0.1.65432" },
  "target": "battlenet",        // launch reports only
  "outcome": "perfect" | "issues" | "broken" | "ok" | "failed",
  "comment": "free text from the user, may be empty",
  "log": "last lines of the launcher log (repeats folded as \"(×N) …\"), problem reports only, paths scrubbed"
}
```

`proton` and `umu` are the launcher's own, the same on every install of a launcher version.
`runtime` is the Steam Linux Runtime umu runs Proton in; umu fetches the newest one by itself, so
it can differ between machines (empty until the first start has set one up). `prefix_kind` is
`blizznux` for a prefix the launcher made; the other values are prefixes an earlier version was
pointed at.

`game` is absent for a bug report that is not tied to a game and for launch reports.
World of Warcraft keeps all its versions in one install folder, so the launcher reports the one
that actually ran: `flavor` is the folder the game ran from (`_retail_`, `_classic_`,
`_classic_era_`, `_anniversary_`, …), `version` is that version's own build, and `name` is
"World of Warcraft" for Retail and otherwise follows the folder ("WoW Classic", "WoW Classic
Era", "WoW Anniversary"); `code` stays `WoW`. When the build of the version that ran cannot be
read, no run report is sent. When the launcher does not know which version ran (an answer saved
by a launcher older than 0.2.19), it reports only if a single version is installed and never
guesses among several. `flavor` is empty for other games.

The launcher sends every version except test and beta clients (a folder with `ptr` or `beta` in
its name sends nothing and asks nothing). Which versions get a thread is the site's decision: a
run report for a version its update bot does not follow is answered `202 {"status": "not
tracked"}` without a `url`, and the launcher treats any `2xx` as delivered.
`outcome` is absent for bug reports. `log` is attached to problem reports only: `bug` and `run`
with `issues` or `broken` (the last 200 lines), and a `failed` launch (the last 120 lines). It
is absent for `perfect` and `ok` runs and `ok` launches. Logs on `issues` and `broken` runs are
shown publicly in the game's thread, as failed launches already show theirs; the launcher's
preview says so before the user sends.

Report types and when the launcher sends them. Every type needs a logged-in launcher: the
automatic ones are only sent by a linked launcher anyway, and a report the user sends while
logged out is kept and goes out right after they log in (see below).

| type | when | outcome | sent |
|------|------|---------|------|
| `launch` | the user pressed Launch; Battle.net appeared within 2 min (`ok`) or did not (`failed`, with log) | `ok` / `failed` | automatically, only when the user opted into sharing; `ok` at most once per day per Battle.net build per install, `failed` always |
| `run` | a game session in the launcher's prefix ended after ≥ 60 s (`ok`). The launcher watches its prefix for as long as it is open, so every session counts: also later games in a Battle.net that stayed open, and games in a Battle.net that was running before the launcher started. A session of under 60 s sends nothing by itself, because a lost connection to Blizzard, a login queue and quitting look the same as a crash; the launcher shows the prompt instead | `ok` automatically; `perfect` / `issues` / `broken` when the user answers the prompt | the automatic `ok` only when sharing, at most once per day per game build per install; prompt answers always once logged in, after a preview unless the user turned previews off |
| `bug` | the user chose "Report a problem", or a launch failed and they confirmed | — | after a preview, once logged in |

### Response

`201 Created` with `{ "url": "https://blizznux.com/d/123-…" }`, the discussion the report
landed in. `429` when rate-limited, `400` with `{ "error": "…" }` for a malformed body, and
`401 {"error": "Log in to BlizzNux to send reports", "code": "login_required"}` for a report
without a valid `user_token` (every type, including a failed launch).

### Reports that could not be delivered

A `run` report without `game.version` is never sent: the launcher tells the user it cannot
read the game's build instead. A report the user chose to send (a prompt answer or "Report a
problem") that does not get through is kept on disk, without the account token, for up to a
week:

- logged out: the launcher does not ask the site at all. It keeps the report (marked
  `needs_login`), opens the login, and sends it right after the user logs in, without a second
  preview. Nothing is sent while the launcher stays logged out; a start shows how many reports
  are waiting. A `401` for a launcher that thought it was logged in (the link was ended on the
  site) drops the stored link and is handled the same way. A saved report is sent at most once
  per start or login, so a second `401` keeps it waiting instead of looping;
- no answer, `429` or `5xx`: the same body is sent again on later starts, so `created_at` can
  be days older than the arrival and a body whose first answer got lost can arrive twice;
- any other `4xx` on a prompt answer: the site did not accept the report. The answer is kept
  and built afresh by the next launcher version, unless the game was patched in between; a
  refused bug report is not kept.

Automatic reports are not kept.

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
   HttpOnly; SameSite=None; Max-Age=600` into its own WebKit cookie store, waits until the
   store has taken it (at most two seconds), then loads the forum's front page,
   `https://blizznux.com/`, in its embedded view. When the user asked to log in (Log in and
   send on a report), the page is `https://blizznux.com/?login=<timestamp>`: the marker makes
   the view load the page afresh, and the site opens its login box even if one was closed
   earlier in the session. The marker carries nothing else; the pairing still comes only from
   the cookie. Only the
   launcher can plant that cookie; page script cannot read it; a crafted link, another site or
   a phishing page cannot set cookies for blizznux.com in anyone's browser.
3. **Login.** An account is optional. On a full page load from a logged-out visitor whose
   request carries an open pairing cookie, the site opens Flarum's standard LogInModal,
   "Remember me" pre-ticked (sign-up and password reset as on the site), once per pairing, and
   every time the URL carries `?login`; it can be closed and the visitor browses on. `/launcher/link` stays for people who open it by
   hand.
4. **Approve.** Once `app.session.user` exists (after the forum app has booted, so
   `app.session.csrfToken` is defined), the page calls `POST /api/launcher/link/approve` with
   an empty JSON body and the `X-CSRF-Token` header. The endpoint is browser-only and
   CSRF-protected and must stay that way; it reads `pair_id` from the `bz_pair` cookie and
   ignores any body. It approves **automatically** when the pairing is unexpired and unused,
   binding it to the logged-in user; single use. Responses: `2xx` approved; `204` no cookie
   (ordinary visitors see nothing); `404` unknown pair; `410` expired; `409` already used.
   The page shows "Linked to the BlizzNux launcher as <username>" on success.
5. **Collect.** From the moment it opened the page, the launcher polls
   `POST /api/launcher/link/poll` with `{"pair_id": "<id>", "pair_secret": "<secret>"}`: every
   two seconds for the first two minutes, then every 15 seconds, and once at once whenever the
   embedded view finishes loading a page (finishing the login reloads it). When the pairing
   expires the launcher stops asking; it does not pair again by itself. A page load after that,
   or the "Log in" button in Settings, starts a new pairing (at most three per run from page
   loads). `429` and `5xx` are waited out: the launcher pauses for `Retry-After` seconds (30
   when the header is missing) and carries on, giving up after five in a row. Answers:
   `202 {"status": "pending"}`; `201 {"token": "<≥32 chars [A-Za-z0-9._-]>",
   "username": "<display name>"}` once approved (pairing consumed); `410 {"error":
   "expired"}` after 10 minutes; `404` for an unknown pair or wrong secret (one opaque answer
   for both). CSRF-exempt; allow 60 per minute per pairing (a launcher sends at most 30),
   with only a loose cap per IP, since several launchers can share one address. On success the launcher deletes the
   cookie and returns the embedded view to the forum, which is now logged in.
6. The site stores the token with the user id, `install_id` and creation date, bound to the
   web session that approved the pairing. The launcher sends it as `user_token` on every
   report; an unknown or revoked token, or one whose web session has ended, gets `401` (see
   *Response*).

### One login

Inside the launcher there is one login, not two that can disagree: logged in on the embedded
site means linked, logged out means not linked.

- **Check.** `POST /api/launcher/link/check` with `{"install_id": "<uuid>", "user_token":
  "<token>"}`, CSRF-exempt. `200 {"username": "…"}` while the token is valid and its web
  session is alive; `401` when it is unknown, revoked, belongs to another install, or its web
  session is gone (logged out in the embedded view, expired, or revoked on the profile);
  `429` with `Retry-After` when rate-limited (30 per minute per token). The launcher asks once
  at start, after each page load that the embedded view does by itself (at least ten seconds
  apart; a question that comes too soon is asked late, not dropped), and every ten minutes
  while it is linked. Only `401` ends the link: the launcher forgets the token, keeps the sharing choices
  and starts a pairing, so a login in the embedded view links it again without a question.
  Every other answer leaves the link alone.
- **Revoke on the profile** ends the token and its web session, so the embedded view is
  logged out as well. The list of linked installs shows the latest token per install.
- **Logging out.** There is one place to log out: the site's own menu in the embedded view.
  The launcher has no log-out button of its own. When someone else logs in on the same
  launcher, the sharing choices are switched off until that person has been asked.

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
  `install_id` + game build within 24 h replace the previous reply instead of adding one. Reports
  are posted as the user (or as BlizzNux with "reported by @user"). Automatic `ok` runs may be aggregated ("12 users ran this build fine") rather than
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
