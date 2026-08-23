# Item 4, slice 1 — the seam, unwired

The board said to scope this and not start it inside an iteration. Scoped in
note 04; this is slice 1 of the three that note named, landed on its own.

## What landed

**`nightjar_db::season_number_for_path(stored, library_root) -> Option<u32>`.**
The same walk `show_folder_relpath` and `under_numbered_season_directory`
already make, asking a third question: *which number*. It lives beside them
because a second predicate about the same naming convention written elsewhere is
the reimplemented-`norm_key` trap — that one reported 25 non-folding folders
where the shipped chain gave 12.

**`nightjar_core::parse_filename_in(file_name, FolderContext)`.** Two rules:

1. An **empty** title takes the folder's name — `stored_title` moved down,
   unchanged.
2. An **episode** with no season number takes the folder's.

**Nothing calls either.** Verified by grep: the only references outside the two
defining files are the two `pub use` lines.

## The crate graph decided the API shape

`nightjar-core` has no internal dependencies, and `nightjar-db` — a sibling, not
a parent — owns the path walk. So core **cannot** call `show_folder_relpath`,
and a context-taking parser cannot walk a path itself without reimplementing
db's rules inside core.

Hence `FolderContext`: the caller, which is the layer that owns path semantics,
passes down the two things a basename cannot carry. This was not a preference;
it was the only shape the dependency direction allows.

## The finding that changes slice 2

My own scope note assumed the fill-the-silence rules would carry the two big
shapes. **They do not, and the reason is worth having before slice 2 starts.**
Measured through the shipped chain rather than assumed:

| basename | kind | parsed title | season |
|---|---|---|---|
| `Season 01/Episode 1.mkv` | `Movie` | `"Episode 1"` | `None` |
| `Season 1/01 - Closure.mkv` | `Movie` | `"01 - Closure"` | `None` |

Two things follow.

**Both parse to a non-empty title.** So rule 1 never fires: the folder would
have to *override* a title the basename asserted, which is a different rule from
filling a silence.

**Both parse to `Movie`.** So rule 2 never fires either — it is gated on the
parsed kind, deliberately, so that a film never gains a season.
`Futurama/Season 5/Futurama Bender's Big Score (2007).avi` is a real film in a
real library, and `stored_kind` correctly keeps it one because it carries its
own year.

**So both rules wait on the kind, and the kind is decided a layer up.** That is
the knot note 04 predicted in the abstract, now concrete: slice 2 is not "move a
call site". It is *decide which layer owns the kind*, and only then move.

Two shapes, 11,684 oracle rows, both at 0.0% correct, both blocked on that one
decision.

## A guard whose two halves must not be made to agree

`under_numbered_season_directory` and `season_number_for_path` walk the same
tail and **disagree on purpose for exactly one path**:

    Show/Season 99999999999999999999/x.mkv
      under_numbered_season_directory -> true    a file here is not a film
      season_number_for_path          -> None    and that is not a season number

`season_directory_number` takes `u32::MAX` for an over-wide run so a segment
does not stop being a season directory at sixteen digits — right for the
predicate, useless as a number. A test asserts both halves, so nobody
"tidies" one into the other and quietly changes what a file in that folder may
bind to.

## Measured

| instrument | before | after | why |
|---|---|---|---|
| parser corpus | 535 / 738 (72.5%) | **535 / 738 (72.5%)** | identical |
| parser sweep, `HEAD~1..HEAD` | 74,624 names | **0 differ, 0 gained, 0 lost** | `parse_filename` untouched over 74,624 names |
| `cargo test -p nightjar-core` | 111 | **118 pass, 0 failed** | 7 new |
| `cargo test -p nightjar-db` | 68 | **72 pass, 0 failed** | 4 new |
| matcher oracle | — | **not run** | see below |
| dogfood strict pair | — | **not run** | see below |

**The oracle and the dogfood pair were not run, and no zero is claimed from
them.** This diff adds two functions and two `pub use` lines and changes no
production code path — grep confirms no caller. Draining 90,072 rows to watch a
function nobody calls do nothing is the insensitive-by-construction zero the
board warns about, and reporting it as a clean run would be the error this loop
exists to avoid. The sweep is the sensitive instrument here, it ran against this
commit's own parent, and it reads 0 of 74,624.

## The property that makes slice 2 safe

    parse_filename_in(name, FolderContext::default()) == parse_filename(name)

Asserted directly over ten names including the empty string. A call site with no
folder to offer gets exactly what it gets today, which is what lets slice 2 move
one site at a time and attribute anything that moves.

## The transcode flake, characterised properly this time

`nightjar-transcode`'s `hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac`
failed in this slice's workspace run, as it has intermittently all loop.

**The argument used for iteration 1 does not work here, and reusing it would
have been wrong.** There, the reasoning was that `nightjar-transcode` does not
depend on `nightjar-metadata`, so the change could not reach it. True then. This
slice touches `nightjar-core` **and** `nightjar-db`, and transcode depends on
both. So it was re-derived rather than recalled.

**It is not this slice**, on three independent grounds:

1. **It fails on both sides.** `HEAD~1`: ok, ok, FAILED. `HEAD`: ok, FAILED,
   FAILED. Same test, same machine, minutes apart.
2. **There is no mechanism.** Everything transcode uses from the two crates:
   `nightjar_core::VideoEncodePlan`, `nightjar_db::content_id_for_path`,
   `nightjar_db::Db`, `nightjar_db::open`. None is touched. And the sweep proves
   `parse_filename` is byte-identical over 74,624 names.
3. **It fails alone**, not only under parallel load, so it is not test
   interference either.

**What it actually is.** The test reads a **727 MB file over an SMB network
mount** — `//GUEST:@RM400._smb._tcp.local/media` — probes it with ffmpeg, and
asserts where the video lands:

    video starts at 63.646s, land was 58.975s

A keyframe-landing assertion over a network filesystem. It skips when the mount
is absent, so it does not fail in that case; it fails when the mount is *present
and slow*, which is the worst of the three states because it looks like a code
result.

**It should not be counted as a regression signal by anyone**, and the README's
"single known-flaky failure" undersells it: the flakiness has a cause, and the
cause is that the test depends on a network mount it does not control. Naming
that is cheaper than re-deriving it every loop, which is now twice.
