import Hls from 'hls.js';

/**
 * Exactly two attach backends. The server contract is identical for both
 * (full-title VOD, 503 while cold regions cook, ADR-0011): no user-agent
 * branching on the server. Differences live only here.
 *
 * - `native-hls`: Safari / WebKit. `video.src = playlist`. Hardware HLS
 *   path; required for iOS/tvOS. Scrub often drives window moves via
 *   segment fetches alone (no `?startMs=`). Desktop Safari ignores
 *   EXT-X-START, so a mid-title attach must seek to the window itself on
 *   loadedmetadata or Safari fetches pre-window segments and stalls on
 *   their 503s.
 * - `hls-js`: Chromium / Firefox MSE. `startPosition` lands a mid-title
 *   attach; network/media errors are retried. Scrub notifies via `seeked`
 *   → `playlist?startMs=`.
 */
type AttachBackend = 'native-hls' | 'hls-js';

export function canPlayNativeHls(video: HTMLVideoElement): boolean {
	return video.canPlayType('application/vnd.apple.mpegurl') !== '';
}

function pickBackend(video: HTMLVideoElement): AttachBackend {
	if (canPlayNativeHls(video)) {
		return 'native-hls';
	}
	if (Hls.isSupported()) {
		return 'hls-js';
	}
	throw new Error('HLS playback is not supported in this browser');
}

export interface HlsHandle {
	destroy: () => void;
	/** Title-absolute playback position in seconds. */
	positionSeconds: () => number;
}

/**
 * Attach an HLS VOD playlist.
 *
 * Scrub sequence:
 * 1. User finishes a scrub → `seeked` fires once (not `seeking` ticks).
 * 2. Client GETs playlist?startMs=T (explicit restart). Safari often skips
 *    this and hits the segment path instead; the server handles both.
 * 3. Player requests the target segment; 503 while cooking (ADR-0011).
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

	const backend = pickBackend(video);

	const positionSeconds = (): number => Math.max(0, video.currentTime);

	if (backend === 'native-hls') {
		video.src = playlistBase;
		if (startAtSeconds > 0) {
			video.addEventListener(
				'loadedmetadata',
				() => {
					if (!destroyed) video.currentTime = startAtSeconds;
				},
				{ once: true }
			);
		}
	} else {
		hls = new Hls({
			enableWorker: true,
			maxBufferHole: 1.5,
			// Full-title playlist: land at the session window (hls.js; native
			// uses EXT-X-START from the server instead).
			startPosition: startAtSeconds > 0 ? startAtSeconds : -1
		});
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
