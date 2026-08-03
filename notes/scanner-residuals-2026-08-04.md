# Scanner residuals after #44 + #45 (updated 2026-08-04)

Plan after global index epoch (#44) and path-hinted notify (#45), revised
with N150 dogfood evidence (~6 h continuous run, 2026-08-03) and B′ design
review (poll-dirt flip so large libraries still delete).

## Shipped

| PR | What | Dogfood status |
|---|---|---|
| #44 | Process-wide index epoch; poll default 300 s; ADR-0015 amend | Confirmed: `poll_interval_s=300`; jobs serialize under one epoch |
| #45 | Path-hinted notify ingest; never `delete_missing` on hint; dirty mid-scan skips `delete_missing` when dirty | Confirmed: dirty skip in production; `fs change` fires on `/media/...`; catch-up TV job 40 added 4899 |

Deploy: N150 `nightjar-dogfood` / `nightjar/nightjar:n150-hw` @ `main` `631fa7c`.

## Dogfood evidence (why the plan changed)

### What worked

- Preferred encoder stayed `h264_qsv`.
- Libraries stable: Movies 1749, TV 23099, `pathsUnresolved=0`, reachable.
- First post-deploy TV walk (job 40): ~86 min, **added=4899**, then 23099 unchanged.
- Dirty skip line repeated for hours:
  `skipping delete_missing; library dirty during scan (follow-up will heal)`.
- Notify is **not** mute on this mount: continuous
  `fs change; requesting scan` for concrete mkv paths under Movies and TV.

### What failed to settle

The box **never went idle**. Pattern for ~50 jobs after catch-up:

```text
long index walk (minutes)
  + poll every 300s and/or fs notify mid-walk
  → mark dirty (today: any request_scan while active)
  → skip delete_missing
  → follow-up full scan
  → more notify during that walk
  → dirty again
  → never quiet
```

Almost every completed job: `added=0 updated=0 removed=0 unchanged=full`.
Still paid multi-minute `index_duration_ms`.

| Lib | Post-39 completed jobs | Typical index | Best | Worst post-catch-up |
|---|---:|---|---:|---:|
| Movies (1749) | ~27 | avg ~6 min | 675 ms (warm, quiet) | ~18 min |
| TV (23099) | ~25 | avg ~8.7 min | ~40 s | ~12 min (700s+) |

Warm-and-quiet is possible (jobs 49 / 72 / 84 Movies at sub-10 s). Most runs
are several minutes even with `relisted_dirs=0` — full-tree **directory
stats** over the media mount, not multi-lib thrash of empty libraries.

**#44 fixed pile-up.** It did not make a 23k-dir SMB stat pass free, and it
did not stop notify from re-dirtying every long walk.

**#45 fixed the mid-scan delete race for path-hint upserts.** Combined with
chatty notify + long walks, treating **every** mid-walk `request_scan` as
dirty made the follow-up path a **permanent rescan treadmill**. The #45
race is **hint-specific** (row outside keep-set); poll-while-active is
collateral from “any request_scan marks dirty,” not the same hazard.

### Design trap rejected for B′

TV warm index ~8.7 min; poll 300 s. Every TV walk is poll-touched mid-flight.

If B′ kept “mid-walk poll → still skip `delete_missing`, no follow-up”:

- every walk longer than the poll interval would **never** run
  `delete_missing`;
- successive polls only start another walk that is dirtied at T+300 s again;
- **worse than the treadmill** (deletes starved forever on large libs).

So mid-walk **poll** must **not** set the dirty bit that suppresses delete.

## Revised residual queue

```text
Shipped: #44 epoch + #45 path-hint
     │
     ▼
 B′  Notify / dirty coalesce (break the treadmill)     ← NEXT
     │
     ▼
 B   deferred_remove holdoff (repoint)                 still needed, rarer
     │
     ▼
 C   Repoint single-walk reuse + WalkCache rekey
     │
     ▼
 E0  Quiet warm-walk measure (walk vs canonicalize vs DB)
     │
     ▼
 D   Epoch-wait log (polish)
     │
     ▼
 E1  Probe budget / further walk opts only if E0 demands
```

Do **not** open adaptive poll, notify-works probes, or force-repoint.

---

### Slice B′ — Notify / dirty coalesce (next code)

**Goal:** Steady-state dogfood can go **idle** under continuous SMB notify
without losing add latency or **delete safety on long walks**.

**Problem statement:** Today `watch` always `hint_ingest` then
`request_scan`; mid-walk that is only `mark_scan_dirty`; end of job always
follow-ups when dirty. Unchanged=full forever is the expected outcome of
that loop on N150.

**Design constraints (ADR-0014 / 0015):**

- Full keep-set walk remains the only path that may `delete_missing`.
- Path-hint still never calls `delete_missing`.
- Mid-scan dirty skip remains for the **hint race only** (`dirty_add`).
- Poll remains the mute-share bound (300 s default).
- No mount-type classification as a gate (ADR-0015 rejected list).

#### Settled policy (end-of-index / mid-walk)

| Mid-walk trigger | Set dirty bit? | Skip `delete_missing`? | Auto follow-up? |
|---|---|---|---|
| **Hint / `dirty_add`** (path-hint upsert while a scan is active) | Yes | **Yes** (keep #45 race fix) | **No** if index unchanged / path absorbed into keep-set or already upserted by hint |
| **Poll** / `request_scan` while active (same library) | **No** (no-op on dirty) | **No** — running walk **is** this poll | **No** |
| **Manual `POST .../scan`** while active | Yes (today’s coalesce) | Yes if dirty at end (existing behaviour) | **Yes** — exactly one follow-up |

Deletes that happen mid-walk **without** a hint wait for the **next quiet
poll walk** (same as mute share). Poll-while-active is **not** “skip
delete, defer heal.”

#### Notify / request_scan split (load-bearing ADR change)

ADR-0015 decision 1 (“every trigger calls `request_scan`”) and decision 5
(“caller should still `request_scan`” after hint) **change**. Spell the
new split in the ADR amend:

| Trigger | Behaviour |
|---|---|
| **Notify media path** | `hint_ingest` only for creates (upsert / ignore / unchanged). **Do not** call `request_scan` after successful hint, Ignored, or Unchanged. Poll heals deletes (≤300 s). |
| **Notify dir / non-media** | No dirty, no scan; poll covers. |
| **Poll** | Sole periodic full-walk entry (`request_scan`). If a scan is already active for that library, **do not** `mark_scan_dirty`. |
| **Manual POST .../scan** | `request_scan`; if active, dirty + one follow-up (unchanged coalesce). |
| **Library create** | Still `request_scan` (unchanged). |

**Highest-leverage code change:** after `hint_ingest`, never auto
`request_scan`. That alone stops most of the SMB noise loop. The
poll-dirty no-op stops delete starvation on walks longer than the poll
interval.

**No 30–60 s coalesce window in v1.** It is a second scheduling policy and
fights “poll-only after hint.” Ship without it.

#### Dirty taxonomy (v1)

Two states only on `LibraryPool` (or equivalent):

- **`dirty_add`** — set when path-hint upserts while a scan is active (and
  optionally when manual scan coalesces — keep existing dirty bit for
  manual).
- **clear** — poll-while-active does not set dirty.

`dirty_delete_suspect`, delayed coalesce, and multi-bit taxonomies stay
**out** of the first PR.

#### End-of-index (implementation sketch)

```text
if dirty_add (hint race):
  skip delete_missing
  log: skip delete (hint dirt); heal on next poll or manual scan
  do not take_scan_dirty follow-up solely for dirty_add if unchanged
else:
  allow delete_missing under existing ADR-0014 reachability rules
  // poll-while-active never set dirty, so long TV walks can still delete

if manual dirty still set (POST coalesce):
  one follow-up request_scan (today)
```

Update the log line that today says **follow-up will heal** when that is
no longer the heal path for poll dirt (heal = next poll / manual).

#### ADR

**Load-bearing**, not a footnote. Amend ADR-0015:

- Decision 1: full-walk entry points are poll, manual scan, library create
  (and repoint’s index) — **not** every notify.
- Decision 5: notify may **stand alone** for creates via `hint_ingest`;
  it does not mandate an immediate walk.
- Mid-walk table above as the dirty / delete / follow-up contract.
- Cross-ref ADR-0014: `delete_missing` still only on a full walk that was
  not hint-dirtied; poll-while-active does not suppress it.

#### Tests

- Mid-walk **poll** (or synthetic `request_scan` while active) does **not**
  set dirty; completed walk **still runs `delete_missing`** when a file was
  removed from disk before/during the walk and there was **no** hint dirt.
  Successive polls must not suppress deletes forever (walk longer than poll
  interval — synthetic time or forced mid-walk poll).
- Mid-walk **hint upsert** → skip `delete_missing`; no automatic follow-up
  when path absorbed / unchanged keep-set story holds.
- After `hint_ingest` (Upserted / Unchanged / Ignored), watcher does **not**
  call `request_scan`.
- Manual POST while active still dirty-coalesces to one follow-up.
- Real add via full walk still enqueues probe; hint never calls
  `delete_missing`.

#### Out of scope for B′

Changing poll interval; subtree refresh; repoint; probe concurrency;
30–60 s quiet window; `dirty_delete_suspect`.

#### Success metric (dogfood)

After deploy: multi-minute stretches with **no** active scan under normal
media noise; long TV walks can still `removed > 0` when files are gone;
new media appears via hint without a forced full walk; unchanged full-library
indexes are rare outside the poll cadence.

---

### Slice B — Post-repoint `deferred_remove` holdoff

Unchanged intent. Rarer than B′; keep after B′. Real Gate 3 hazard (auto
scan ~9 s after repoint applied deferred deletes) but not why the box never
idles.

- On repoint complete with `deferred_remove > 0`, hold off poll and automatic
  dirty follow-up deletes (in-memory holdoff OK for v1).
- Manual `POST .../scan` still allowed.
- Clear on ordinary scan complete or fixed duration (~1 h).
- Gate 3 note already flagged this.

---

### Slice C — Repoint single-walk reuse + WalkCache rekey

Unchanged. One cold readdir for retain + commit index; clear and reseed
WalkCache for new absolute paths. ADR-0030 amend: second walk not required
when dry-run list is reused in the same epoch.

---

### Slice E0 — Quiet warm-walk measure

Only after B′ so the box can be quiet. Instrument one Movies (and one TV)
unchanged poll: wall time for dir stats vs canonicalize vs DB match.
Write `notes/scan-warm-breakdown-YYYY-MM-DD.md`. No product change unless
numbers show a clear hotspot.

---

### Slice D — Epoch-wait observability

Log when `enter_index_epoch` blocks longer than ~5 s (library_id / job).
Optional API state later. Polish only.

---

### Slice E1 — Only if E0 demands

Global probe budget or canonicalize reduction. Not before quiet measure.

---

## Delivery order and PR shape

| Order | Work | PR area |
|---:|---|---|
| 0 | #44 + #45 shipped + N150 deploy | done |
| 1 | **B′ notify/dirty coalesce** + ADR-0015 (load-bearing) | done (#46) |
| 2 | B deferred_remove holdoff | `scanner/repoint-deferred-holdoff` |
| 3 | C repoint walk reuse / cache rekey | `scanner/` + ADR-0030 |
| 4 | E0 warm-walk measure note | done (`notes/scan-warm-breakdown-2026-08-03.md`) |
| 5 | D epoch-wait log | `scanner/` |
| 6 | E1 | only with new timings |

One concern per PR. **B′ is the first implementation prompt** — only after
this note’s poll-dirt rule (no skip-delete for poll).

## Explicit non-goals

- Adaptive poll intervals / confidence scoring / notify-works verification
  (ADR-0015 rejected).
- Mount-type classification as a gate.
- Force-repoint; doctor UI counters (Phase 4 / ASS).
- Tuning 0.90 retain without remount evidence under threshold.
- Stopping dirty-skip for **mid-scan hints** (race returns).
- Treating mid-walk poll as dirty that suppresses `delete_missing`.

## Open questions — settled

| # | Question | Answer |
|---|---|---|
| 1 | After successful media hint, poll-only delete latency (≤300 s)? | **Yes.** No immediate `request_scan`; no delayed coalesce in v1. |
| 2 | Mid-walk poll: skip delete? follow-up? | **No / no.** Poll-while-active is a dirty no-op; running walk may delete. |
| 3 | Mass delete / rename without notify? | **Poll only** (mute-share model). |

## Related notes

- `notes/gate3-repoint-dogfood-2026-08-03.md` — remount + deferred_remove
- `notes/migration-012-dogfood-2026-08-03.md` — live migrate
- N150 run: jobs 39–90+ in dogfood DB; container `nightjar-dogfood`
