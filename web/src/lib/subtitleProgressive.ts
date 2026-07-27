import type { components } from '$lib/api/schema';

type PlaybackInfo = components['schemas']['PlaybackInfo'];
type SubtitleTrack = components['schemas']['SubtitleTrack'];

/** Cache-busting src so a `<track>` reload fetches the grown WebVTT. */
export function subtitleTrackSrc(track: SubtitleTrack): string | null {
	if (!track.url) return null;
	const rev = track.revision ?? 0;
	const sep = track.url.includes('?') ? '&' : '?';
	return `${track.url}${sep}r=${rev}`;
}

/**
 * Reload one HTMLTrackElement without calling video.load() (that would
 * reset playback). Remembers showing/hidden mode; Firefox needs a mode
 * toggle after a fresh track (ADR-0013 growing-`<track>` experiment), and
 * often skips the track `load` event — poll until cues exist.
 */
export function reloadTextTrackElement(
	video: HTMLVideoElement,
	trackEl: HTMLTrackElement,
	nextSrc: string
): HTMLTrackElement {
	const wasDefault = trackEl.default;
	const srclang = trackEl.srclang;
	const label = trackEl.label;
	const kind = trackEl.kind || 'subtitles';
	const prevMode = trackEl.track?.mode ?? 'disabled';

	trackEl.remove();
	const next = document.createElement('track');
	next.kind = kind;
	next.srclang = srclang;
	next.label = label;
	next.default = wasDefault;
	next.src = nextSrc;
	video.appendChild(next);

	const applyMode = (): boolean => {
		const tt = next.track;
		if (!tt) return false;
		// Firefox leaves cues null while mode is disabled; leave disabled
		// briefly only for the measured toggle, otherwise use hidden to load.
		if (tt.mode === 'disabled' && tt.cues == null) {
			tt.mode = 'hidden';
		}
		if (tt.cues == null) return false;
		if (prevMode === 'showing') {
			tt.mode = 'disabled';
			tt.mode = 'showing';
		} else {
			tt.mode = prevMode;
		}
		return true;
	};

	if (!applyMode()) {
		next.addEventListener(
			'load',
			() => {
				applyMode();
			},
			{ once: true }
		);
		const t0 = performance.now();
		const poll = () => {
			if (applyMode()) return;
			if (performance.now() - t0 < 4000) requestAnimationFrame(poll);
		};
		requestAnimationFrame(poll);
	}
	return next;
}

export function needsSubtitleWatch(info: PlaybackInfo): boolean {
	return (info.subtitleTracks ?? []).some(
		(t) => t.readiness === 'preparing' || t.readiness === 'partial'
	);
}

/**
 * Direct-play progressive captions (ADR-0013 §11). Re-reads playbackInfo
 * only while the server still reports preparing/partial — readiness is
 * never invented client-side. Reloads `<track>` when revision grows.
 */
export function watchProgressiveSubtitles(opts: {
	video: HTMLVideoElement;
	initial: PlaybackInfo;
	fetchPlaybackInfo: () => Promise<PlaybackInfo>;
	isAlive: () => boolean;
	onPlaybackInfo?: (info: PlaybackInfo) => void;
	/** Interval used only to ask the server again while incomplete. */
	pollMs?: number;
}): { destroy: () => void } {
	const pollMs = opts.pollMs ?? 1000;
	let destroyed = false;
	let timer: ReturnType<typeof setTimeout> | null = null;
	const lastRevision = new Map<string, number>();

	const syncTracks = (info: PlaybackInfo) => {
		const serveable = (info.subtitleTracks ?? []).filter((t) => t.url);
		const existing = Array.from(opts.video.querySelectorAll('track'));
		for (const track of serveable) {
			const src = subtitleTrackSrc(track);
			if (!src) continue;
			const rev = track.revision ?? 0;
			const prev = lastRevision.get(track.trackId);
			const el = existing.find(
				(t) => t.getAttribute('data-track-id') === track.trackId
			) as HTMLTrackElement | undefined;
			if (!el) {
				const created = document.createElement('track');
				created.kind = 'subtitles';
				created.srclang = track.language ?? 'und';
				created.label = track.label ?? track.language ?? `Subtitles ${track.trackId}`;
				created.default = existing.length === 0 && serveable[0] === track;
				created.src = src;
				created.setAttribute('data-track-id', track.trackId);
				opts.video.appendChild(created);
				lastRevision.set(track.trackId, rev);
				continue;
			}
			if (prev != null && rev > prev) {
				const reloaded = reloadTextTrackElement(opts.video, el, src);
				reloaded.setAttribute('data-track-id', track.trackId);
				lastRevision.set(track.trackId, rev);
			} else if (prev == null) {
				lastRevision.set(track.trackId, rev);
			}
		}
	};

	const tick = async () => {
		if (destroyed || !opts.isAlive()) return;
		try {
			const info = await opts.fetchPlaybackInfo();
			if (destroyed || !opts.isAlive()) return;
			opts.onPlaybackInfo?.(info);
			syncTracks(info);
			if (needsSubtitleWatch(info)) {
				timer = setTimeout(() => void tick(), pollMs);
			}
		} catch {
			if (!destroyed && opts.isAlive()) {
				timer = setTimeout(() => void tick(), pollMs);
			}
		}
	};

	syncTracks(opts.initial);
	if (needsSubtitleWatch(opts.initial)) {
		timer = setTimeout(() => void tick(), pollMs);
	}

	return {
		destroy: () => {
			destroyed = true;
			if (timer != null) clearTimeout(timer);
		}
	};
}
