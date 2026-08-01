# Library change detection

Status: open brief (no decision). Date: 2026-07-27.

How Nightjar learns that files appeared, changed, or vanished under a library
root, why the present arrangement fails users, what the warm-walk measurements
rule out, and which shapes of fix are on the table. This note is
self-contained. It does not recommend a path.

Related: [ADR-0004](adr/0004-async-scan-jobs.md) (async scan jobs),
[ADR-0013](adr/0013-subtitle-extraction-at-scan.md) (watcher + walk cache),
[ADR-0014](adr/0014-library-availability.md) (reachability). Engineering
constraints: [ENGINEERING_RULES.md](../ENGINEERING_RULES.md) Rules 4.7, 4.8,
4.10.

The code excerpts describe the arrangement that produced the dogfood findings
(scanner after ADR-0013 / availability work, before later WIP on the same
problem). Paths: `server/crates/scanner/src/watch.rs`,
`server/crates/scanner/src/pool.rs`, `server/crates/api/src/routes/libraries.rs`.

---

## 1. The problem

Nightjar indexes each library with an async scan job (walk, upsert, probe,
subtitle extract). Something must start those jobs when the tree changes.

Two mechanisms exist today:

1. **Filesystem notify** (`notify` + a 2 s debounce). After the first index
   pass finishes (`last_index_duration_ms > 0`), the watcher arms recursive
   watches on every library root. Create/modify/delete events call
   `start_scan_job`.
2. **Periodic poll.** On a fixed cadence (intended as a fallback), every
   watched library gets `start_scan_job` again. The walk uses a
   directory-mtime cache: warm passes re-stat known directories and readdir
   only when a directory’s mtime moved.

Both run in one loop in `run_with_notify`. After notify arms, `maybe_poll`
still runs every cycle against the same set of roots. There is no gate that
says “this root is covered by notify; stop polling it.”

Two concrete failures follow.

### Adding a library does not start a scan

`POST /api/v0/libraries` inserts the row and returns 201. It does not call
`start_scan_job`. The new root is picked up when the watcher next syncs and
the next poll tick fires. Dogfood: library added at 21:40:12, first scan at
21:40:52, about 40 s of wait for a 60 s cycle. That latency is the first
thing a new user sees. Nothing is broken; nothing is useful either.

(Scan jobs themselves are already async per ADR-0004: enqueue and return.
Blocking the POST on a cold SMB walk would be worse. The missing piece is
enqueue-on-create, not a synchronous walk in the request.)

### Poll never becomes a fallback

Once notify is armed, poll continues forever. On a small local library that
is free but redundant: six jobs in a row with `unchanged` equal to the whole
library and `index_duration_ms` of 1–2 ms. On the household SMB libraries it
is constant NAS traffic for the life of the process. Notify had already been
logged as armed at 21:40:57 on that same session; from then on the 60 s poll
was a second full mechanism doing the same job.

So the arrangement is wrong in two directions at once: too slow to start
work the user just asked for, and too eager to keep walking forever where
another mechanism was supposed to take over.

---

## 2. Why notify alone is not an option

Kernel filesystem watches (`inotify`, FSEvents, ReadDirectoryChangesW, and
whatever `notify` selects on the host) are local-filesystem APIs. Network
file systems often accept a recursive watch without error and then never
deliver create events.

Dogfood on macOS SMB (`/Volumes/media/...`):

- The watcher logged `watching library` / armed recursive FS notify for both
  Movies and TV Shows.
- Creates under those roots did not produce watch-driven scan jobs.
- Discovery fell back to poll. When poll was the only working path, a newly
  added show could sit unseen for a long time if attention was elsewhere:
  hours in one case when the operator expected notify to have caught it.

ADR-0013 also records a counter-example on the same household mount: one
nested create did produce a watch event in ~2.4 s. That is not a
contradiction of the failure mode. It means SMB notify is unreliable, not
uniformly absent. “Armed successfully” and “will deliver the next create”
are different predicates. Gating “stop polling” on the former disables the
only mechanism that worked when the latter is false.

The same class of silent miss shows up anywhere the kernel watch is not the
real store:

| Layer | Typical failure |
|---|---|
| SMB / NFS / WebDAV / SSHFS | Watch arms; creates missed or delayed |
| FUSE (rclone, sshfs, …) | Events incomplete or absent |
| Union / merger layers (mergerfs, Unraid user shares) | Events on one branch, not the merged view |
| Docker Desktop file sharing (macOS / Windows) | Host↔VM filesystem layer drops or delays watches |
| Some NAS “local” paths that are really network | Same as SMB underneath |

