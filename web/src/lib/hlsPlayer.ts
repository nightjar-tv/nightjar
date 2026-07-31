import Hls from 'hls.js';
import {
	chooseAttachBackend,
	seekSuppressOnSeeked,
	seekSuppressRafMayClear,
	type AttachBackend
} from './hlsAttachBackend';
import { probeEnabled } from './latencyProbe';
import { api } from './api/client';
import {
	mediaSecondsFromTitle,
	mediaTimeInProducedWindow,
	titleSecondsFromMedia
} from './hlsTimeline';
import {
	applyAbsoluteCueTimesFromVtt,
	parseSubtitleTrackIdsFromMaster,
	parseWebVttCues,
	segmentIndexAtSeconds,
	sessionBaseFromMaster,
	sessionIdFromPlaylist,
	subtitleSegmentUrl
} from './nativeHlsSubs';

/** Slightly above server `SEGMENT_WAIT` (30s) so the server can 503 first. */
const LAND_SEGMENT_FETCH_MS = 32_000;
/** Pause between land-ensure attempts so 503 retries do not spin hot. */
const LAND_ENSURE_BACKOFF_MS = 400;
/** If land-retarget seeked never arrives, clear suppress (stuck-flag safety). */
const SEEK_SUPPRESS_TIMEOUT_MS = 2_000;

export {
	watchProgressiveSubtitles
} from './subtitleProgressive';

/** `[nj-subs]` diagnostics — same opt-in as latency probe (`?njProbe=1`). */
function subsProbeOn(): boolean {
	if (typeof window === 'undefined') return false;
	return probeEnabled(window.location.search);
}

function canPlayNativeHls(video: HTMLVideoElement): boolean {
	return video.canPlayType('application/vnd.apple.mpegurl') !== '';
}

/** Debug escape: `?njNativeHls=1` forces native on desktop Apple WebKit. */
function forceNativeHlsOverride(): boolean {
	if (typeof window === 'undefined') return false;
	return new URLSearchParams(window.location.search).get('njNativeHls') === '1';
}

/**
 * Backend pick is the only client UA/engine fork (ADR-0011: server stays
 * identical). Do not trust `canPlayType` alone — Chromium lies with `"maybe"`.
 *
 * Decision tree (ADR-0017):
 * - `?njNativeHls=1` + Apple WebKit canPlay → native (desktop regression hatch)
 * - iOS/iPadOS Apple WebKit + canPlay → native
 * - Else hls.js when MSE works (desktop Safari, Chrome, Firefox, …)
 * - Else native if canPlay still claims HLS (odd WebViews)
 */
function pickBackend(video: HTMLVideoElement): AttachBackend {
	const ua = typeof navigator !== 'undefined' ? navigator.userAgent : '';
	const maxTouchPoints =
		typeof navigator !== 'undefined' ? navigator.maxTouchPoints || 0 : 0;
	return chooseAttachBackend({
		ua,
		maxTouchPoints,
		canPlayNativeHls: canPlayNativeHls(video),
		hlsJsSupported: Hls.isSupported(),
		forceNativeHls: forceNativeHlsOverride()
	});
}

export interface HlsHandle {
	destroy: () => void;
	/** Title-absolute playback position in seconds (live `landedMs` + media). */
	positionSeconds: () => number;
	/**
	 * Scrub to a title-absolute time. In-window → `currentTime`; otherwise
	 * POST /seek and swap the playlist URI (ADR-0020). Resolves when the
	 * land is applied (or immediately on the in-window path).
	 */
	seekToTitleSeconds: (titleSeconds: number) => Promise<void>;

	/**
	 * Enable HLS/native text track `index`, or `-1` to turn captions off.
	 * Chrome MSE does not list HLS TEXT tracks in the native CC menu; the
	 * item page Subtitles control drives this.
	 */
	setSubtitleTrack: (index: number) => void;
}

