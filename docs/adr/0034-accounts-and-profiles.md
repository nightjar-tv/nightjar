# ADR-0034: Accounts and profiles

- Status: accepted
- Date: 2026-08-06
- Depends on: ADR-0003 §3 (no auth in v0, ended by this ADR); ADR-0007
  (playback sessions, cap model); ADR-0011 (session sharing removed);
  ADR-0022 §5 (policy ceilings, no policy schema until accounts exist);
  ADR-0025 §1 (opaque key contract); ADR-0028 (manual fix must go admin-only)
- Gate: Gate 3 — resume works across devices and survives server restarts;
  kids scoping and parent overrides are unreachable from a profile session
  including an adult-flagged one; full v1 API frozen
- Related: Block 2 plan B2-A (`nightjar-meta/docs/BLOCK_2_PLAN.md` §2); decision
  sheet `nightjar-meta/notes/design/adr-0034-questions-2026-08-06.md` (twelve
  questions signed off 2026-08-06 before B2-1 dispatch); B2-B watch state;
  B2-D kids scoping; B2-9 policy ceilings

## Context

Nothing in the server knows who is asking. ADR-0003 §3 chose single-user local
trust for v0 and put auth in Phase 3, and every shape decided since then has
deferred to that: ADR-0025 keys watch state on `item_key` with no owner,
ADR-0022 §5 says there is no policy schema until accounts exist, ADR-0007 caps
playback sessions globally because there is no user to charge them to, and
ADR-0028's assign endpoint rewrites watch state for anyone who can reach the
port. This ADR is the root the rest of Block 2 hangs off, and it decides the
identity shape before any of those writers exist (Rules 4.9, 6.1).

Two forces set the shape. A household has one or two people who own the server
and several who only watch it, so authority and viewing identity are different
things and fusing them means splitting them again when the kids cap arrives in
B2-D. And a television is a bad place to type a password, so switching viewer
has to be cheap while switching authority has to not be.

## Decision

**Two tiers. An account holds credentials and the server-management toggle; a
profile holds viewing identity and never holds authority.**

1. **Account and profile.** An account holds a username, an Argon2id password
   hash, and a `can_manage_server` boolean. A profile holds a name, the durable
   reference in item 6, a classification cap, and a simple-interface flag, and
   belongs to exactly one account. Profiles never share watch state, including
   profiles under one account. The cap and the flag are two fields and stay two
   fields; what they mean is decided by the kids-scoping ADR (B2-D), and this
   ADR decides only that both columns exist on the profile.

2. **No role system.** Owner and member is the whole model. `can_manage_server`
   is a boolean on the account, not a row in a roles table and not a permission
   set (Rule 4.7). A second axis of authority arrives with a second concrete use
   case or not at all.

3. **Authority is account scope, and profile scope never carries it.** A logged
   in session is in account scope until it selects a profile, and selecting a
   profile narrows it. Admin routes require `can_manage_server` **and** account
   scope, so no profile session administers the server, including the account
   holder's own profile and including an adult-flagged one. Narrowing to a
   profile is free. Widening back to account scope re-authenticates with the
   account password. B2-7 is expected to add a PIN as an equal-strength shortcut
   here, but B2-7 is not designed yet and this ADR does not bind its routing.
   Without the re-authentication a child on the television leaves the cap
   by tapping a menu, which defeats B2-D before it ships.

   Account scope browses unrestricted, because the manual fix flow (ADR-0028)
   has to see the item it is fixing. Account scope cannot write watch state and
   cannot start playback: those need a profile, and an account with no profile
   would have nowhere to write. **Cannot start playback means the byte routes
   refuse an account-scope token**, not that a client hides a button.
   `GET /items/{id}/stream` and `POST /items/{id}/sessions` both reject account
   scope with a named error. ADR-0022 §6 already reached this: enforcement
   belongs on `/stream` and session start rather than only on `playbackInfo`,
   because an authenticated caller can skip the client and request the stream URL
   directly. A ceiling the client is trusted to respect is not a ceiling. Creating an account therefore creates one
   profile in the same transaction, uncapped, simple interface off. Those two
   values are a creation default and not a semantics decision; what a cap means
   is still B2-D's to make. The two
   scopes are what B2-D's non-defaultable scope parameter distinguishes; the
   filter itself is B2-D's decision, not this one.