Notify-only would be correct on a quiet local APFS/ext4 disk and wrong for a
large share of real Nightjar installs (NAS first, Docker second).

---

## 3. Why polling alone is not free

Poll cost is a warm (or cold) tree walk over the share. Measurements below
are from a MacBook Air on Wi-Fi (802.11ax) to the household SMB share,
release-built walk code matching the scanner algorithm, Nightjar process
stopped during idle runs so extract/index did not contend.

| Library | Files | Directories |
|---|---:|---:|
| Movies | 1,753 | 1,769 |
| TV Shows | 23,062 | 3,046 |

Warm walk = walk cache already filled; zero directory mtimes changed
(`relisted=0`). Cold after remount = empty walk cache immediately after a
fresh SMB mount (proxy for process restart / cold client metadata cache).

| Library | Condition | Timings (s) | min / med / max |
|---|---|---|---|
| Movies | idle warm | 0.084, 7.941, 12.925, 12.222, 12.412, 14.931 | 0.08 / 12.3 / 14.9 |
| TV | idle warm | 21.292, 22.391, 21.683, 21.506, 21.484, 22.180 | 21.3 / 21.6 / 22.4 |
| Movies | warm, during ffmpeg | 16.585, 16.476, 17.494, 16.820, 18.711, 20.176 | 16.5 / 17.2 / 20.2 |
| TV | warm, during ffmpeg | 22.460, 20.807, 21.502, 20.923, 20.926, 22.105 | 20.8 / 21.2 / 22.5 |
| Movies | cold, after remount | 120.7, 115.7, 129.2, 136.0, 124.1, 127.5 | 115.7 / 125.8 / 136.0 |
| TV | cold, after remount (n=3) | 723.7, 728.8, 686.7 | 686.7 / 723.7 / 728.8 |

Caveats:

- Movies idle 0.084 s is a hot-cache artifact immediately after the priming
  cold walk; later idle warms sit in the 8–15 s band. Upper end, not mean,
  is what an interval must respect.
- “During ffmpeg” means a software x264 session reading an ~8 GB title from
  the same SMB share. ffmpeg exited before the suite finished, so Movies
  warms were under load; some TV warms may not have been.
- TV cold stopped at three runs: each remount required a manual macOS Guest
  Connect click, and the three results were already consistent (~11–12 min).
- Numbers are SMB-over-Wi-Fi from one MacBook Air. They do not generalise to
  local disk or ethernet.

Cost drivers (separately):

- **Warm walk** cost tracks **directory count** (re-stat every known dir).
  TV has ~1.7× Movies dirs and ~1.5× Movies upper warm time.
- **Cold walk** cost tracks **file count** (readdir + file stat). TV has
  ~13× Movies files and ~5.6× Movies cold time.

What these rule out:

- A **60 s** poll against TV Shows, with ~22 s warm walks, means the NAS is
  walking on the order of **a third of all wall time**, permanently, even
  when nothing changes. That is not a light heartbeat.
- Deriving the interval from “2 × last warm duration” with a **60 s floor**
  never engages the 2× term on these warm numbers (see §4). The floor is the
  policy; 60 s is too aggressive for TV on this link.

Encouraging result: warm walks during an active transcode read were only
modestly slower than idle (Movies ~15 s → ~20 s upper; TV stayed ~22 s).
Polling does not appear to thrash playback the way a concurrent cold index
plus extract did in earlier ADR-0013 dogfood. Pausing poll during playback
is still an open product question (§8), not forced by these numbers.

---

## 4. The current code

### Poll interval formula

```rust
pub fn poll_interval(&self) -> Duration {
    let ms = self.last_index_duration_ms();
    let secs = (ms.saturating_mul(2) / 1000).max(60);
    Duration::from_secs(secs)
}
```

`max(60, 2 × duration_s)` exceeds 60 only when the last index walk took
**≥ 30 s**. Every warm case in §3 is below that. The floor is the real
policy for steady state; the 2× term only stretched the interval after cold
Movies/TV indexes (e.g. 111 s cold → ~223 s). A formula that never changes
the answer for the common case invites someone to trust it as adaptive when
it is not.

### Scan on create

`POST /libraries` creates the library row and returns. It does not call
`start_scan_job`. The natural place is immediately after a successful
`create_library`, fire-and-forget via the same async accept pattern as
`POST .../scan` (ADR-0004): enqueue the job, return 201 with the library
DTO, do not wait for the walk.

