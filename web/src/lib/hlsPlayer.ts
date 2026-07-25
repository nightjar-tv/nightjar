import Hls from 'hls.js';

/** True when the browser can play HLS without hls.js (Safari). */
export function canPlayNativeHls(video: HTMLVideoElement): boolean {
	return video.canPlayType('application/vnd.apple.mpegurl') !== '';
}

export type HlsAttachOptions = {
	/**
	 * Called when a seek gets HTTP 409 (shared session). Should POST a new
	 * session at absoluteStartMs, DELETE the previous session id (move the
	 * ref), and return the forked playlist — or null if the cap is full
	 * ("sessions busy").
	 */
	forkAt?: (absoluteStartMs: number) => Promise<{
		sessionId: string;
		playlistUrl: string;
	} | null>;
	onSession?: (sessionId: string) => void;
};

/**
 * Attach an HLS VOD playlist. Native Safari; hls.js elsewhere.
 *
 * Scrub sequence (no 750ms ignore window — `seeked` replaces that):
 * 1. User finishes a scrub → `seeked` fires once (not `seeking` ticks).
 * 2. Client GETs playlist?startMs=T. Solo: server restarts in place and
 *    retains prior-window segments. Shared: 409 → forkAt POSTs a new
 *    session, DELETEs the old ref, swaps the playlist URL.
 * 3. Player requests the target segment; server waits until it exists
 *    (503 while cooking, not 404).
 */
export function attachHls(
	video: HTMLVideoElement,
	basePlaylistUrl: string,
	options: HlsAttachOptions = {}
): { destroy: () => void } {
	let playlistBase = basePlaylistUrl;
	let hls: Hls | null = null;
	let destroyed = false;
	let seekInFlight = false;

	const attach = (resumeAt: number) => {
		const restore = () => {
			if (destroyed || resumeAt <= 0) return;
			try {
				video.currentTime = resumeAt;
			} catch {
				/* ignore */
			}
		};
		if (canPlayNativeHls(video)) {
			video.addEventListener('loadedmetadata', restore, { once: true });
			video.src = playlistBase;
			return;
		}
		if (!Hls.isSupported()) {
			throw new Error('HLS playback is not supported in this browser');
		}
		if (hls) {
			hls.destroy();
			hls = null;
		}
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
		video.addEventListener('loadedmetadata', restore, { once: true });
		hls.loadSource(playlistBase);
		hls.attachMedia(video);
	};

	const onSeeked = () => {
		if (destroyed || seekInFlight) return;
		seekInFlight = true;
		void (async () => {
			try {
				const startMs = Math.max(0, Math.floor(video.currentTime * 1000));
				const res = await fetch(`${playlistBase}?startMs=${startMs}`);
				if (destroyed) return;
				if (res.status === 409 && options.forkAt) {
					const forked = await options.forkAt(startMs);
					if (!forked || destroyed) return;
					playlistBase = forked.playlistUrl;
					options.onSession?.(forked.sessionId);
					attach(video.currentTime);
				}
			} catch {
				// Cap-full / network: page surfaces "sessions busy" via forkAt.
			} finally {
				seekInFlight = false;
			}
		})();
	};

	video.addEventListener('seeked', onSeeked);
	attach(0);

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
