import Hls from 'hls.js';

export {
	needsSubtitleWatch,
	reloadTextTrackElement,
	subtitleTrackSrc,
	watchProgressiveSubtitles
} from './subtitleProgressive';

// Temporary dogfood marker: proves this bundle is what Chrome loaded.
console.warn('[nj-subs] hlsPlayer module loaded');

/**
 * Exactly two attach backends. The server contract is identical for both
 * (full-title VOD, 503 while cold regions cook, ADR-0011): no user-agent
 * branching on the server. Differences live only here.
 *
 * - `native-hls`: Apple WebKit (Safari / iOS). Chosen by engine check, not
 *   `canPlayType` alone — Chromium returns `"maybe"` for the HLS MIME and
 *   must not take this path. Hardware path; `#t=` land before first segment.
 * - `hls-js`: Chromium / Firefox MSE. Captions: EXT-X-MEDIA SUBTITLES.
 */
type AttachBackend = 'native-hls' | 'hls-js';

export function canPlayNativeHls(video: HTMLVideoElement): boolean {
	return video.canPlayType('application/vnd.apple.mpegurl') !== '';
}

/**
 * True for Apple WebKit browsers that actually ship HLS (Safari, iOS
 * WebViews). False for Chromium — including Chrome, Edge, and Chrome iOS
 * (`CriOS`) — even when `canPlayType('application/vnd.apple.mpegurl')` is
 * non-empty. Chrome 142+ returns `"maybe"` for that MIME and will take a
 * native path that is not WebKit HLS (no reliable EXT-X-MEDIA text).
 */
function isAppleWebKitHlsEngine(): boolean {
	if (typeof navigator === 'undefined') return false;
	const ua = navigator.userAgent;
	// Chromium family (desktop + iOS wrappers) must use hls.js when MSE works.
	if (/Chrom(e|ium)|Edg\/|OPR\/|CriOS|EdgiOS|FxiOS/i.test(ua)) {
		return false;
	}
	// Desktop/iOS Safari, and other Apple WebKit without a Chromium brand.
	return /Safari/i.test(ua) || /AppleWebKit/i.test(ua);
}

/**
 * Backend pick is the only client UA/engine fork (ADR-0011: server stays
 * identical). Do not trust `canPlayType` alone — Chromium lies with `"maybe"`.
 *
 * - Apple WebKit + canPlay HLS → native (required on iOS; product path on
 *   desktop Safari).
 * - Else hls.js when MSE is available (Chrome, Firefox, Edge, …).
 * - Else native if canPlayType still claims HLS (odd WebViews).
 */
function pickBackend(video: HTMLVideoElement): AttachBackend {
	if (isAppleWebKitHlsEngine() && canPlayNativeHls(video)) {
		return 'native-hls';
	}
	if (Hls.isSupported()) {
		return 'hls-js';
	}
	if (canPlayNativeHls(video)) {
		return 'native-hls';
	}
	throw new Error('HLS playback is not supported in this browser');
}

export interface HlsHandle {
	destroy: () => void;
	/** Title-absolute playback position in seconds. */
	positionSeconds: () => number;
	/**
	 * Enable HLS/native text track `index`, or `-1` to turn captions off.
	 * Chrome MSE does not list HLS TEXT tracks in the native CC menu; the
	 * item page Subtitles control drives this.
	 */
	setSubtitleTrack: (index: number) => void;
}

/**
 * Stop audible playback before a session cutover (audio switch). Leaves the
 * element without a source so the old track cannot keep playing while the
 * new session cooks — the item page shows a loading state until attach.
 */
export function parkForSwitch(video: HTMLVideoElement | null, handle: HlsHandle | null) {
	handle?.destroy();
	if (!video) return;
	video.pause();
	video.removeAttribute('src');
	video.load();
}

function logSubs(phase: string, detail?: Record<string, unknown>) {
	if (detail) {
		console.info(`[nj-subs] ${phase}`, detail);
	} else {
		console.info(`[nj-subs] ${phase}`);
	}
}

function logSubtitleState(phase: string, hls: Hls) {
	const tracks = hls.subtitleTracks;
	const level0 = hls.levels[0];
	const first = tracks[0];
	logSubs(phase, {
		trackCount: tracks.length,
		selected: hls.subtitleTrack,
		subtitleGroups: level0?.subtitleGroups ?? null,
		firstUrl: first?.url ?? null,
		firstName: first?.name ?? null
	});
}

