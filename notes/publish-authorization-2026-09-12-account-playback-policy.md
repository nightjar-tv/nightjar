# Publish authorization — account playback policy

On 2026-09-12, after PR #278 merged, the maintainer directed continued
progress. The active anti-drift plan authorizes carrying the settled B2-9
server slice through branch publication, pull request, and CI. This covers the
independently verified account playback-policy contract, atomic per-account
playback concurrency, and explicit session replacement. It does not authorize
deployment or mutation of the live dogfood host, or merging the pull request;
both remain separate maintainer actions.
