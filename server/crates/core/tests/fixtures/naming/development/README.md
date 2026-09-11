# development/

Pinned upstream parser evidence. `corpus.json` is generated from the 12 files in
`upstream/` by `server/tools/naming_corpus/extract.py`. `upstream/SOURCES.json`
records each source's repository, commit and SHA-256; `upstream/UPSTREAM.md`
records the license and the retrieval date.

Ownership: the core crate. Regenerate with the extractor; do not hand-edit
`corpus.json`.

This is a development set. It is not held out, and it must not be used alone to
report parse quality.

## Case fields

`corpus.json` holds `schema_version`, `set`, `counts`, `categories`, `sources`
and `cases`. Every case carries:

| Field | Meaning |
|-------|---------|
| `id` | Stable identifier: source stem, upstream test name, ordinal |
| `source` | Local upstream file name |
| `test` | Upstream `[TestCase]` method name |
| `input` | The filename string the upstream test passes |
| `expect` | Nightjar fields the case asserts: `title`, `title_key`, `year`, `season`, `episode`, `episodes`, `reject` |
| `dropped` | Upstream fields Nightjar does not produce, plus absent-as-zero notes |
| `applicable` | True when `expect` asserts at least one Nightjar field |
| `reason` | Why the case is applicable, or why it is excluded |
| `category` | Naming category, from the input's shape |
