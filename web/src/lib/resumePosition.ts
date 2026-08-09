/**
 * Where an item is up to, asked in one place.
 *
 * Two cases, and only one of them exists today:
 *
 * - **Within one page life** the client knows the position without asking
 *   anyone: the player holds `video.currentTime` plus the session's window
 *   offset. That covers the case this module was written for, which is a
 *   session dying under an open player and the viewer pressing play again.
 * - **Across a reload, or on another device**, the position lives in
 *   server-held watch state. That is ADR-0035 and lands with B2-3; the cold
 *   path below returns null until it does.
 *
 * Both the item page and the player read through [`resumePositionMs`] rather
 * than each keeping their own idea of position, so B2-3 has one function to
 * change instead of two surfaces to find.
 */

/**
 * Positions observed this page life, by item id. Module scope, so it survives
 * client-side navigation between the item page and the player and dies on
 * reload — which is exactly the boundary between the two cases above.
 */
const observed = new Map<number, number>();

/** Record where an item reached. Called by the player as it plays. */
export function rememberPositionMs(itemId: number, ms: number): void {
	if (!Number.isFinite(ms) || ms < 0) return;
	observed.set(itemId, Math.floor(ms));
}

/** Forget an item's position, e.g. after it is played to the end. */
export function forgetPosition(itemId: number): void {
	observed.delete(itemId);
}

/**
 * Where to resume `itemId`, or null to start from the beginning.
 *
 * Async because the server read that B2-3 adds here will be, and a caller
 * written against a synchronous signature today would have to change then.
 */
export async function resumePositionMs(itemId: number): Promise<number | null> {
	const live = observed.get(itemId);
	if (live != null && live > 0) return live;
	// ADR-0035 / B2-3: read the profile's watch_state row here. Until that
	// table exists there is no answer, and returning null is the honest one.
	return null;
}
