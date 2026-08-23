# Iteration 1 — board item 1, the resolver

## The board's mechanism is wrong, and the fix it implies is the wrong fix

> `ureq` takes the first address the resolver returns and never falls back.

It does fall back. `ureq` 2.12.1, `src/stream.rs:380`:

    // Find the first sock_addr that accepts a connection
    let multiple_addrs = sock_addrs.len() > 1;
    for sock_addr in sock_addrs {
        ...
        if let Ok(stream) = stream { ...; break; }
        else if let Err(err) = stream { any_err = Some(err); }
    }

Every address is tried and the loop breaks on the first success. **What `ureq`
does not do is give them equal time.** When there is more than one address it
halves the *remaining* connect budget on each pass:

    let mut deadline = time_until_deadline(deadline)?;
    if multiple_addrs { deadline = deadline.div(2); }

So the n-th address gets `timeout / 2ⁿ`, and `time_until_deadline` returns an
error — out of the loop entirely — once the budget is gone.

## The population, counted where it can be counted

`api.themoviedb.org` on this machine, resolved 2026-08-23 (**DNS only, no
connection, no request**):

    12 addresses: 8 × AAAA, then 4 × A
    666666664444

With `timeout_connect(10s)` and IPv6 to TMDB dead, the eight AAAA records
consume `5 + 2.5 + 1.25 + 0.625 + 0.31 + 0.16 + 0.08 + 0.04 ≈ 9.96s`. The first
A record is reached with **19ms** of budget, and IPv4 needs **280ms**. So the
call fails having never once tried an address that works — and it fails as
`Connect error: connection timed out`, which is what 141 warming requests
reported over 23 minutes on 2026-08-22 while reading like a bad key.

**Both diagnoses produce "never connects". They do not imply the same fix.** An
IPv4-only resolver repairs either one; interleaving repairs only the real one.
Worth the twenty minutes it took to read the loop.

## The convention it depends on

That `getaddrinfo` returns both families, and that `ureq` walks the resolver's
list in order. Without both — a v6-only answer, say — interleaving changes
nothing and the host stays unreachable, which is correct.

## Predicted before running

First IPv4 moves from index 8 to index 1, where the budget is still 2.5s. The
call then succeeds about 5s in: one dead IPv6 timeout, then a live connection.
Every offline instrument reports exactly zero, because none of them opens a
socket.

## Measured

Through the shipped function, on the real DNS answer:

    12 addrs; first IPv4 at Some(8) -> Some(1)
    families 666666664444 -> 646464646666

Six unit tests, and one `#[ignore]`d DNS check so the suite never depends on a
resolver. `cargo test --workspace`: **the metadata crate 268 passed, 0 failed.**

## Why not prefer IPv4, which is what the measurement tree did

Because that ships a second bug to everyone. An IPv4-only resolver strands any
host on a v6-only network and silently discards the ordering the OS expressed
through RFC 6724. Interleaving keeps **both** families and only stops one of
them from spending the whole budget first; a host with working IPv6 still
connects on attempt 1 and never notices. The board says the same thing, and the
tests assert it in both directions.

## What this does not fix

The ~5s lost to the first dead address. Avoiding that needs the families raced
in parallel (RFC 8305 Happy Eyeballs), and `ureq` 2.x connects strictly in
sequence inside its own loop, where a resolver cannot reach. **This turns
*never* into *slow*, and no further.** At ~5.3s per call, the 1,758-call warming
run in note 07 would take about 2.6 hours — worth saying out loud to whoever
runs it next, because "working" and "quick" are not the same claim.

## Which instrument saw this, and why the zeros are zeros

| instrument | reading | why |
|---|---|---|
| matcher oracle | not run | **Insensitive by construction.** Strict and offline; `requests=0` is the pass condition. The resolver closure is only entered on a connect, and no connect happens. |
| parser corpus | not run | Insensitive by construction — parse-level, no socket. |
| parser sweep | not run | Insensitive by construction — parse-level, no socket. |
| dogfood strict pair | not run | Insensitive by construction — offline replay. |
| `cargo test --workspace` | 0 failures attributable | Genuinely sensitive to a compile or behaviour break, and the only instrument here that is. |

**No instrument on the board can see this change, so none was run to claim it
clean.** A zero from an instrument that cannot see the change is insensitivity,
and reporting one as a result is the thing this loop exists to stop.

## One failure, and it is not this

`nightjar-transcode`'s `hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac`
failed in the workspace run. It is not this change:

  * `nightjar-transcode` does not depend on `nightjar-metadata` — not in its
    `Cargo.toml`, and no reference to it anywhere in the crate. The change
    cannot reach it.
  * Base tree, same suite: **158 passed, 0 failed.**
  * My tree, same binary, three consecutive runs: **FAILED, ok, ok.**
  * Alone rather than in the suite, with 11 GB free: passes.

Flaky under parallel execution, and the known one the README already names. Disk
was checked first, because the README records a full volume turning 1 transcode
failure into 20 and reading exactly like a regression — 11 GB free here.
