# Gate 3 repoint dogfood — 2026-08-03

ADR-0030 remount proof against live dogfood after migration 012.
Binary: `main` @ `18ae3c1` (#43). Data: `~/nightjar-data`.

## Setup

Movies library (id=2) was `/Volumes/media/Movies` (SMB
`//GUEST:@RM400…/media`). macOS would not accept a second mount of the same
share under that name; remount used a distinct host path that survives
`canonicalize`:

```text
mount_smbfs -o soft,noowners '//guest:@192.168.1.2/media' \
  /Users/gmacarthur/mnt/media-gate3
```

Old root `/Volumes/media` dropped during the dual-mount attempt (share only
visible at the new path). That is a real remount: same tree, different
absolute root string. Symlink-only candidates are invalid for this gate
(product canonicalize collapses them).

## Baseline (before PATCH)

Library 2: 1773 items, path `/Volumes/media/Movies`.

| id | probe | subtitle | links | `{DATA}/subs/{id}/` |
|---:|---|---|---:|---|
| 32 | probed | ready | 0 | e2.vtt, e3.vtt |
| 33 | probed | ready | 0 | e2.vtt |
| 34 | probed | ready | 0 | e2.vtt |
| 35 | probed | error | 0 | (dir empty) |
| 36 | probed | error | 0 | (dir empty) |

Metadata links were still pending on this sample (`link_count=0`); survival
check is “still zero / same rows,” not a successful match bind.

## Repoint job

```text
PATCH /api/v0/libraries/2
{"path":"/Users/gmacarthur/mnt/media-gate3/Movies"}
→ 202  jobId=7704
```

| Field | Value |
|---|---|
| kind | repoint |
| state | completed |
| candidatePath | `/Users/gmacarthur/mnt/media-gate3/Movies` |
| unchanged | 1748 |
| added / updated / removed | 0 / 0 / 0 |
| deferredRemove | 25 |
| skippedOutsideRoot | 0 |
| indexDurationMs | 63191 |
| library_id after | 2 (unchanged) |
| libraries.path after | `/Users/gmacarthur/mnt/media-gate3/Movies` |

Retain on the index keep-set: 1748 / 1773 ≈ **0.986** (≥ 0.90). Path
committed; first index deferred `delete_missing` as specified.

### deferred_remove = 25

Not a clean zero. Composition (from pre-012 backup ids missing after the
follow-up scan):

- 24 × `Movies/dolby-vision-browser-kit/...` Patterns kit files still keyed
  under Movies after the kit was moved to the DV library root (see CONTINUITY
  measure-exclude / Patterns move). Not present under the Movies walk.
- 1 × `E.T. The Extra-Terrestrial (1982)/…mkv` (id 383) — absent from the
  walked tree at repoint time.

None of the five baseline ids were in this set.

### Auto scan after repoint (ops note)

ADR expects operators to review `deferred_remove` before the next ordinary
`kind=scan`. Poll requested a scan ~9 s after job 7704 completed
(`job_id=7706`, `removed=25`). The 25 unmatched rows were then deleted.
Gate 3 id-survival for matched rows still holds; the review window is too
short under default poll. Product follow-up candidate (not done here):
suppress poll-driven scan until deferred_remove is acknowledged, or surface
the counter in doctor/UI (Phase 4).

## Survival (baseline sample)

After repoint (and after the auto scan that dropped the 25):

| id | id same | probe | subtitle | subs files | open via resolve | API path root |
|---:|---|---|---|---|---|---|
| 32 | yes | probed | ready | e2, e3 | yes | `…/mnt/media-gate3/Movies/…` |
| 33 | yes | probed | ready | e2 | yes | same root |
| 34 | yes | probed | ready | e2 | yes | same root |
| 35 | yes | probed | error | empty | yes | same root |
| 36 | yes | probed | error | empty | yes | same root |

`GET /api/v0/items/32/playback-info`: `itemId=32`, `durationMs=5704576`,
`subtitleStatus=ready`, 2 soft tracks. `MediaItem.path` absolute form
changed with the library root; id did not.

Item count Movies: 1773 → 1748 (only the deferred 25, via the follow-up
scan — not via the repoint job itself).

## Refuse smoke

```text
PATCH /api/v0/libraries/2
{"path":"/Users/gmacarthur/mnt/empty-repoint-smoke"}
→ 202  jobId=7711
```

| Field | Value |
|---|---|
| state | failed |
| error | `repoint_empty_match: current=1748 walked=0 matched=0 would_remove=1748` |
| libraries.path | unchanged (`…/media-gate3/Movies`) |
| media_items count | unchanged (1748) |

## Household recovery (same session)

`/Volumes/media` did not come back without elevated mkdir. Other libraries
were repointed onto the working gate3 mount:

| lib | job | result |
|---|---:|---|
| DV (4) | 7716 | completed, unchanged=16, deferredRemove=0 |
| DV2 (5) | 7717 | completed, unchanged=3, deferredRemove=0 |
| Shows (3) | 7720 | path committed to `…/media-gate3/TV Shows` after retain;
  first index still walking ~23k items over SMB when this note closed
  (count still 23099; Movies Gate 3 does not depend on it) |

Working library roots after recovery (all on the gate3 SMB mount except
Test Data): Movies, Shows, DV, DV2.

## Verdict

Gate 3 remount criterion for ADR-0030: **pass** on Movies. Same
`library_id`, same media item ids for the matched tree, probe and extract
state preserved, playback opens via resolved path, refuse guard works.
`deferred_remove=25` was tree churn (DV kit under Movies + one missing
title), not a wrong-root wipe; retain stayed above 0.90. No reason to
revisit the 0.90 default from this remount.

Retain default revisit trigger (ADR): not fired.