4. **Argon2id at m=19456 KiB, t=2, p=1, 16-byte salt, 32-byte output.** This is
   the second recommended parameter set in RFC 9106, and it is chosen against
   the slowest machine we claim to run on. Published guidance puts this
   configuration in the tens of milliseconds on N150-class hardware and the low
   hundreds on Pi 4 class, which is right for a box that is also serving
   transcodes. We have not measured either, and no acceptance in B2-1 turns on
   the number; if login latency on the Gate 1 Pi carry is bad the constants move
   and old hashes keep working, because of the PHC storage below. New
   dependency under Rule 1.1: `argon2` (RustCrypto, pure Rust, maintained), plus
   an OS randomness source for salts and tokens. `sha2` is already in the
   workspace. Token and hash rendering is lowercase hex written locally rather
   than a base64 crate (Rule 4.4).

   Hashes are stored as PHC strings, so the parameters travel with the hash and
   raising the constants later does not lock anyone out. A hash whose encoded
   parameters are below the current constants verifies against its own encoded
   parameters and is rehashed on the next successful login. A hash that is not
   `argon2id` is refused rather than upgraded, and that refusal is the negative
   case proving the parameters are applied and not defaulted.

5. **Login sessions are opaque server-side tokens, stored hashed.** A login
   mints 256 bits from the OS CSPRNG. The server stores `SHA-256` of the token,
   never the token, and returns the plaintext exactly once. SHA-256 rather than
   Argon2 is correct here and deliberate: the token is a full-entropy random
   value, not a guessable secret, and it is hashed on every request, so a slow
   KDF would buy nothing and cost per-request latency.

   A session row carries the account, a nullable active profile, issue and
   expiry timestamps, `last_seen_at`, a nullable `revoked_at`, and a client
   label supplied at login so the revoke list can name a device. Absolute
   expiry is 90 days with no sliding renewal and no refresh token (Rule 4.7).

   That 90 days is a real cost on a television, where re-entering a password by
   D-pad four times a year is an event. **Device-scoped long-lived tokens are the
   expected answer and they are out of scope for v1**, recorded here so a later
   phase finds a decision rather than a schema change. The session row already
   carries the account, the client label, and its own expiry, so a device token
   is a different lifetime on the same row and not a second table. This is the
   move ADR-0022 §5 made when it split capability from policy before either was
   enforced.

   Revocation sets `revoked_at` rather than
   deleting the row, so unknown, expired, and revoked stay three distinct typed
   errors instead of collapsing into one. Sign out everywhere revokes every
   session on the account. A sweeper removes rows well past expiry.

   Deleting a profile revokes every session whose active profile is that
   profile. Setting the reference null instead would silently widen a capped
   session into unrestricted account scope, which is a privilege escalation
   dressed as a foreign key action.

   These are login sessions. The playback sessions of ADR-0007 and ADR-0011 are
   a different object on a different route namespace, and the wire name
   `sessions` stays theirs; login lives under `/api/v0/auth/`.

6. **Durable opaque `profile_ref`, not the rowid.** A profile carries a
   reference minted at creation from 128 bits of OS randomness, rendered as 32
   lowercase hex characters. Playback events log against it. Clients treat it as
   opaque exactly as they treat `item_key` (ADR-0025 §1); the server never parses
   it. SQLite reuses rowids after a delete, so profile 4 deleted and profile 4
   recreated would inherit the old profile's history, which is the reason
   ADR-0025 refused `media_items.id` as a watch key. `AUTOINCREMENT` was the
   alternative and is rejected because a monotonic integer still leaks profile
   creation order.

   **The wire name is `profileRef`, never `profileId`.** `profileId` is taken:
   ADR-0022 uses it for the client capability profile (`BROWSER_V0`) as a query
   parameter on `/stream` and as a field on the capability bag. B2-9 puts
   per-user policy ceilings on that same route, so both meanings would otherwise
   meet on one endpoint and a reader could not tell which profile a parameter
   meant. ADR-0022 keeps `profileId`. Viewer profiles are `profileRef` in every
   path segment, query parameter, and response field, and that includes the
   continue-watching route in B2-4.

7. **Profile deletion, one sentence used by this ADR and the watch-state and
   playback-event ADRs.** Deleting a profile destroys its watch state and its
   name, and leaves playback-event rows in place with their profile reference no
   longer resolving to anything. `watch_state.profile_id` cascades on delete;
   `playback_events.profile_ref` is a plain column with no foreign key and does
   not. Deleting an account deletes its profiles and revokes its sessions. The
   last account with `can_manage_server` cannot be deleted or have the toggle
   cleared.

