import Hls from 'hls.js';
import { probeEnabled } from './latencyProbe';
import {
	parseSubtitleTrackIdsFromMaster,
	parseWebVttCues,
	segmentIndexAtSeconds,
	sessionBaseFromMaster,
	subtitleSegmentUrl
} from './nativeHlsSubs';

export {
	needsSubtitleWatch,
	reloadTextTrackElement,
	subtitleTrackSrc,
	watchProgressiveSubtitles
} from './subtitleProgressive';

/** `[nj-subs]` diagnostics — same opt-in as latency probe (`?njProbe=1`). */
function subsProbeOn(): boolean {
	if (typeof window === 'undefined') return false;
	return probeEnabled(window.location.search);
}

/**
 * Exactly two attach backends. The server contract is identical for both
 * (full-title VOD, 503 while cold regions cook, ADR-0011): no user-agent
 * branching on the server. Differences live only here.
 *
 * - `native-hls`: Apple WebKit (Safari / iOS). Chosen by engine check, not
 *   `canPlayType` alone — Chromium returns `"maybe"` for the HLS MIME and
 *   must not take this path. Hardware path; `#t=` land before first segment.
 *   After a user scrub, captions are injected from `subs/{id}/segNNN.vtt`
 *   (WebKit does not reload EXT-X-MEDIA TextTracks — ADR-0013).
 * - `hls-js`: Chromium / Firefox MSE. Captions: EXT-X-MEDIA SUBTITLES;
 *   scrub uses `startLoad` to retarget the subtitle stream controller.
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
	if (!subsProbeOn()) return;
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
 * 1. User finishes a scrub → `seeked` fires once.
 * 2. hls.js: GET playlist?startMs=T then subtitle `startLoad` reassert.
 * 3. Native Safari: no master?startMs= (segment GETs move the window,
 *    ADR-0011). Captions switch to manual VTT segment fetch + cue inject
 *    because WebKit does not reload EXT-X-MEDIA TextTracks after seek
 *    (ADR-0013).
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

	let positionSeconds = (): number => Math.max(0, video.currentTime);
	/** `performance.now()` of last user scrub (`seeked`); for [nj-scrub] gaps. */
	let lastScrubAtMs = 0;

	const logScrubRequested = () => {
		const now = performance.now();
		const priorMs = lastScrubAtMs > 0 ? Math.round(now - lastScrubAtMs) : null;
		console.info(
			`[nj-scrub] scrub requested t=${video.currentTime.toFixed(2)} priorMs=${priorMs ?? 'none'}`
		);
		lastScrubAtMs = now;
	};

	// --- Safari native: post-seek cue injection (ADR-0013) -----------------
	const sessionBase = sessionBaseFromMaster(playlistBase);
	let nativeInjectMode = false;
	let nativeInjectTrack: TextTrack | null = null;
	let nativeInjectGen = 0;
	const nativeLoadedSegs = new Set<number>();
	let nativeTrackIds: string[] | null = null;
	let nativeTrackIdsPromise: Promise<string[]> | null = null;
	let nativeInjectTimeHandler: (() => void) | null = null;

	const disableHlsTextTracks = () => {
		const list = video.textTracks;
		for (let i = 0; i < list.length; i++) {
			const t = list[i];
			if (!t || t === nativeInjectTrack) continue;
			if (t.mode !== 'disabled') t.mode = 'disabled';
		}
	};

	/**
	 * WebKit DEFAULT/AUTOSELECT keeps flipping the HLS MEDIA track back to
	 * `showing`, which demotes our inject track (only one subtitle track may
	 * show). Dogfood: after inject, logs showed `#0 showing | #1 disabled`
	 * while cues lived on #1. Re-assert only when wrong to avoid change loops.
	 */
	const assertInjectModes = () => {
		if (!nativeInjectMode || !nativeInjectTrack) return;
		disableHlsTextTracks();
		const want: TextTrackMode = wantedSubtitle >= 0 ? 'showing' : 'disabled';
		if (nativeInjectTrack.mode !== want) nativeInjectTrack.mode = want;
	};

	let injectModeAssertScheduled = false;
	const scheduleAssertInjectModes = () => {
		if (injectModeAssertScheduled) return;
		injectModeAssertScheduled = true;
		queueMicrotask(() => {
			injectModeAssertScheduled = false;
			if (destroyed || !nativeInjectMode) return;
			assertInjectModes();
		});
	};

	const clearInjectCues = (track: TextTrack) => {
		const cues = track.cues;
		if (!cues) return;
		for (let i = cues.length - 1; i >= 0; i--) {
			const c = cues[i];
			if (c) track.removeCue(c);
		}
	};

	const cancelNativeInject = () => {
		nativeInjectGen += 1;
	};

	const ensureNativeTrackIds = async (): Promise<string[]> => {
		if (nativeTrackIds) return nativeTrackIds;
		if (!nativeTrackIdsPromise) {
			nativeTrackIdsPromise = (async () => {
				const res = await fetch(playlistBase.split('#')[0] ?? playlistBase);
				if (!res.ok) {
					throw new Error(`master fetch ${res.status}`);
				}
				const body = await res.text();
				const ids = parseSubtitleTrackIdsFromMaster(body);
				nativeTrackIds = ids;
				logSubs('native inject master track ids', { ids });
				return ids;
			})().catch((err) => {
				nativeTrackIdsPromise = null;
				throw err;
			});
		}
		return nativeTrackIdsPromise;
	};

	const ensureInjectTrack = (index: number): TextTrack => {
		if (!nativeInjectTrack) {
			const src = video.textTracks[index];
			// Distinct label so WebKit does not merge with the HLS MEDIA track
			// in the CC menu / DEFAULT group.
			nativeInjectTrack = video.addTextTrack(
				'subtitles',
				src?.label ? `${src.label} (Nightjar)` : 'Subtitles',
				src?.language || 'en'
			);
			logSubs('native inject track created', {
				label: nativeInjectTrack.label,
				language: nativeInjectTrack.language
			});
		}
		assertInjectModes();
		return nativeInjectTrack;
	};

	const pruneInjectCues = (track: TextTrack, beforeSec: number) => {
		const cues = track.cues;
		if (!cues) return;
		for (let i = cues.length - 1; i >= 0; i--) {
			const c = cues[i];
			if (c && c.endTime < beforeSec) track.removeCue(c);
		}
	};

	const fetchAndInjectSegment = async (
		track: TextTrack,
		trackId: string,
		segIdx: number,
		gen: number
	) => {
		if (destroyed || gen !== nativeInjectGen) return;
		if (nativeLoadedSegs.has(segIdx)) return;
		const url = subtitleSegmentUrl(sessionBase, trackId, segIdx);
		let body: string;
		try {
			const res = await fetch(url);
			if (destroyed || gen !== nativeInjectGen) return;
			if (!res.ok) {
				logSubs('native inject segment network error', {
					segIdx,
					url,
					status: res.status
				});
				return;
			}
			body = await res.text();
		} catch (err) {
			logSubs('native inject segment network error', {
				segIdx,
				url,
				error: err instanceof Error ? err.message : String(err)
			});
			return;
		}
		if (destroyed || gen !== nativeInjectGen) return;

		let cues;
		try {
			cues = parseWebVttCues(body);
		} catch (err) {
			logSubs('native inject segment parse error', {
				segIdx,
				url,
				error: err instanceof Error ? err.message : String(err)
			});
			return;
		}

		logSubs('native inject segment fetched', {
			segIdx,
			url,
			parsed: cues.length
		});
		if (cues.length === 0) {
			nativeLoadedSegs.add(segIdx);
			return;
		}

		let injected = 0;
		for (const cue of cues) {
			try {
				const vtt = new VTTCue(cue.startSec, cue.endSec, cue.text);
				if (cue.id) vtt.id = cue.id;
				track.addCue(vtt);
				injected += 1;
			} catch (err) {
				logSubs('native inject addCue failure', {
					segIdx,
					id: cue.id ?? null,
					error: err instanceof Error ? err.message : String(err)
				});
			}
		}
		nativeLoadedSegs.add(segIdx);
		logSubs('native inject cues injected', {
			segIdx,
			injected,
			trackCues: track.cues?.length ?? 0
		});
	};

	const syncNativeInject = async (reset: boolean) => {
		if (destroyed || !nativeInjectMode || wantedSubtitle < 0) return;
		const gen = nativeInjectGen;
		let trackIds: string[];
		try {
			trackIds = await ensureNativeTrackIds();
		} catch (err) {
			logSubs('native inject master fetch error', {
				error: err instanceof Error ? err.message : String(err)
			});
			return;
		}
		if (destroyed || gen !== nativeInjectGen) return;

		const idx = Math.min(wantedSubtitle, Math.max(0, trackIds.length - 1));
		const trackId = trackIds[idx];
		if (!trackId) {
			logSubs('native inject no track id', { wantedSubtitle: idx });
			return;
		}

		const track = ensureInjectTrack(idx);
		if (reset) {
			clearInjectCues(track);
			nativeLoadedSegs.clear();
			logSubs('native inject reset for scrub', {
				t: video.currentTime,
				trackId
			});
		} else {
			pruneInjectCues(track, video.currentTime - 30);
		}

		const seg = segmentIndexAtSeconds(video.currentTime);
		await fetchAndInjectSegment(track, trackId, seg, gen);
		await fetchAndInjectSegment(track, trackId, seg + 1, gen);
	};

	const armNativeInjectTimeupdate = () => {
		if (nativeInjectTimeHandler) return;
		nativeInjectTimeHandler = () => {
			if (destroyed || !nativeInjectMode || wantedSubtitle < 0) return;
			void syncNativeInject(false);
		};
		video.addEventListener('timeupdate', nativeInjectTimeHandler);
	};

	const disarmNativeInjectTimeupdate = () => {
		if (!nativeInjectTimeHandler) return;
		video.removeEventListener('timeupdate', nativeInjectTimeHandler);
		nativeInjectTimeHandler = null;
	};

	const enterNativeInjectMode = () => {
		if (nativeInjectMode) {
			cancelNativeInject();
			void syncNativeInject(true);
			return;
		}
		nativeInjectMode = true;
		cancelNativeInject();
		disableHlsTextTracks();
		armNativeInjectTimeupdate();
		logSubs('native inject mode on', { t: video.currentTime });
		void syncNativeInject(true);
	};

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
		if (nativeInjectMode) {
			disableHlsTextTracks();
			if (wantedSubtitle < 0) {
				if (nativeInjectTrack) {
					clearInjectCues(nativeInjectTrack);
					nativeInjectTrack.mode = 'disabled';
				}
				nativeLoadedSegs.clear();
				logSubs('native inject captions off');
				return;
			}
			void syncNativeInject(true);
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

	/** hls.js only: retarget subtitle fragments after scrub. */
	const reassertHlsSubtitleAfterSeek = () => {
		if (destroyed || !hls || wantedSubtitle < 0) return;
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
		if (nativeInjectMode) {
			scheduleAssertInjectModes();
			return;
		}
		if (wantedSubtitle < 0) applyWantedSubtitle();
	};

	const onTextTrackChange = () => {
		logNativeTracks('native textTracks change', video, { wantedSubtitle });
		if (nativeInjectMode) scheduleAssertInjectModes();
	};

	const onSeeked = () => {
		if (destroyed || seekInFlight) return;
		if (suppressSeekNotify) {
			suppressSeekNotify = false;
			if (hls) reassertHlsSubtitleAfterSeek();
			else {
				logNativeTracks('seek suppress — attach land', video, { wantedSubtitle });
			}
			return;
		}
		// Before any startMs / inject network work — one line for scrub pacing.
		logScrubRequested();
		if (!hls) {
			if (wantedSubtitle >= 0) enterNativeInjectMode();
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
				reassertHlsSubtitleAfterSeek();
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
		cancelNativeInject();
		wantedSubtitle = index;
		applyWantedSubtitle();
	};

	return {
		destroy: () => {
			if (destroyed) return;
			destroyed = true;
			cancelNativeInject();
			disarmNativeInjectTimeupdate();
			video.removeEventListener('seeked', onSeeked);
			video.removeEventListener('canplay', onCanPlay);
			video.textTracks.removeEventListener('addtrack', onAddTrack);
			video.textTracks.removeEventListener('change', onTextTrackChange);
			if (nativeInjectTrack) {
				clearInjectCues(nativeInjectTrack);
				nativeInjectTrack.mode = 'disabled';
				nativeInjectTrack = null;
			}
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