### Poll gating

In `run_with_notify`, after notify arms, the loop still calls
`maybe_poll(...)` every ~5 s recv timeout against the full watched set.
Arming is gated on `last_index_duration_ms() > 0` (defer recursive watches
during the first cold walk so they do not starve SMB metadata IOPS). That
gate does not distinguish local from network, and it does not stop poll
after arm. `NIGHTJAR_POLL_ONLY=1` disables notify entirely and polls
everything, useful for shares where notify is known bad, but not the
default path that dogfood hit.

---

## 5. Environments that must work

Nightjar is dogfooded on a Mac laptop and intended to run on a Raspberry Pi,
an Unraid/NAS box, and similar home servers. Library roots are user-chosen
paths. Any detection design has to be honest across:

| Environment | Notify | Mtime / warm poll | Poll harm |
|---|---|---|---|
| Local disk (APFS, ext4, …) | Usually works | Works | Wasteful if also notify, cheap in absolute terms |
| External USB / Thunderbolt | Usually works when spun up | Works; **spin-up can take 5–15 s** on first stat | A walk that blocks on spin-up looks like hang; also collides with the **5 s** reachability `is_dir` timeout (ADR-0014) |
| SMB / NFS / SSHFS / WebDAV | Unreliable or silent miss | Works if server bumps dir mtime on create (SMB sometimes does not; see ADR-0013) | Can be expensive (measured) |
| iSCSI / network block device | Looks **local** to the OS; notify often works | Works | Classifying as “network” by path/transport is wrong |
| FUSE / mergerfs / Unraid user shares | Often broken or partial | Depends on layer | Poll may be the only truth; classification by fstype is a table that rots |
| rclone / cloud mounts | Poor | Listings cost **API quota** / rate limits | Polling is actively harmful, not just wasteful |
| Docker bind mounts / Desktop VM shares | Flaky | Usually ok | Same as notify unreliability |
| Symlink farms (*arr) | Events on link vs target vary | Walk must not loop (already handled) | Extra dirs inflate warm cost |
| Read-only mounts | Notify may work | Works | Verification that writes a probe file is impossible |
| exFAT and 2 s mtime granularity | Notify may work | Rapid create+create in the same 2 s window can look unchanged | Warm poll can miss a beat; restart/cold still heals |

“Network path detection” (e.g. `MNT_LOCAL`, mount fstype lists) helps SMB on
macOS and fails open or closed on iSCSI, mergerfs, and Docker. Verification
by writing a tempfile fails on read-only roots and violates the project’s
stance that library trees are not Nightjar’s to write.

---

## 6. Options

Presented without a preferred winner. Each can be combined with
**scan-on-create** (async `start_scan_job` after `POST /libraries`), which
fixes the new-user latency independently of how steady-state detection
works.

### A. Always poll; notify only hints to poll sooner

One detection story: the source of truth is the periodic (or hinted) walk.
Notify, where it fires, only advances the next poll or starts a job
immediately; missing notify never loses creates.

- **Pros:** Works everywhere by construction. No classification. Matches
  “one concept, one path” for detection. Docs can state a clear latency
  bound per library size / link.
- **Cons:** Detection latency on a large SMB library is **minutes** if the
  interval is set from upper warm cost (e.g. several × ~22 s for TV).
  Interval should be **per-library** (Movies and TV differ). Permanent NAS
  traffic at whatever interval is chosen. Notify code becomes optional
  optimisation, which tempts Rule 4.7 thrash if left half-wired.

### B. Notify primary; poll fallback gated on classification

Keep notify for roots classified local; poll forever for roots classified
network (fstype / `MNT_LOCAL` / path heuristics). Optionally never arm
notify on network roots.

- **Pros:** Local disks stay cheap and fast. Measured SMB cost only hits
  shares that need it. Close to ADR-0013’s intent (“poll is the fallback”).
- **Cons:** Classification is a pattern table. **iSCSI** looks local.
  **mergerfs / Unraid** look odd. Docker Desktop looks local inside the VM.
  Wrong “local” → stop polling → silent miss. Wrong “network” → pay poll
  forever on a disk that notify would have covered. Rule 4.7 pressure to
  grow the table forever.

### C. Notify primary; poll fallback gated on verification

On library add (or first arm), write a tempfile under the library root and
see whether a notify event arrives within a deadline. If yes, treat notify
as trustworthy and suppress steady-state poll (perhaps keep a very slow
safety poll). If no, poll-only for that root.

