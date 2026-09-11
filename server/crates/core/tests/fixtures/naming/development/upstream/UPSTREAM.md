# Upstream provenance — naming parser fixtures

These files are unmodified copies of parser test fixtures from two GPL-3.0-only
projects. Nightjar uses them as development evidence only. They are not Nightjar
ground truth, and the product never ships them.

## Origin

| Prefix | Repository | Commit | Upstream directory | Retrieval |
|--------|------------|--------|--------------------|-----------|
| `sonarr-` | https://github.com/Sonarr/Sonarr | `a533a1a463add90fd919060b6abd426d5665859d` | `src/NzbDrone.Core.Test/ParserTests` | 2026-08-13T06:28:12Z |
| `radarr-` | https://github.com/Radarr/Radarr | `34b0f5450fdc346cc72cf18669e601d68b974250` | `src/NzbDrone.Core.Test/ParserTests` | 2026-08-13T06:28:12Z |

The local file name prefixes `sonarr-` and `radarr-` disambiguate the two
upstream files that are both named `ParserFixture.cs`. `SOURCES.json` maps every
local file to its repository and its upstream file name.

## License

Both upstream projects are licensed GPL-3.0-only. Nightjar is GPL-3.0-only, so
the files stay under the same license. The full license text is the product
`LICENSE` at the repository root. The copies are byte-for-byte, and they carry
the upstream notices they carried when fetched. The fetched fixture files carry
no per-file SPDX header; the repository license is the license record for them.

## Integrity

`SOURCES.json` records the SHA-256 of every copied file. A changed byte fails
the integrity check. The check also fails when a `.cs` file appears here that
`SOURCES.json` does not declare, or when a declared file is missing.

## Register

| File | Upstream file | SHA-256 |
|------|---------------|---------|
| `radarr-EditionParserFixture.cs` | `EditionParserFixture.cs` | `963ef744e70c2d1127fa91893ad4c01f3d1a879433b8219d1ae103c56f517f01` |
| `radarr-ParserFixture.cs` | `ParserFixture.cs` | `4b48f995ab0b370561c141e05250a785528fa3ba723d7308dc40922366d0be03` |
| `sonarr-AbsoluteEpisodeNumberParserFixture.cs` | `AbsoluteEpisodeNumberParserFixture.cs` | `5bdf58e311672c6531714b28f2eea3f9c08fbf94eeb5380e5aeef7ccdb89bdb0` |
| `sonarr-CrapParserFixture.cs` | `CrapParserFixture.cs` | `a653a8ea6538931fa24d54442dac17ee9452ba508aaaf370183dcb065e70b5bb` |
| `sonarr-DailyEpisodeParserFixture.cs` | `DailyEpisodeParserFixture.cs` | `d4905f0ae688688f2ac94a93b185b25ab878df097de32fc619561d6af0184d9b` |
| `sonarr-MiniSeriesEpisodeParserFixture.cs` | `MiniSeriesEpisodeParserFixture.cs` | `05b4f8d2f3bb10677672007f210fe0393b4fa97dfdd588837420889cf6c77165` |
| `sonarr-MultiEpisodeParserFixture.cs` | `MultiEpisodeParserFixture.cs` | `b41c5d11fd2253fd7ca4d06570c41d55e2aa784cde2270aa6f35e1d749f6a24d` |
| `sonarr-ParserFixture.cs` | `ParserFixture.cs` | `d328af39d16f6e2db57e039c4d51478a5fb549713d2750c540f2910ae8daf26e` |
| `sonarr-PathParserFixture.cs` | `PathParserFixture.cs` | `3b34877a42f40a98f35d775db8b83b4c62af0590ace2b0b213025640f0b65fb2` |
| `sonarr-SeasonParserFixture.cs` | `SeasonParserFixture.cs` | `77e91e8e9cd8cea0a5acb31c07867b23059c9e315d324b013513b582ca36e85c` |
| `sonarr-SingleEpisodeParserFixture.cs` | `SingleEpisodeParserFixture.cs` | `3647e3a3ce3e568f80b933946238e9fae1c5e68723662ac3c819ce60a4a99871` |
| `sonarr-UnicodeReleaseParserFixture.cs` | `UnicodeReleaseParserFixture.cs` | `60c47dbfcf48692b4608f635335596572814264e142ab4960c32a5cbf0af130d` |
