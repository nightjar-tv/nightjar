import Hls from 'hls.js';

/** True when the browser can play HLS without hls.js (Safari). */
export function canPlayNativeHls(video: HTMLVideoElement): boolean {
	return video.canPlayType('application/vnd.apple.mpegurl') !== '';
}

function playlistUrl(base: string, startMs: number): string {
	const sep = base.includes('?') ? '&' : '?';
	return `${base}${sep}startMs=${startMs}`;
}

function inBufferedRange(video: HTMLVideoElement, t: number): boolean {
	const { buffered } = video;
	for (let i = 0; i < buffered.length; i++) {
		if (t >= buffered.start(i) && t <= buffered.end(i) + 0.5) {
			return true;
		}
	}
	return false;
}

/**
 * Attach an HLS playlist to a video element. Uses native playback on Safari;
 * hls.js elsewhere. Seeks outside the buffered window reload the playlist with
 * ?startMs= so the server restarts FFmpeg (ADR-0007).
 */
export function attachHls(
	video: HTMLVideoElement,
	basePlaylistUrl: string
): { destroy: () => void } {
	let startMs = 0;
	let hls: Hls | null = null;
	let destroyed = false;
	/** Ignore seeking storms while hls.js / the element settles after a reload. */
	let ignoreSeekUntil = 0;

	const load = (ms: number) => {
		startMs = ms;
		ignoreSeekUntil = performance.now() + 750;
		const url = playlistUrl(basePlaylistUrl, startMs);
		if (canPlayNativeHls(video)) {
			video.src = url;
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
			// Live-ish growing playlist from FFmpeg; don't treat gaps as fatal.
			maxBufferHole: 1.5
		});
		hls.on(Hls.Events.ERROR, (_event, data) => {
			if (!data.fatal || destroyed) return;
			if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
				hls?.startLoad();
			} else if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
				hls?.recoverMediaError();
			}
		});
		hls.loadSource(url);
		hls.attachMedia(video);
	};

	const onSeeking = () => {
		if (destroyed) return;
		if (performance.now() < ignoreSeekUntil) return;
		const t = video.currentTime;
		if (inBufferedRange(video, t)) return;
		// Only restart when the user jumped clearly past what we have encoded.
		const absoluteMs = startMs + Math.floor(t * 1000);
		if (Math.abs(absoluteMs - startMs) < 500) return;
		load(absoluteMs);
		try {
			video.currentTime = 0;
		} catch {
			/* ignore */
		}
	};

	video.addEventListener('seeking', onSeeking);
	load(0);

	return {
		destroy: () => {
			destroyed = true;
			video.removeEventListener('seeking', onSeeking);
			if (hls) {
				hls.destroy();
				hls = null;
			}
			video.removeAttribute('src');
			video.load();
		}
	};
}