- **Pros:** Reflects reality instead of mount folklore. Handles “armed but
  mute” SMB.
- **Cons:** Extra mechanism. **Writes into the user’s library tree**, which
  Nightjar otherwise treats as read-only. Fails on **read-only** mounts.
  Probe file may be scooped by *arr watch folders, scanners, or antivirus.
  Still need a policy when verification is inconclusive.

### D. Poll only; delete notify

Ship one mechanism. `NIGHTJAR_POLL_ONLY` becomes the only mode.

- **Pros:** Smallest design. No dual-path bugs. Forced honesty in docs about
  latency.
- **Cons:** Gives up sub-second local detection that notify already delivers
  on real disks. On large NAS libraries, same interval tension as A.

### E. Do less

Examples, not a full design:

- Fix **scan-on-create** only; leave steady-state poll+notify as-is for a
  later brief (risks Rule 4.8 if everyone knows the dual path is wrong).
- Document “run `POST .../scan` after copying files” and stop promising
  automatic discovery on network shares.
- Lengthen a global poll to many minutes and accept slow NAS discovery
  without per-library logic.

Each “do less” option must still say what “a file added to a library
appears quickly” means in the product copy, or stop saying it.

### Interval policy (orthogonal, but coupled)

Whatever uses poll must choose an interval. Evidence so far:

- Global **60 s** is too hot for TV-sized trees on SMB-over-Wi-Fi.
- **Per-library** intervals match the measured gap between Movies and TV.
- A **fixed** interval with a comment beats a decorative adaptive formula.
- Safety poll on “notify trusted” roots (very slow) is an optional extra if
  notify-primary wins; it is not free on rclone.

---

## 7. Constraints any answer must respect

- **Rule 4.7 (no speculative abstraction).** No trait soup or config matrix
  for hypothetical mount types. Abstract when a second concrete case
  demands it, not when the table of fstypes gets long.
- **Rule 4.10 (one concept, one path).** “Library tree changed” should not
  remain two competing implementations that both fire forever. If notify
  and poll coexist, the ADR must say which is source of truth and which is
  hint or fallback, and the code must match that sentence.
- **Rule 4.8 (incomplete, never provisional).** A slice may omit notify on
  network shares, or omit fast local notify, but it must not ship
  “poll+notify both always on, we will tighten later” as the architecture.
- **Library directories are read-only** to Nightjar. No marker files, no
  `.nightjar` droppings, unless an explicit product decision overturns that
  (and read-only mounts still exist).
- **Product property:** a file added under a library root appears in the
  library without a manual ritual. Docs must be honest about what
  “quickly” means on local disk vs large SMB vs API-backed mounts.
- **Defensible targets:** Raspberry Pi (slow CPU, often USB or SMB), Unraid
  (FUSE/merger/user shares), Mac over Wi-Fi (measured). A design that only
  works on a quiet local NVMe is not enough.

---

## 8. Open questions (named, not answered)

1. Should polling **pause during playback / transcode**? Measured warm
   impact was modest; earlier cold-index+extract contention was not. Are
   we optimising for NAS idle duty cycle or for worst-case interactive
   playback?
2. What is **first poll after startup** when a cold TV walk is ~12 minutes?
   Block other libraries? Overlap? Cap concurrency? Show progress only?
3. Is **detection latency of minutes** on large network libraries
   acceptable, or does the product need a different approach (user-triggered
   scan, webhook from *arr, inotify on a local landing directory that
   renames onto the NAS, etc.)?
4. Is a **slow safety poll** on notify-trusted roots required (missed local
   event is otherwise permanent until restart), and what interval does not
   recreate the TV duty-cycle problem on a misclassified root?
5. How should **USB spin-up** interact with the 5 s reachability timeout so
   a sleeping drive is not marked unreachable and paused forever?
6. Does **scan-on-create** return only the library DTO (201) or also a
   `jobId` (closer to scan’s 202)? API shape is additive either way within
   v0, but clients differ.
7. Should **interval** be derived from measured warm duration per library,
   a fixed tier table, or operator config? (Config is a Rule 4.7 tripwire.)

---

## Appendix: prior ADR numbers (context only)

ADR-0013 (2026-07-26) reported Movies cold walks 73–152 s and warm polls
0.02–11 s (0 dirs changed) on the same household share, and a TV `find`
alone ~110 s. The 2026-07-27 table in §3 is the first paired Movies/TV warm
measurement with directory counts and playback/remount conditions. Prefer
§3 for interval decisions; ADR-0013 remains useful for walk-cache design
history.
