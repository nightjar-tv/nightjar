/**
 * Title ↔ media timeline (ADR-0020, ADR-0054).
 *
 * **Element time is zeroed at the first segment the playlist lists, and there
 * are two listings.** ADR-0020's per-run playlist begins at the run's land, so
 * `currentTime` 0 is the land — measured on hls.js and Safari native after
 * POST /seek @ 600000 → currentTime ≈ 0…tens, not 600. ADR-0054's full-title
 * playlist begins at 0 and says the land in `EXT-X-START` instead, so
 * `currentTime` is already title-absolute. `sidx` / `-output_ts_offset` stay
 * title-absolute for the map in both.
 *
 * **So the offset is `mediaOriginMs`, not `landedMs`.** The server states it
 * per playlist because which listing a run serves is a runtime property that
 * can change mid-session, and reading the land instead added it twice: a
 * session landed at 600 s showed `20:02` on a 15-minute title.
 *
 * `mediaOriginMs` is mutable (every run swap, and every listing change).
 * Callers must pass the **current** value, not one cached at first attach.
 */

/** Scrub / total-time authority: usable extent when damaged, else item duration. */
export function scrubRangeMs(
	itemDurationMs: number | null | undefined,
	usableExtentMs: number | null | undefined
): number {
	if (usableExtentMs != null && Number.isFinite(usableExtentMs) && usableExtentMs >= 0) {
		return usableExtentMs;
	}
	if (itemDurationMs != null && Number.isFinite(itemDurationMs) && itemDurationMs > 0) {
		return itemDurationMs;
	}
	return 0;
}

export function titleSecondsFromMedia(mediaSeconds: number, mediaOriginMs: number): number {
	const media = Number.isFinite(mediaSeconds) ? Math.max(0, mediaSeconds) : 0;
	const origin = Math.max(0, mediaOriginMs) / 1000;
	return origin + media;
}

export function mediaSecondsFromTitle(titleSeconds: number, mediaOriginMs: number): number {
	const title = Number.isFinite(titleSeconds) ? Math.max(0, titleSeconds) : 0;
	const origin = Math.max(0, mediaOriginMs) / 1000;
	return Math.max(0, title - origin);
}

/**
 * True when `mediaSeconds` is inside already-produced media for this run
 * (seekable/buffered). Used to choose currentTime vs POST /seek.
 */
export function mediaTimeInProducedWindow(
	mediaSeconds: number,
	seekable: TimeRanges,
	buffered: TimeRanges,
	duration: number,
	slackSec = 0.35
): boolean {
	if (!Number.isFinite(mediaSeconds) || mediaSeconds < 0) return false;
	let end = 0;
	let have = false;
	if (seekable.length > 0) {
		end = seekable.end(seekable.length - 1);
		have = true;
	} else if (buffered.length > 0) {
		end = buffered.end(buffered.length - 1);
		have = true;
	} else if (Number.isFinite(duration) && duration > 0) {
		end = duration;
		have = true;
	}
	if (!have) {
		// Empty EVENT just after land: allow only a tiny local nudge.
		return mediaSeconds <= slackSec;
	}
	return mediaSeconds <= end + slackSec;
}
