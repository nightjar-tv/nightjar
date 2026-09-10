# PROVENANCE - nfo_identity fixture corpus

## Origin

All files in this corpus were authored fresh for the `nfo_identity`
fixture. Nothing is copied or adapted from a real provider response, a
real catalogue entry, a real person's name or personal data, an external
corpus, or a live network call.

Every title, plot, genre, character, actor name, studio fact, rating,
image path, identifier, and JSON field is original synthetic material.
The identifier blocks are fictional and are not real provider records:

- TMDB movie ids 9200001-9200043 and show ids 9300005-9300006
- IMDb ids tt9200001-tt9200043 and tt9300005-tt9300006
- TVDB ids 8200001-8200043 and 8300005-8300006

## License

The corpus is GPL-3.0-only. This matches the product LICENSE. Each NFO
file carries an SPDX-License-Identifier header. JSON files carry no
comments, so the license is recorded here and in manifest.json instead.

## Withholding and adjudication

The historical author stage withheld `expected.json` and every pass/fail
assertion. A separate adjudicator owns all verdicts about identity
binding, conflict resolution, fallback, rejection, and abstention. The
checked-in `expected.json` is that later independent adjudication
artifact, not an author artifact. The manifest still records evidence and
route inputs only; it never asserts an outcome.

## Authoring constraints honored

- Author-owned artifacts only: xml inputs, provider responses,
  manifest.json, README.md, PROVENANCE.md. The author stage wrote no
  expected.json; the adjudicator added it later as a separate artifact.
- No tests, source edits, commits, or reports at authoring time.
- No provider or network calls. No builds or corpus runs.
- All existing dirty files in the checkout were preserved untouched.
- NFO-08 `manual-assign.nfo` is byte-identical to `corrected-body.nfo`.

## Hash record

The SHA-256 hashes below cover only the input bytes of the XML and
provider response files. Per the hashing rule in manifest.json, this
file, manifest.json, and README.md never record their own hashes.

### XML inputs

```
d17ec5c2526cc8321da850805371c99b5734782ab1d95c00561c2a4428d06624  xml/NFO-01/complete.nfo
31104cbc0fd5dbf5047ef890a83afb0bdf6cf90ff938ca62e109e2d138355bd9  xml/NFO-02/partial.nfo
2e90ddb1101ce2f6a80990e67cd61ae4abc261e31589954c604a0282363f4d18  xml/NFO-03/content-only.nfo
d5ab12742142b8c0c37916834839fe47e0ffb73a7c530a301a11c022b4c549e9  xml/NFO-04/conflict.nfo
16d21a8e430f4e73f8db321fa86aa834c6b6a93f999660aef699dc8643b50920  xml/NFO-05/tvshow.nfo
83873d465620a4a0903902a5df59cb78d7ea0ef75f5d4f988a3d37a9d4ae7ba8  xml/NFO-06/tvshow.nfo
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  xml/NFO-07/absent.nfo
0d33e4987d73453a537c77eeefda1df751fab05b6a1335f71c4d6c7ec9da4d86  xml/NFO-08/initial-malformed.nfo
f9e518f709d07a529cc6a36f5e3e0a543c7f03c16d4bd54cc24315835a27be9d  xml/NFO-08/corrected-body.nfo
f9e518f709d07a529cc6a36f5e3e0a543c7f03c16d4bd54cc24315835a27be9d  xml/NFO-08/manual-assign.nfo
```

`absent.nfo` is the empty string; its hash is the empty-input hash.
`manual-assign.nfo` shares the hash of `corrected-body.nfo` by design.

### Provider responses

```
62191902c0ceb7041ccd08306bfffa783965f880fa4df02225525408742e5529  provider/NFO-02/movie_detail_9200002.json
27775707d3db51fcfc8ae13e3015c79a1097c8e4c02f7513c2026d6533037602  provider/NFO-03/movie_detail_9200003.json
b0da8fa4d015249791a74a71813281818da03fbbe1123cc2a1b46afd3f56f285  provider/NFO-03/search_movie_keeper_of_the_lantern.json
b9ec0590bd2a9c5e46d7988ea8b7b195f4aa22351e1bfcd5d300bdd6eb95c52e  provider/NFO-04/movie_detail_9200041.json
8e09d869ac8f04838939de6e06384e241015e814c27980b30ac21f1d16921897  provider/NFO-04/movie_detail_9200042.json
cc06e2b233d5463b62decc6656ba709707a4c693818ed83f682b57401e95c511  provider/NFO-04/movie_detail_9200043.json
56944d7f8c06b0b09dfb740c946a4305e44ff8a920196a3b87b3848a920d3cad  provider/NFO-05/find_imdb_tt9300005.json
4012cee9812475d2583fd95301f55e3158df8085a8ee1d75e8273ee2c0103123  provider/NFO-05/tv_detail_9300005.json
8de91486f6954e2cdd37753b926f86a3250ae076704a831cf55d13bb8972b112  provider/NFO-06/find_imdb_tt9300006.json
56e343c83c1fbf27b81db2aad28c78c61010698a050b42e564edc7486a19bcad  provider/NFO-06/search_tv_gravelmere.json
82b86a4bdd514250be800aaead89e4945ac68b348c344e5dd493dd8e5f8b3cf0  provider/NFO-06/tv_detail_9300006.json
67be016f552d5b3364e37d3817d28836ce2aea3df9a37d9645e1aae546c30434  provider/NFO-07/movie_detail_9200007.json
06e81fd34c5f7a06ea4370d88f52061721f522fed22ce4c37059c1381d1e37f3  provider/NFO-07/search_movie_kestrel_road.json
a48c41a25db11bb60971c5b23105040be1ed2af39b4b0fe7a4d5e0e3ecf0e2b9  provider/NFO-08/movie_detail_9200008.json
56e343c83c1fbf27b81db2aad28c78c61010698a050b42e564edc7486a19bcad  provider/NFO-08/search_movie_secondhand_orbit_miss.json
```

`search_tv_gravelmere.json` and `search_movie_secondhand_orbit_miss.json`
share a hash because both bodies are an empty-results search response;
they answer different routes and stay separate files.

## Totals at authoring time

- XML: 10 files, 7543 bytes (limit 128 KiB)
- Provider: 15 files, 21407 bytes (limit 512 KiB)
- Largest single file under any per-file limit

No generated reports or execution receipts are part of this corpus.
