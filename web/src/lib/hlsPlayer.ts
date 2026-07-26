import Hls from 'hls.js';

/** True when the browser can play HLS without hls.js (Safari). */
export function canPlayNativeHls(video: HTMLVideoElement): boolean {
	return video.canPlayType('application/vnd.apple.mpegurl') !== '';
}

export interface HlsHandle {
	destroy: () => void;
	/** Title-absolute playback position in seconds (see timeline note). */
	positionSeconds: () => number;
}

/**
 * Attach an HLS VOD playlist. Native Safari; hls.js elsewhere.
 *
 * Scrub sequence (no 750ms ignore window — `seeked` replaces that):
 * 1. User finishes a scrub → `seeked` fires once (not `seeking` ticks).
 * 2. Client GETs playlist?startMs=T; the server restarts this session at
 *    that offset and retains prior-window segments (ADR-0011).
 * 3. Player requests the target segment; server waits until it exists
 *    (503 while cooking, not 404).
 *
 * Timeline note: a mid-title session (audio switch, ADR-0012) serves a
 * playlist that starts at its window. Safari native exposes the absolute
 * fMP4 timestamps, so currentTime is title-absolute; hls.js aligns media to
 * the playlist and reports a 0-based clock. `positionSeconds` resolves which
 * timeline this element got and always returns title-absolute seconds —
 * reading raw currentTime after a switch is how "switch back" restarts a
 * title at zero.
 */
export function attachHls(
	video: HTMLVideoElement,
	playlistBase: string,
	startAtSeconds = 0
): HlsHandle {
	let hls: Hls | null = null;
	let destroyed = false;
	let seekInFlight = false;
	// First land after a mid-title attach is not a user scrub.
	let suppressSeekNotify = startAtSeconds > 0;
	// null until the element shows real progress and the timeline is known.
	let timelineOffset: number | null = startAtSeconds > 0 ? null : 0;

	const positionSeconds = (): number => {
		if (timelineOffset === null) {
			if (video.currentTime < 0.5) {
				// No progress yet: the session was opened at the window start.
				return startAtSeconds;
			}
			// Within a segment of the window start = absolute timestamps
			// (Safari); otherwise the player normalised to zero (hls.js).
			timelineOffset = video.currentTime >= startAtSeconds - 4 ? 0 : startAtSeconds;
		}
		return timelineOffset + video.currentTime;
	};

	if (canPlayNativeHls(video)) {
		video.src = playlistBase;
	} else if (!Hls.isSupported()) {
		throw new Error('HLS playback is not supported in this browser');
	} else {
		hls = new Hls({
			enableWorker: true,
			maxBufferHole: 1.5
		});
		// Not-ready segments return 503; treat as retryable network errors.
		hls.on(Hls.Events.ERROR, (_event, data) => {
			if (!data.fatal || destroyed) return;
			if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
				hls?.startLoad();
			} else if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
				hls?.recoverMediaError();
			}
		});
		hls.loadSource(playlistBase);
		hls.attachMedia(video);
	}

	const onSeeked = () => {
		if (destroyed || seekInFlight) return;
		if (suppressSeekNotify) {
			suppressSeekNotify = false;
			return;
		}
		seekInFlight = true;
		void (async () => {
			try {
				const startMs = Math.max(0, Math.floor(positionSeconds() * 1000));
				await fetch(`${playlistBase}?startMs=${startMs}`);
			} catch {
				// Network blip: the player retries the segment fetch itself.
			} finally {
				seekInFlight = false;
			}
		})();
	};

	// A mid-title attach replaces a session the user was already watching;
	// waiting for another play press loses them. Rejection (autoplay policy)
	// just leaves the existing controls.
	const onCanPlay = () => {
		if (!destroyed && startAtSeconds > 0) {
			void video.play().catch(() => {});
		}
	};

	video.addEventListener('seeked', onSeeked);
	video.addEventListener('canplay', onCanPlay, { once: true });

	return {
		destroy: () => {
			destroyed = true;
			video.removeEventListener('seeked', onSeeked);
			video.removeEventListener('canplay', onCanPlay);
			if (hls) {
				hls.destroy();
				hls = null;
			}
			video.removeAttribute('src');
			video.load();
		},
		positionSeconds
	};
}
