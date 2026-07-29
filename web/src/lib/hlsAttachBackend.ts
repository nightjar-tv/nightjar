/**
 * AttachBackend selection helpers (ADR-0017) and seek-notify suppress
 * state transitions. Pure so they can be checked without a browser.
 */

export type AttachBackend = 'native-hls' | 'hls-js';

/**
 * True for Apple WebKit that ships HLS (Safari / iOS WebViews). False for
 * Chromium — including Chrome iOS (`CriOS`) — even when canPlayType lies.
 */
export function isAppleWebKitHlsEngine(ua: string): boolean {
	if (/Chrom(e|ium)|Edg\/|OPR\/|CriOS|EdgiOS|FxiOS/i.test(ua)) {
		return false;
	}
	return /Safari/i.test(ua) || /AppleWebKit/i.test(ua);
}

/**
 * iPhone / iPod / iPad, including iPadOS that reports as Macintosh.
 * Desktop Safari must not match (ADR-0017: desktop → hls.js).
 */
export function isAppleMobileOs(ua: string, maxTouchPoints: number): boolean {
	if (/iPhone|iPod|iPad/i.test(ua)) return true;
	// iPadOS 13+: desktop UA + multi-touch.
	if (/Macintosh/i.test(ua) && maxTouchPoints > 1) return true;
	return false;
}

export type PickBackendInput = {
	ua: string;
	maxTouchPoints: number;
	canPlayNativeHls: boolean;
	hlsJsSupported: boolean;
	/** `?njNativeHls=1` — force native on desktop Apple WebKit for regression. */
	forceNativeHls: boolean;
};

/**
 * Client-only attach fork (ADR-0011 / ADR-0017). Server contract identical.
 *
 * - iOS/iPadOS Apple WebKit + canPlay → native-hls
 * - Else hls.js when MSE works (desktop Safari, Chrome, Firefox, …)
 * - Else native if canPlay still claims HLS (odd WebViews)
 * - `forceNativeHls`: desktop Apple WebKit escape hatch → native-hls
 */
export function chooseAttachBackend(input: PickBackendInput): AttachBackend {
	const apple = isAppleWebKitHlsEngine(input.ua);
	const mobile = isAppleMobileOs(input.ua, input.maxTouchPoints);

	if (input.forceNativeHls) {
		if (!(apple && input.canPlayNativeHls)) {
			throw new Error(
				'njNativeHls=1 requested but Apple WebKit native HLS is not available'
			);
		}
		return 'native-hls';
	}

	if (apple && mobile && input.canPlayNativeHls) {
		return 'native-hls';
	}
	if (input.hlsJsSupported) {
		return 'hls-js';
	}
	if (input.canPlayNativeHls) {
		return 'native-hls';
	}
	throw new Error('HLS playback is not supported in this browser');
}

/**
 * After a suppressed `seeked`: if a land retarget is pending, apply it and
 * keep suppress for the follow-up seeked; otherwise clear suppress.
 * Generation counters live in the caller so a stale rAF cannot clear a
 * newer arm.
 */
export function seekSuppressOnSeeked(landRetargetPending: boolean): {
	applyLand: boolean;
	clearSuppress: boolean;
} {
	if (landRetargetPending) {
		return { applyLand: true, clearSuppress: false };
	}
	return { applyLand: false, clearSuppress: true };
}

/**
 * Double-rAF safety clear: only clear when this arm's generation is still
 * current and we are not mid two-step land retarget (that needs the second
 * seeked, or the timeout fallback).
 */
export function seekSuppressRafMayClear(
	armedGen: number,
	currentGen: number,
	landRetargetPending: boolean
): boolean {
	return armedGen === currentGen && !landRetargetPending;
}
