/**
 * Ordering for the start of playback. Pure so it can be checked without a
 * browser, which is why the narrowing itself lives in the API client rather
 * than here.
 */

/**
 * Narrow the session, **then** mark playback started. The order is the point.
 *
 * **ADR-0034 item 3 draws the boundary this walks.** A profile session never
 * administers the server, and the two byte routes
 * (`POST /items/{id}/sessions`, `GET /items/{id}/stream`) refuse an
 * account-scope session because they need to know who is watching. Those are
 * the only two routes in the API that require a profile.
 *
 * **Direct play is why the order matters rather than merely being tidy.** It
 * renders `<video src={streamUrl}>` as soon as `started` is true, and the
 * browser issues that request itself with the session cookie. An account-scope
 * session gets 403 from `/items/{id}/stream`, so the narrowing has to be
 * *complete* before the element mounts, not racing it.
 *
 * Dependencies are injected so the ordering is checkable:
 * `tests/profileScope.test.ts` holds `ensureScope` unresolved and asserts
 * `markStarted` has not run.
 */
export async function beginPlayback(deps: {
	ensureScope: () => Promise<void>;
	markStarted: () => void;
}): Promise<void> {
	await deps.ensureScope();
	deps.markStarted();
}
