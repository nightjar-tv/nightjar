# Iteration 2 prediction, written before any measurement run

| shape | prediction | why |
|---|---|---|
| tv.mixedroot, root half (2,885 rows) | moves | the only rows whose group has no show folder AND whose basenames carry episode titles |
| tv.mixedroot, foldered half (2,759) | 0 | non-empty show folder, unchanged read |
| tv.root (5,644) | **0** | root groups, but basenames are `Show.S01E01.1080p.WEB-DL.mkv` — no episode title, so `usable_episode_titles` was already empty |
| every other shape | 0 | every file sits under a non-empty show folder |
| dogfood pair | 0 | the real library has no root-level episode file |
| corpus / sweep | insensitive | `nightjar-core` untouched |

Direction inside tv.mixedroot's root half, at base: 2,806 correct, 53 absent,
12 stalled, 6 wrong.entity, 8 wrong.unknownepisode.

- the 14 wrong rows are the target; expect them to fall
- **expect some correct -> absent too.** Confirmation is promotion evidence that
  lifts a 0.72 pick to 0.90. Removing a neighbour's titles can drop a binding
  that is right back below the floor, and the loop's own rule says a trade of
  wrong for absent reads as better, not worse.
- if tv.root moves, the model above is wrong and the result is not readable.