function logSubs(phase: string, detail?: Record<string, unknown>) {
	if (!subsProbeOn()) return;
	// One string — Safari collapses a second-arg object to "Object".
	if (!detail) {
		console.info(`[nj-subs] ${phase}`);
		return;
	}
	const flat = Object.entries(detail)
		.map(([k, v]) => {
			if (v === null || v === undefined) return `${k}=null`;
			if (typeof v === 'object') return `${k}=${JSON.stringify(v)}`;
			return `${k}=${String(v)}`;
		})
		.join(' ');
	console.info(`[nj-subs] ${phase} ${flat}`);
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
 * hls.js SubtitleStreamController keeps `tracksBuffered` across seeks so a
 * later startLoad can believe the playhead is already covered. Clear that
 * map + subtitle fragment-tracker entries before retargeting.
 * Coupled to hls.js 1.6 controller shape; probe if upgrading major.
 */
function clearHlsSubtitleBufferedState(hls: Hls): void {
	type SubCtrl = {
		tracksBuffered?: Array<Array<{ start: number; end: number }>>;
		fragmentTracker?: {
			removeFragmentsInRange: (start: number, end: number, type: string) => void;
		};
	};
	const controllers = (hls as unknown as { networkControllers?: SubCtrl[] })
		.networkControllers;
	if (!controllers) return;
	for (const c of controllers) {
		if (!Array.isArray(c.tracksBuffered)) continue;
		for (let i = 0; i < c.tracksBuffered.length; i++) {
			c.tracksBuffered[i] = [];
		}
		c.fragmentTracker?.removeFragmentsInRange(
			0,
			Number.POSITIVE_INFINITY,
			'subtitle'
		);
	}
}

/** Subtitle SC only — `hls.startLoad` also restarts the main A/V loader. */
function startHlsSubtitleLoad(hls: Hls, positionSec: number): boolean {
	type SubCtrl = {
		tracksBuffered?: unknown;
		startLoad?: (pos: number, skipSeek?: boolean) => void;
	};
	const controllers = (hls as unknown as { networkControllers?: SubCtrl[] })
		.networkControllers;
	if (!controllers) return false;
	let found = false;
	for (const c of controllers) {
		if (!Array.isArray(c.tracksBuffered)) continue;
		c.startLoad?.(positionSec, true);
		found = true;
	}
	return found;
}

function clearVideoSubtitleCues(video: HTMLVideoElement): void {
	const list = video.textTracks;
	for (let i = 0; i < list.length; i++) {
		const track = list[i];
		if (!track) continue;
		if (track.kind !== 'subtitles' && track.kind !== 'captions') continue;
		const cues = track.cues;
		if (!cues?.length) continue;
		for (let j = cues.length - 1; j >= 0; j--) {
			const cue = cues[j];
			if (cue) track.removeCue(cue);
		}
	}
}

/**
 * Attach an HLS EVENT playlist (ADR-0020).
 *
 * Media element time is window-relative: `currentTime` 0 is the run land, not
 * title 0. Title position is `landedMs/1000 + currentTime`. `landedMs` mutates
 * on every run swap — never cache it at first attach for position/scrub/subs.
 *
 * Far scrub: POST /sessions/{id}/seek → fresh playlistUri → source swap.
 * In-window scrub: set `currentTime` only. Title scrub UI calls
 * `seekToTitleSeconds`; native control seeks stay media-relative and do not
 * POST.
 *
 * Subtitles: wire VTT is title-absolute; paint times are media-relative
 * (`title − land`). Segment index uses title seconds. After mid-title land,
 * cue inject covers both backends (hls.js media time would otherwise load
 * the wrong full-title subtitle frags).
 */
export function attachHls(
	video: HTMLVideoElement,
	playlistBase: string,
	startAtSeconds = 0
): HlsHandle {
	let hls: Hls | null = null;
	let destroyed = false;
	/**
	 * Seek-notify suppress generation. Non-zero = ignore user scrub handlers.
	 * Bumped on each arm so a stale rAF/timeout cannot clear a newer suppress.
	 * Replaces a boolean that could stick true if `currentTime = t` fired no
	 * seeked (same-value / no-op path).
	 */
	let seekSuppressGen = 0;
	let seekSuppressTimeout: ReturnType<typeof setTimeout> | null = null;
	// Applied when MEDIA tracks appear (never assign while length === 0 —
	// that clears hls.js selectDefaultTrack and can leave zero fetches).
	let wantedSubtitle = 0;
	/** Probe: only log subtitle FRAG lines until this time (after seek reassert). */
	let fragProbeUntilMs = 0;
	/** Last subtitle reassert segment — echo seeked at same land must not wipe cues. */
	let lastSubtitleReassertSeg = -1;

	const backend = pickBackend(video);
	logSubs('backend', {
		backend,
		canPlayType: video.canPlayType('application/vnd.apple.mpegurl'),
		hlsJsSupported: Hls.isSupported(),
		forceNativeHls: forceNativeHlsOverride()
	});

	/**
	 * Producer land for the **current** run (title-absolute ms). Updated on
	 * every seek response — do not close over attach-time only.
	 */
	let landedMs = Math.max(0, Math.floor(startAtSeconds * 1000));
	let positionSeconds = (): number =>
		titleSecondsFromMedia(video.currentTime, landedMs);
	/** Abort in-flight far-seek when a newer scrub arrives. */
	let landEnsureAbort: AbortController | null = null;
	/** startMs with an in-flight seek (blocks identical echoes). */
	let landEnsureSegIdx: number | null = null;
	/** Last startMs we successfully sought to. */
	let landEnsuredSegIdx: number | null = null;
	/**
	 * After source swap / land nudge: first suppressed seeked completes the
	 * nudge; second applies this media time + play(). null = not retargeting.
	 */
	let landRetargetSeconds: number | null = null;
	let currentPlaylist = playlistBase;
	let sessionBase = sessionBaseFromMaster(currentPlaylist);
	/** Last startMs we told the seek API — skip identical echoes. */
	let lastStartMsSent: number | null = null;

	const seekSuppressActive = () => seekSuppressGen !== 0;

	const clearSeekSuppress = () => {
		seekSuppressGen = 0;
		landRetargetSeconds = null;
		if (seekSuppressTimeout !== null) {
			window.clearTimeout(seekSuppressTimeout);
			seekSuppressTimeout = null;
		}
	};

	const armSeekSuppress = () => {
		seekSuppressGen += 1;
		const gen = seekSuppressGen;
		if (seekSuppressTimeout !== null) {
			window.clearTimeout(seekSuppressTimeout);
			seekSuppressTimeout = null;
		}
		requestAnimationFrame(() => {
			requestAnimationFrame(() => {
				if (
					!seekSuppressRafMayClear(
						gen,
						seekSuppressGen,
						landRetargetSeconds !== null
					)
				) {
					if (gen === seekSuppressGen && landRetargetSeconds !== null) {
						seekSuppressTimeout = window.setTimeout(() => {
							if (gen === seekSuppressGen) clearSeekSuppress();
						}, SEEK_SUPPRESS_TIMEOUT_MS);
					}
					return;
				}
				clearSeekSuppress();
			});
		});
	};

	const nudgePlayheadToMedia = (mediaSeconds: number) => {
		const t = Math.max(0, mediaSeconds);
		armSeekSuppress();
		landRetargetSeconds = t;
		if (Math.abs(video.currentTime - t) < 0.05) {
			video.currentTime = t >= 0.05 ? t - 0.05 : t + 0.05;
		} else {
			video.currentTime = t;
		}
		void video.play().catch(() => {});
	};

	/**
	 * Far scrub (ADR-0020): session seek API → fresh playlist URI → source
	 * swap. Clients must not construct segment URLs. Track selections are
	 * re-applied after the swap. New run media time starts at 0.
	 */
	const swapToPlaylist = (url: string, nextLandedMs: number) => {
		landedMs = Math.max(0, Math.floor(nextLandedMs));
		currentPlaylist = url;
		sessionBase = sessionBaseFromMaster(url);
		nativeTrackIds = null;
		nativeTrackIdsPromise = null;
		const wanted = wantedSubtitle;
		if (hls) {
			hls.loadSource(url);
			hls.startLoad(0);
			// loadSource leaves the element paused; without play() far scrub
			// shows a land frame and never resumes (dogfood item 33).
			void video.play().catch(() => {});
		} else {
			armSeekSuppress();
			landRetargetSeconds = 0;
			video.src = url;
			void video.play().catch(() => {});
		}
		wantedSubtitle = wanted;
		if (landedMs > 0 && wantedSubtitle >= 0) {
			enterNativeInjectMode();
		} else {
			applyWantedSubtitle();
		}
	};

	const seekToTitleSeconds = (titleSeconds: number): Promise<void> => {
		if (destroyed) return Promise.resolve();
		const startMs = Math.max(0, Math.floor(titleSeconds * 1000));
		const landSec = Math.max(0, landedMs) / 1000;
		const media = mediaSecondsFromTitle(titleSeconds, landedMs);
		// Fast path only inside the current run's produced media. A title
		// before `landedMs` clamps media to 0 and must POST /seek — otherwise
		// scrub-back stays stuck at the land (dogfood: 600 → 120).
		const beforeLand = titleSeconds + 0.05 < landSec;
		if (
			!beforeLand &&
			mediaTimeInProducedWindow(
				media,
				video.seekable,
				video.buffered,
				video.duration
			)
		) {
			nudgePlayheadToMedia(media);
			if (wantedSubtitle >= 0 && landedMs > 0) enterNativeInjectMode();
			else if (hls && wantedSubtitle >= 0) reassertHlsSubtitleAfterSeek();
			return Promise.resolve();
		}
		if (lastStartMsSent === startMs) return Promise.resolve();
		lastStartMsSent = startMs;
		const sid = sessionIdFromPlaylist(currentPlaylist);
		if (!sid) return Promise.resolve();
		if (
			landEnsureSegIdx === startMs &&
			landEnsureAbort &&
			!landEnsureAbort.signal.aborted
		) {
			return Promise.resolve();
		}
		landEnsureAbort?.abort();
		const parent = new AbortController();
		landEnsureAbort = parent;
		landEnsureSegIdx = startMs;
		landRetargetSeconds = null;
		// Stop the live run's reload timer before teardown so hls.js does not
		// keep GETting runs/{old}/index.m3u8 into 404 (measured: exact URL).
		if (hls) {
			try {
				hls.stopLoad();
			} catch {
				// ignore
			}
		}
		return (async () => {
			try {
				const view = await api.seekTranscodeSession(sid, startMs);
				if (destroyed || parent.signal.aborted) return;
				const deadline = Date.now() + LAND_SEGMENT_FETCH_MS;
				const indexUrl = view.playlistUrl.replace(/master\.m3u8$/i, 'index.m3u8');
				while (!destroyed && !parent.signal.aborted && Date.now() < deadline) {
					// Wait for media playlist, not only master — master can 200
					// while index is still NotReady after a run swap.
					const res = await fetch(indexUrl, { signal: parent.signal });
					if (res.ok) break;
					await new Promise((r) => setTimeout(r, LAND_ENSURE_BACKOFF_MS));
				}
				if (destroyed || parent.signal.aborted) return;
				landEnsuredSegIdx = startMs;
				swapToPlaylist(view.playlistUrl, view.landedMs);
			} catch {
				// Network / abort: next scrub retries.
			} finally {
				if (landEnsureAbort === parent) {
					landEnsureAbort = null;
					landEnsureSegIdx = null;
				}
			}
		})();
	};

		// --- Safari native: post-seek cue injection (ADR-0013) -----------------
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
		// Clear immediately so a later sync(true) await cannot race with
		// timeupdate leaving prior-land cues on screen (dogfood: stacked
		// captions after scrub nudge until the next line).
		if (nativeInjectTrack) clearInjectCues(nativeInjectTrack);
		nativeLoadedSegs.clear();
		disableHlsTextTracks();
	};

	const ensureNativeTrackIds = async (): Promise<string[]> => {
		if (nativeTrackIds) return nativeTrackIds;
		if (!nativeTrackIdsPromise) {
			nativeTrackIdsPromise = (async () => {
				const res = await fetch(currentPlaylist.split('#')[0] ?? currentPlaylist);
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

		const landSec = Math.max(0, landedMs) / 1000;
		let injected = 0;
		for (const cue of cues) {
			try {
				const start = Math.max(0, cue.startSec - landSec);
				const end = Math.max(start, cue.endSec - landSec);
				const existing = track.cues;
				if (existing) {
					let dup = false;
					for (let i = 0; i < existing.length; i++) {
						const c = existing[i];
						if (
							c &&
							Math.abs(c.startTime - start) < 0.05 &&
							Math.abs(c.endTime - end) < 0.05 &&
							(c as VTTCue).text === cue.text
						) {
							dup = true;
							break;
						}
					}
					if (dup) continue;
				}
				const vtt = new VTTCue(start, end, cue.text);
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
		const titleSec = titleSecondsFromMedia(video.currentTime, landedMs);
		if (reset) {
			clearInjectCues(track);
			nativeLoadedSegs.clear();
			logSubs('native inject reset for scrub', {
				t: video.currentTime,
				titleSec,
				landedMs,
				trackId
			});
		} else {
			pruneInjectCues(track, video.currentTime - 30);
		}

		const seg = segmentIndexAtSeconds(titleSec);
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
			assertInjectModes();
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
			if (landedMs > 0 && wantedSubtitle >= 0) {
				// Full-title subtitle playlist is title-timed; media is window-
				// relative — inject by title index (same as native after scrub).
				hls.subtitleDisplay = false;
				hls.subtitleTrack = -1;
				if (!nativeInjectMode) enterNativeInjectMode();
				else void syncNativeInject(true);
				return;
			}
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

	/** hls.js only: retarget subtitle fragments after in-window scrub. */
	const reassertHlsSubtitleAfterSeek = () => {
		if (destroyed || !hls || wantedSubtitle < 0) return;
		if (landedMs > 0) {
			enterNativeInjectMode();
			return;
		}
		const idx = Math.min(wantedSubtitle, Math.max(0, hls.subtitleTracks.length - 1));
		const t = video.currentTime;
		const seg = Math.floor(t / 2);
		// Echo seeked (land nudge / cook settle) at the same 2s land must not
		// clear cues mid-FRAG — dogfood: reassert @548.03 then @548.13 wiped paint.
		if (seg === lastSubtitleReassertSeg) {
			logSubs('seek reassert skip same-seg', { seg, currentTime: t });
			return;
		}
		lastSubtitleReassertSeg = seg;
		logSubs('seek reassert hls.js', {
			idx,
			currentTime: t,
			subtitleTrack: hls.subtitleTrack,
			trackCount: hls.subtitleTracks.length
		});
		fragProbeUntilMs = Date.now() + 8000;
		clearVideoSubtitleCues(video);
		clearHlsSubtitleBufferedState(hls);
		hls.subtitleDisplay = true;
		hls.subtitleTrack = idx;
		if (!startHlsSubtitleLoad(hls, t)) {
			hls.startLoad(t, true);
		}
		logSubs('seek startLoad subtitles', { currentTime: t, idx });
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
		if (destroyed) return;
		if (seekSuppressActive()) {
			const step = seekSuppressOnSeeked(landRetargetSeconds !== null);
			if (step.applyLand && landRetargetSeconds !== null) {
				const target = landRetargetSeconds;
				landRetargetSeconds = null;
				video.currentTime = target;
				void video.play().catch(() => {});
				if (wantedSubtitle >= 0 && landedMs > 0) {
					enterNativeInjectMode();
					assertInjectModes();
				} else if (hls && wantedSubtitle >= 0) {
					reassertHlsSubtitleAfterSeek();
				}
				return;
			}
			if (step.clearSuppress) clearSeekSuppress();
			if (hls && landedMs === 0) reassertHlsSubtitleAfterSeek();
			else if (wantedSubtitle >= 0 && landedMs > 0) {
				enterNativeInjectMode();
			} else {
				logNativeTracks('seek suppress — attach land', video, { wantedSubtitle });
			}
			return;
		}
		// In-window media scrub (native control or prior currentTime set):
		// refresh cues only — do not POST /seek from element time.
		if (hls) reassertHlsSubtitleAfterSeek();
		else if (wantedSubtitle >= 0 && landedMs > 0) enterNativeInjectMode();
	};

	const onCanPlay = () => {
		if (destroyed) return;
		void video.play().catch(() => {});
		if (landedMs > 0 && wantedSubtitle >= 0) enterNativeInjectMode();
	};

	video.addEventListener('seeked', onSeeked);
	video.addEventListener('canplay', onCanPlay, { once: true });

	if (backend === 'native-hls') {
		logSubs('native-hls attach — filter console for [nj-subs]');
		// Window-relative: session already lands at landedMs; no title #t=.
		video.src = playlistBase;
		video.addEventListener(
			'loadedmetadata',
			() => logNativeTracks('native loadedmetadata', video, { wantedSubtitle }),
			{ once: true }
		);
		// Do not force mode=showing on attach: master DEFAULT=YES already
		// enables the track. Forcing showing on an auto-selected HLS track
		// has been seen to paint cues twice in Safari.
		video.textTracks.addEventListener('addtrack', onAddTrack);
		video.textTracks.addEventListener('change', onTextTrackChange);
	} else {
		hls = new Hls({
			enableWorker: true,
			maxBufferHole: 1.5,
			startPosition: -1
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
		// Product: after hls.js appends subtitle cues, restore title-absolute
		// times from the wire VTT (sticky load-cycle baseline — ADR-0013).
		hls.on(Hls.Events.FRAG_LOADED, (_e, data) => {
			if (destroyed || data.frag?.type !== 'subtitle') return;
			const payload = data.payload;
			if (!payload || (payload as ArrayBuffer).byteLength === 0) return;
			let text: string;
			try {
				text = new TextDecoder('utf-8').decode(payload as ArrayBuffer);
			} catch {
				return;
			}
			const track = [...video.textTracks].find(
				(t) =>
					(t.kind === 'subtitles' || t.kind === 'captions') &&
					t.mode !== 'disabled'
			);
			if (!track) return;
			const fixed = applyAbsoluteCueTimesFromVtt(track, text, landedMs);
			if (subsProbeOn() && Date.now() <= fragProbeUntilMs) {
				const parsed = parseWebVttCues(text);
				const rawFirst = parsed[0]?.startSec;
				const id = parsed[0]?.id;
				const after = id ? track.cues?.getCueById(id) : null;
				logSubs('FRAG cue absolute restore', {
					sn: data.frag.sn,
					fragStart: Number(data.frag.start.toFixed(3)),
					rawFirstStart:
						rawFirst !== undefined ? Number(rawFirst.toFixed(3)) : null,
					trackStartAfter: after ? Number(after.startTime.toFixed(3)) : null,
					fixed,
					currentTime: Number(video.currentTime.toFixed(3))
				});
			}
		});
		// Probe-only listeners: same gate as logSubs (`?njProbe=1`). Product
		// path only needs SUBTITLE_TRACKS_UPDATED → applyWantedSubtitle.
		if (subsProbeOn()) {
			hls.on(Hls.Events.MANIFEST_PARSED, () => {
				if (!hls) return;
				logSubtitleState('manifest parsed', hls);
			});
			hls.on(Hls.Events.SUBTITLE_TRACK_SWITCH, (_e, data) => {
				logSubs('track switch', {
					id: data.id,
					url: data.url ?? null,
					name: data.name ?? null
				});
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
				if (Date.now() > fragProbeUntilMs) return;
				logSubs('FRAG loading', {
					sn: data.frag.sn,
					start: data.frag.start,
					url: data.frag.url ?? null
				});
			});
		}
		hls.on(Hls.Events.SUBTITLE_TRACKS_UPDATED, () => {
			if (!hls) return;
			logSubtitleState('tracks updated', hls);
			applyWantedSubtitle();
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
			clearSeekSuppress();
			landEnsureAbort?.abort();
			landEnsureAbort = null;
			landEnsureSegIdx = null;
			landEnsuredSegIdx = null;
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
		seekToTitleSeconds,
		setSubtitleTrack
	};
}
