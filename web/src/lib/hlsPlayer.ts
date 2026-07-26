import Hls from 'hls.js';

/** True when the browser can play HLS without hls.js (Safari). */
export function canPlayNativeHls(video: HTMLVideoElement): boolean {
	return video.canPlayType('application/vnd.apple.mpegurl') !== '';
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
 * Mid-title sessions (audio switch): the media playlist starts at the
 * session window (`EXT-X-MEDIA-SEQUENCE`), so players must not be pointed
 * at a `#t=` fragment — Safari's HLS path treats that as a broken source
 * (play button strikethrough). Absolute fMP4 timestamps keep the scrubber
 * near the real offset once the first segment arrives.
 */
export function attachHls(
	video: HTMLVideoElement,
	playlistBase: string,
	startAtSeconds = 0
): { destroy: () => void } {
	let hls: Hls | null = null;
	let destroyed = false;
	let seekInFlight = false;
	// First land after a mid-title attach is not a user scrub.
	let suppressSeekNotify = startAtSeconds > 0;

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
				const startMs = Math.max(0, Math.floor(video.currentTime * 1000));
				await fetch(`${playlistBase}?startMs=${startMs}`);
			} catch {
				// Network blip: the player retries the segment fetch itself.
			} finally {
				seekInFlight = false;
			}
		})();
	};

	video.addEventListener('seeked', onSeeked);

	return {
		destroy: () => {
			destroyed = true;
			video.removeEventListener('seeked', onSeeked);
			if (hls) {
				hls.destroy();
				hls = null;
			}
			video.removeAttribute('src');
			video.load();
		}
	};
}
