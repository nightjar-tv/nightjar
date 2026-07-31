/**
 * Title ↔ media timeline (ADR-0020).
 *
 * Producer-truth EVENT playlists use window-relative media time: element
 * `currentTime` 0 is the run's land, not title 0. Measured on hls.js and
 * Safari native after POST /seek @ 600000 → currentTime ≈ 0…tens, not 600.
 * `sidx` / `-output_ts_offset` stay title-absolute for the map; the element
 * does not.
 *
 * `landedMs` is mutable (every run swap). Callers must pass the **current**
 * session land, not a value cached at first attach.
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

export function titleSecondsFromMedia(mediaSeconds: number, landedMs: number): number {
	const media = Number.isFinite(mediaSeconds) ? Math.max(0, mediaSeconds) : 0;
	const land = Math.max(0, landedMs) / 1000;
	return land + media;
}

export function mediaSecondsFromTitle(titleSeconds: number, landedMs: number): number {
	const title = Number.isFinite(titleSeconds) ? Math.max(0, titleSeconds) : 0;
	const land = Math.max(0, landedMs) / 1000;
	return Math.max(0, title - land);
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
