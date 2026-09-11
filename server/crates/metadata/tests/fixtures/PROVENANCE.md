# PROVENANCE — metadata fixture replacements

## Origin

On 2026-09-11 the items listed below replaced fixture content that carried
real film/television titles, character names and plot text. Every replacement
is original synthetic Nightjar test material. Nothing is copied or adapted
from a real provider response, a real catalogue entry, a real person's name,
an external corpus, or a live network call. All titles, people, roles, plots,
genres, identifiers and collection names are fictional.

The replacements keep every parser, error, normalization, Unicode and fold
transformation the originals exercised. Only the dependent literal
assertions moved with them.

## License

The replacements are GPL-3.0-only, matching the product `LICENSE`. XML files
carry no SPDX header because the parser fixtures must stay byte-minimal for
their error cases; this register is the license record for all four items.

## Item-level register

| Item | Role | Transformation / parser surface kept | SHA-256 |
|------|------|--------------------------------------|---------|
| `movie.nfo` | `parse_nfo` movie path; id extraction, cast, collection, ratings, artwork | `<movie>` root, `uniqueid`/`tmdbid` fallback, `<actor>`, `<set>`, `<ratings>`, `<thumb>`, `<fanart>` | `d0dfccd84b15cdd43218919198901c32eedbf17c6f598e68e19634821e0c32dc` |
| `episode.nfo` | `parse_nfo` episode path; episode id extraction | `<episodedetails>` root, `season`/`episode`, `uniqueid` type `tmdb`/`tvdb` | `6127c145b8f2bc7b978d1fc2bea511af0c8489664a7cef10543ee82afe176334` |
| `malformed.nfo` | `parse_nfo` error path (`NfoError::Malformed`) | truncated, never-closed tags under a `<movie>` root | `a74f5d9abcd45aaa18967cadb948280e21a0b33da39e1cd72ef9f5635c092691` |
| `fold_corpus.json` | `fold_title_orthography` corpus rows | ampersand, ASCII + U+2019 apostrophes, colon, acute/grave accents, hyphen, superscript ², Ł/ó/ź, Č/Š, precomposed + combining macron, ASCII identity | `fceeb7145869ef0d59a70d4ee736ce6ce10f4389ced3e46de032e093a8388f8b` |

Hashes cover the checked-in bytes of each replacement at authoring time.

## Scope

This register covers only the four items above. Unrelated inline test data in
the metadata crate that still names real titles is outside this slice and is
not covered here.
