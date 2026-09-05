/**
 * Wait for a freshly created session's playlist to be servable, and discard
 * the session when it is not.
 *
 * A session the client stops waiting on must be deleted here, not left for the
 * idle reaper: the reaper runs about a minute later, and in that minute a busy
 * server keeps refusing new session starts, so a few failed plays make the
 * next press report the server busy while it is idle. The delete is injected
 * so this stays the single wait path for both the initial start and an
 * audio-track switch, which already lived by this rule.
 *
 * The wait never rejects. A fetch that throws is a network verdict, not an
 * exception: a rejection thrown out of a poll leaves the caller stuck on its
 * "starting…" state with no recovery path, and a transient network error is
 * exactly the case worth retrying through the next poll.
 */

/** Why the wait ended without a servable playlist. */
export type SessionWaitReason = 'gone' | 'gave-up' | 'network';

export type SessionWaitOutcome =
	| { state: 'ready' }
	| { state: 'released'; reason: SessionWaitReason };

export interface SessionWaitDeps {
	/** False once the owning page has unmounted; polling stops. */
	alive: () => boolean;
	/** Discard the session whose readiness the wait was checking. */
	release: () => void;
	/** Override for tests. Defaults to the global fetch. */
	fetchImpl?: typeof fetch;
}

/**
 * Poll ceiling for the ready wait, 100 x 200 ms ≈ 20 s. A guess to cover a
 * cold FFmpeg spawn and the encode lead-in on a busy server; no measurement
 * sits behind it yet (Rule 4.14).
 */
const POLL_ATTEMPTS = 100;
const POLL_INTERVAL_MS = 200;

export async function waitForSessionReady(
	url: string,
	deps: SessionWaitDeps,
	attempts = POLL_ATTEMPTS,
	intervalMs = POLL_INTERVAL_MS
): Promise<SessionWaitOutcome> {
	// Tag .m4s side-channel polls so dogfood logs can tell them from Safari's
	// own native segment GETs (same URL, no query).
	const fetchUrl = /\.m4s(?:\?|$)/i.test(url)
		? `${url}${url.includes('?') ? '&' : '?'}njFetcher=attach-wait`
		: url;
	const doFetch = deps.fetchImpl ?? globalThis.fetch.bind(globalThis);
	let reason: SessionWaitReason = 'gave-up';
	for (let i = 0; i < attempts; i++) {
		if (!deps.alive()) break;
		try {
			const res = await doFetch(fetchUrl);
			if (res.ok) return { state: 'ready' };
			// Gone for good (deleted / never created). 503 means still cooking.
			if (res.status === 404) {
				reason = 'gone';
				break;
			}
			reason = 'gave-up';
		} catch {
			// A network error says nothing about the file. Keep polling: a
			// transient blip may clear, and if it never clears the wait ends
			// on the network verdict instead of rejecting out of the caller's
			// async start.
			reason = 'network';
		}
		await new Promise((r) => setTimeout(r, intervalMs));
	}
	deps.release();
	return { state: 'released', reason };
}