8. **Playback sessions are never shared between profiles, which closes the
   ADR-0007 deferral.** ADR-0011 already removed item-keyed sharing and
   fork-on-scrub for encoder reasons. Real profiles do not reopen it: two
   profiles at the same `item_key` need independent watch writes and will get
   independent track selection (ADR-0024, persisted in B2-E), and a shared
   session would carry content across a kids scope boundary. A playback session
   is owned by the profile that created it.

   **Per-account playback concurrency, the per-user half of the cap model
   ADR-0007 anticipated, has its shape decided here and its enforcement in
   B2-9.** The cap counts concurrent playback sessions across every profile under
   one account, not per profile, because household fairness is about the person
   paying for the bandwidth and three profiles must not buy three times the
   share. It is a nullable integer on the account where null means no
   per-account limit, which is the shipped default. The effective limit is the
   tighter of the global `NIGHTJAR_HLS_MAX_SESSIONS` and the per-account value,
   composing the same way ADR-0022 §5 composes client capability against server
   policy, and a refusal carries a ceiling reason that names which of the two
   bound. Enforcement is at playback session start and nowhere else.

   Enforcement waits for B2-9 because it touches transcode session lifecycle and
   can regress the Gate 2 concurrent-1080p floor, and B2-9 is where the ceiling
   reason strings already live. The numeric default stays open until B2-9
   measures it. **Login sessions are not capped, now or in B2-9.** That number is
   device count, and capping it signs a household out of its television to admit
   a phone.

9. **One credential, two transports, one verifier.** The API authenticates with
   `Authorization: Bearer <token>`. Login additionally sets an `HttpOnly`,
   `SameSite=Lax` cookie holding the same token, scoped to `/api/v0` and marked
   `Secure` when the request arrived over TLS. The cookie lives exactly as long
   as its session: expiry, revocation, and sign-out-everywhere kill both
   credentials at once, because they are one credential in two envelopes.

   **The cookie is accepted only on GET and HEAD of an enumerated route list,
   matched after routing has resolved a handler, never by path prefix.** The
   list is `GET /api/v0/artwork/{itemKey}/{kind}`,
   `GET /api/v0/items/{itemId}/stream`,
   `GET /api/v0/items/{itemId}/subtitles/{trackId}.vtt`, and under
   `/api/v0/sessions/{sessionId}/`: `runs/{runId}/master.m3u8`,
   `runs/{runId}/index.m3u8`, `runs/{runId}/init.mp4`, `subs/{asset}`, and
   `{asset}`. Nothing else, and adding a route to that list is a decision, not a
   consequence of where a path happens to sit.

   Prefix matching is how this control fails. `/api/v0/items/` as a prefix
   admits the whole item surface including the metadata fix endpoints B2-2 is
   about to make admin-only, and it would do it silently, on the day someone adds
   a route rather than on the day someone changes the rule. A test asserts the
   accepted set is exactly these eight and that every other route rejects a
   cookie-only request.

   The cookie exists because an HTML element cannot set a request header. A
   `<video src>`, a native Safari HLS load, and an `<img>` poster all fetch
   without JavaScript in the path, so a header-only design would leave the
   browser with no way to authenticate the bytes it exists to play. Native
   clients drive their own players and set headers, so they use the bearer token
   and ignore the cookie.

   This is one session concept, one table, and one verification function reading
   the credential from two places, which is what Rule 4.11 asks for rather than
   two auth systems. Because no state-changing route accepts the cookie, CSRF is
   structurally absent instead of defended: a cross-site form or image cannot
   reach anything that writes. Query-parameter tokens are rejected outright,
   including for media routes: they land in access logs, browser history, and
   `Referer` headers, and a media server's logs are the first thing a user
   pastes into an issue.

10. **First run: bootstrap, then admin, then libraries.** `POST /api/v0/auth/
    bootstrap` creates the first account with `can_manage_server` set. It is the
    one unauthenticated write in the server and it is gated on exactly one
    condition, that no account exists. No token, no environment variable, no
    setup code printed to the log: core behaviour works by default (Rule 4.12)
    and a printed code is unreadable in half the ways this server is deployed. A
    second attempt against a database that already holds an account is refused
    with a named `bootstrap_already_complete` error, not a generic 403.

    The order falls out of B2-2 rather than being a preference: B2-2 makes
    `POST /libraries` admin-only, so on a fresh install no library can be added
    until an admin exists. It is written here so a Block 3 wizard is not built in
    the other order and the conflict found at integration.

