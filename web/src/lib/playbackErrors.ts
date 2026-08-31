/**
 * What a fatal media-load failure means for the retry loop. Pure so it can be
 * checked without a browser.
 *
 * Both backends reach this (Rule 2.4): hls.js reads the status off the error
 * event, Safari native has to ask the server because a dead session surfaces
 * as a generic media error with no code.
 */
export type LoadFailureAction = 'retry' | 'session-gone' | 'unauthorized';

/** The subset `reportSessionGone` carries to the surface. */
export type SessionGoneReason = Exclude<LoadFailureAction, 'retry'>;

/**
 * **Retrying only helps a failure the next request could answer differently.**
 *
 * A 404 is the server saying the session is gone; re-asking the same missing
 * URL never stops. That was already handled. **A 401 is worse and was not**: no
 * amount of retrying mints a credential the browser does not have, and until
 * 2026-08-31 it fell through to the retry arm — measured at about 100 requests
 * in 25 seconds against `master.m3u8`, with nothing shown to the person
 * watching (`nightjar-meta` `notes/OPEN-DEFECTS.md` entry 16).
 *
 * Everything else is a blip worth another go: a dropped connection, a 5xx while
 * FFmpeg catches up, a 503 from the segment hold.
 */
export function loadFailureAction(status: number | undefined): LoadFailureAction {
	if (status === 404) return 'session-gone';
	if (status === 401 || status === 403) return 'unauthorized';
	return 'retry';
}