function logNativeTracks(phase: string, video: HTMLVideoElement, extra?: Record<string, unknown>) {
	const list = video.textTracks;
	const parts: string[] = [];
	for (let i = 0; i < list.length; i++) {
		const t = list[i];
		if (!t) continue;
		parts.push(
			`#${i} ${t.label || t.language || t.kind} mode=${t.mode} cues=${t.cues?.length ?? 'null'} active=${t.activeCues?.length ?? 'null'}`
		);
	}
	const wanted = extra?.wantedSubtitle;
	const summary =
		parts.length > 0
			? parts.join(' | ')
			: 'no textTracks';
	logSubs(
		`${phase} t=${video.currentTime.toFixed(2)} wanted=${wanted ?? '?'} [${summary}]`,
		extra
	);
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
	// Applied when MEDIA tracks appear (never assign while length === 0 —
	// that clears hls.js selectDefaultTrack and can leave zero fetches).
	let wantedSubtitle = 0;

	const backend = pickBackend(video);
	logSubs('backend', {
		backend,
		canPlayType: video.canPlayType('application/vnd.apple.mpegurl'),
		hlsJsSupported: Hls.isSupported(),
		appleWebKitHls: isAppleWebKitHlsEngine()
	});

	const positionSeconds = (): number => Math.max(0, video.currentTime);

	const applyWantedSubtitle = () => {
		if (destroyed) return;
		if (hls) {
			if (hls.subtitleTracks.length === 0) {
				logSubtitleState('skip select — no tracks yet', hls);
				return;
			}
			if (wantedSubtitle < 0) {
				hls.subtitleDisplay = false;
				hls.subtitleTrack = -1;
				logSubtitleState('captions off', hls);
				return;
			}
			const idx = Math.min(wantedSubtitle, hls.subtitleTracks.length - 1);
			hls.subtitleDisplay = true;
			hls.subtitleTrack = idx;
			logSubtitleState(`select track ${idx}`, hls);
			return;
		}
		const list = video.textTracks;
		if (list.length === 0) {
			logNativeTracks('native apply — no textTracks yet', video, {
				wantedSubtitle
			});
			return;
		}
		for (let i = 0; i < list.length; i++) {
			const track = list[i];
			if (!track) continue;
			track.mode = i === wantedSubtitle ? 'showing' : 'disabled';
		}
		logNativeTracks('native apply', video, { wantedSubtitle });
	};

	/** After scrub, re-select the track and sync subtitle loading to the
	 *  playhead. hls.js: seek can finish before SubtitleStreamController
	 *  sees onMediaSeeking, so startLoad retargets TEXT frags. Native
	 *  Safari reassert is rewritten separately (mode bounce/dwell patches
	 *  failed dogfood); leave DEFAULT=YES alone here. */
	const reassertSubtitleAfterSeek = () => {
		if (destroyed || wantedSubtitle < 0) return;
		if (!hls) {
			logNativeTracks('seek native — no reassert (pending rewrite)', video, {
				wantedSubtitle
			});
			return;
		}
		const idx = Math.min(wantedSubtitle, Math.max(0, hls.subtitleTracks.length - 1));
		const t = video.currentTime;
		logSubs('seek reassert hls.js', { idx, currentTime: t });
		hls.subtitleTrack = -1;
		requestAnimationFrame(() => {
			if (destroyed || !hls || wantedSubtitle < 0) return;
			hls.subtitleDisplay = true;
			hls.subtitleTrack = idx;
			// startLoad(position, skipSeek): subtitle SC resets nextLoadPosition
			// and ticks; skipSeek avoids yanking the main timeline to config start.
			hls.startLoad(t, true);
			logSubs('seek startLoad subtitles', { currentTime: t });
		});
	};

	const onAddTrack = () => {
		logNativeTracks('native addtrack', video, { wantedSubtitle });
		if (wantedSubtitle < 0) applyWantedSubtitle();
	};

	const onTextTrackChange = () => {
		logNativeTracks('native textTracks change', video, { wantedSubtitle });
	};

	const onSeeked = () => {
		if (destroyed || seekInFlight) return;
		if (suppressSeekNotify) {
			suppressSeekNotify = false;
			if (hls) reassertSubtitleAfterSeek();
			else {
				logNativeTracks('seek suppress — attach land', video, { wantedSubtitle });
			}
			return;
		}
		// Native Safari: no master?startMs= — segment GETs move the window
		// (ADR-0011); a fetch here can restart encode and abort TEXT load.
		if (!hls) {
			reassertSubtitleAfterSeek();
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
				reassertSubtitleAfterSeek();
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

	if (backend === 'native-hls') {
		logSubs('native-hls attach — filter console for [nj-subs]');
		// Media fragment asks Safari to begin at the offset before any
		// segment fetch. loadedmetadata + currentTime is a backup only.
		video.src =
			startAtSeconds > 0 ? `${playlistBase}#t=${startAtSeconds}` : playlistBase;
		if (startAtSeconds > 0) {
			video.addEventListener(
				'loadedmetadata',
				() => {
					if (!destroyed && Math.abs(video.currentTime - startAtSeconds) > 1) {
						video.currentTime = startAtSeconds;
					}
					logNativeTracks('native loadedmetadata', video, { wantedSubtitle });
				},
				{ once: true }
			);
		} else {
			video.addEventListener(
				'loadedmetadata',
				() => logNativeTracks('native loadedmetadata', video, { wantedSubtitle }),
				{ once: true }
			);
		}
		// Do not force mode=showing on attach: master DEFAULT=YES already
		// enables the track. Forcing showing on an auto-selected HLS track
		// has been seen to paint cues twice in Safari.
		video.textTracks.addEventListener('addtrack', onAddTrack);
		video.textTracks.addEventListener('change', onTextTrackChange);
	} else {
		hls = new Hls({
			enableWorker: true,
			maxBufferHole: 1.5,
			startPosition: startAtSeconds > 0 ? startAtSeconds : -1
		});
		logSubs('hls.js attach — filter console for [nj-subs]');
		hls.on(Hls.Events.ERROR, (_event, data) => {
			if (destroyed) return;
			const details = String(data.details ?? '');
			if (details.toLowerCase().includes('subtitle') || data.url?.includes('/subs/')) {
				logSubs('error', {
					fatal: data.fatal,
					type: data.type,
					details: data.details,
					url: data.url ?? null
				});
			}
			if (!data.fatal) return;
			if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
				hls?.startLoad();
			} else if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
				hls?.recoverMediaError();
			}
		});
		hls.on(Hls.Events.MANIFEST_PARSED, () => {
			if (!hls) return;
			logSubtitleState('manifest parsed', hls);
			// tracksInGroup is often still empty here; do not assign.
		});
		hls.on(Hls.Events.SUBTITLE_TRACKS_UPDATED, () => {
			if (!hls) return;
			logSubtitleState('tracks updated', hls);
			applyWantedSubtitle();
		});
		hls.on(Hls.Events.SUBTITLE_TRACK_SWITCH, (_e, data) => {
			logSubs('track switch', { id: data.id, url: data.url ?? null, name: data.name ?? null });
		});
		hls.on(Hls.Events.SUBTITLE_TRACK_LOADING, (_e, data) => {
			logSubs('LOADING subtitle playlist', { url: data.url, id: data.id });
		});
		hls.on(Hls.Events.SUBTITLE_TRACK_LOADED, (_e, data) => {
			logSubs('LOADED subtitle playlist', {
				id: data.id,
				fragments: data.details?.fragments?.length ?? null
			});
		});
		hls.on(Hls.Events.FRAG_LOADING, (_e, data) => {
			if (data.frag?.type !== 'subtitle') return;
			logSubs('FRAG loading', {
				sn: data.frag.sn,
				start: data.frag.start,
				url: data.frag.url ?? null
			});
		});
		hls.on(Hls.Events.FRAG_LOADED, (_e, data) => {
			if (data.frag?.type !== 'subtitle') return;
			logSubs('FRAG loaded', { sn: data.frag.sn, start: data.frag.start });
		});
		hls.loadSource(playlistBase);
		hls.attachMedia(video);
	}

	const setSubtitleTrack = (index: number) => {
		wantedSubtitle = index;
		applyWantedSubtitle();
	};

	return {
		destroy: () => {
			if (destroyed) return;
			destroyed = true;
			video.removeEventListener('seeked', onSeeked);
			video.removeEventListener('canplay', onCanPlay);
			video.textTracks.removeEventListener('addtrack', onAddTrack);
			video.textTracks.removeEventListener('change', onTextTrackChange);
			if (hls) {
				hls.destroy();
				hls = null;
			}
			video.removeAttribute('src');
			video.load();
		},
		positionSeconds,
		setSubtitleTrack
	};
}