11. **Setup state is two independent booleans, never one flag.**
    `GET /api/v0/system/setup` is unauthenticated and returns `adminExists` and
    `libraryExists`. Additive under Rule 2.3 and the same class as
    `/system/transcode`, so it costs nothing at the freeze. The third state is
    real and is the only install we have: the N150 dogfood database holds
    libraries and roughly 24,800 items and no account. A `setupComplete` boolean
    reads that as fresh, and a wizard trusting it offers to add a library the
    server already holds.

12. **Password reset is a subcommand on the binary, not a route.** An admin can
    set any account's password, including another admin's, from an admin
    session. A household whose only admin forgets theirs runs
    `nightjar reset-password <username>` on the server. No route, no OpenAPI
    change, no recovery email, no security questions.

    A subcommand requires local filesystem access, which is already the trust
    boundary for opening the database in a SQLite editor, so it adds no attack
    surface and replaces an undocumented database edit with one documented
    command. "Recover by editing SQLite" is an acceptable answer for the person
    who wrote the schema and a bad one against the grandma gate. It belongs to
    the same family as the planned Phase 4 `nightjar doctor`, and it is the
    escape-hatch half of Rule 4.12 rather than a preference.

## Alternatives considered

**One tier, accounts only.** Every viewer gets a password. Rejected because the
kids cap then attaches to a credential, so a parent cannot hand a child a
television without either sharing a password or managing another login, and
switching viewer on a remote control becomes a typing exercise.

**A roles or permissions table.** Rejected under Rule 4.7. There are two kinds of
person in a household media server and a boolean says so.

**JWTs or another stateless token.** Rejected. Revocation and "sign out
everywhere" need server state regardless, so a stateless token buys a signing-key
rotation problem and a revocation list, and arrives back at a table. The server
already has SQLite open on every request.

**Signed short-lived media URLs instead of the cookie.** This is the design that
scales to a CDN and it is genuinely tempting, since ADR-0020 already has the
server minting playlist URIs per run. Rejected for v1 under Rule 4.7: it is a
key-rotation and clock-skew subsystem solving a case the cookie already covers.
It becomes the right answer if a client appears that can hold neither a cookie
nor a header, and nothing on the ADR-0001 client list is that client.

**bcrypt, scrypt, or PBKDF2.** All fine in isolation. Argon2id is the current
recommendation for new work and the maintained Rust implementation is
comparable in dependency weight.

**Session-scoped profile selection as a second token.** Selecting a profile could
mint a new token and discard the old one. Rejected: it doubles the token count
per device for no gain, and the revoke list becomes a list of the same device
several times.

## Consequences

- Account scope is a real UI state, not an implementation detail. The account
  holder leaves their profile to administer, and Block 3 has to make that
  legible or people will think admin has disappeared.
- The binary grows a subcommand surface for the first time. `reset-password` is
  the only one this ADR adds, and it is worth naming that the precedent is now
  set: anything else wanting to be a subcommand argues for itself.
- Password recovery requires physical or shell access to the server. There is no
  path back in over the network for the last admin, by design, and that goes on
  the security-pass list as a stated posture rather than being found by the
  auditor.
- The bootstrap window is a real hole: until the first account exists, anyone who
  can reach the port can claim admin. Plex and Jellyfin have the same window. It
  is accepted, and it is named here so the cross-cutting security pass carries it
  alongside trusted-proxy and the traversal audit.
- On plain HTTP over a LAN, both the cookie and the bearer token travel in the
  clear. TLS is the mitigation, by whichever route ADR-0002 settles on: that ADR
  is still `proposed` and its stated bias is reverse proxy or Tailscale first,
  with built-in ACME only if household testing shows proxy friction is a real
  blocker. Accounts do not decide it, and they do add a reason to.
- Every item-returning query gains a scope parameter, so B2-D's filter has
  somewhere to attach and a Block 3 route inherits it by construction.
- ADR-0007's global `NIGHTJAR_HLS_MAX_SESSIONS` stays global after this slice.
  The per-user half of that cap model waits for B2-9, so accounts exist for a
  slice or two before they constrain anything.
- `watch_state` and `playback_events` do not exist yet. This ADR fixes the
  identity they reference and nothing else; their own shapes are B2-B and B2-C.
