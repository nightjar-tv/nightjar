/**
 * Shared WebVTT helpers for HLS subtitle segments (ADR-0013).
 *
 * Wire shape is the same for both attach backends: title-absolute cues in
 * `subs/{trackId}/segNNN.vtt`, stable cue id = start ms.
 *
 * Native (iOS/iPadOS): WebKit does not reliably reload EXT-X-MEDIA TextTrack
 * cues after a seek. After the first scrub we fetch those slices and inject
 * onto a client TextTrack.
 *
 * hls.js (desktop Safari / Chrome / …): do not trust parsed cue times after
 * a mid-title load. hls.js freezes a load-cycle baseline (≈ first frag
 * start of that cycle) and adds it to every cue; per-frag `−frag.start`
 * only cancels the first frag and piles the rest. After each subtitle
 * FRAG_LOADED, `applyAbsoluteCueTimesFromVtt` overwrites TextTrack times
 * from the fetched VTT by cue id.
 *
 * SEGMENT_MS must match server/crates/transcode/src/hls.rs.
 */

/** Locked subtitle VTT window (ADR-0010 / ADR-0020). Video segments are
 * time-keyed and are not derived from this constant. */
export const SEGMENT_MS = 2000;

export type ParsedVttCue = {
	startSec: number;
	endSec: number;
	text: string;
	/** Stable id from the slice (cue start ms), when present. */
	id?: string;
};

/** Title-absolute subtitle segment index for a playhead. */
export function segmentIndexAtSeconds(seconds: number): number {
	const ms = Math.max(0, Math.floor(seconds * 1000));
	return Math.floor(ms / SEGMENT_MS);
}

/** `segNNN.vtt` — matches `segment_vtt_name` in hls.rs (`:03` minimum width). */
export function segmentVttName(index: number): string {
	const n = Math.max(0, Math.floor(index));
	return `seg${String(n).padStart(3, '0')}.vtt`;
}

/** Session directory containing assets (no trailing slash). Strips per-run master. */
export function sessionBaseFromMaster(playlistBase: string): string {
	const noHash = playlistBase.split('#')[0] ?? playlistBase;
	const noQuery = noHash.split('?')[0] ?? noHash;
	return noQuery
		.replace(/\/runs\/\d+\/master\.m3u8$/i, '')
		.replace(/\/master\.m3u8$/i, '');
}

export function sessionIdFromPlaylist(playlistBase: string): string | null {
	const m = sessionBaseFromMaster(playlistBase).match(/\/sessions\/([^/]+)$/);
	return m?.[1] ?? null;
}

export function subtitleSegmentUrl(
	sessionBase: string,
	trackId: string,
	segmentIndex: number
): string {
	return `${sessionBase}/subs/${trackId}/${segmentVttName(segmentIndex)}`;
}

/**
 * Parse EXT-X-MEDIA SUBTITLES track ids from a master playlist, in declaration
 * order (aligned with Safari's textTracks order for the same master).
 */
export function parseSubtitleTrackIdsFromMaster(masterBody: string): string[] {
	const ids: string[] = [];
	for (const raw of masterBody.split(/\r?\n/)) {
		const line = raw.trim();
		if (!line.startsWith('#EXT-X-MEDIA:') || !line.includes('TYPE=SUBTITLES')) {
			continue;
		}
		const m = /URI="(?:(?:\.\.\/)*|\/api\/v0\/sessions\/[^/]+\/)subs\/([^"/]+)\.m3u8"/.exec(
			line
		);
		if (m?.[1]) ids.push(m[1]);
	}
	return ids;
}

function parseVttTimestamp(ts: string): number | null {
	const cleaned = ts.trim().replace(',', '.');
	const parts = cleaned.split(':');
	let h = 0;
	let m = 0;
	let rest: string;
	if (parts.length === 3) {
		h = Number(parts[0]);
		m = Number(parts[1]);
		rest = parts[2] ?? '';
	} else if (parts.length === 2) {
		m = Number(parts[0]);
		rest = parts[1] ?? '';
	} else {
		return null;
	}
	if (!Number.isFinite(h) || !Number.isFinite(m)) return null;
	const [secStr, fracStr = '0'] = rest.split('.');
	const s = Number(secStr);
	if (!Number.isFinite(s)) return null;
	let digits = fracStr.replace(/\D/g, '');
	while (digits.length < 3) digits += '0';
	digits = digits.slice(0, 3);
	const ms = Number(digits);
	if (!Number.isFinite(ms)) return null;
	return h * 3600 + m * 60 + s + ms / 1000;
}

function parseVttTiming(line: string): { start: number; end: number } | null {
	const trimmed = line.trim();
	const arrow = trimmed.indexOf('-->');
	if (arrow < 0) return null;
	const start = parseVttTimestamp(trimmed.slice(0, arrow));
	const right = trimmed.slice(arrow + 3).trim();
	const endTok = right.split(/\s+/)[0] ?? '';
	const end = parseVttTimestamp(endTok);
	if (start === null || end === null) return null;
	return { start, end };
}

/**
 * Minimal WebVTT cue parser for Nightjar HLS slices (id + timing + text).
 * Skips NOTE/STYLE/region headers. No cue-settings positioning.
 */
export function parseWebVttCues(body: string): ParsedVttCue[] {
	const normalised = body.replace(/\r\n/g, '\n').replace(/\r/g, '\n').trim();
	if (!normalised) return [];
	const cues: ParsedVttCue[] = [];
	for (const block of normalised.split('\n\n')) {
		const trimmed = block.trim();
		if (
			!trimmed ||
			trimmed.startsWith('WEBVTT') ||
			trimmed.startsWith('NOTE') ||
			trimmed.startsWith('STYLE') ||
			trimmed.startsWith('REGION')
		) {
			continue;
		}
		const lines = trimmed.split('\n');
		const timingIdx = lines.findIndex((l) => l.includes('-->'));
		if (timingIdx < 0) continue;
		const timing = parseVttTiming(lines[timingIdx] ?? '');
		if (!timing || timing.end <= timing.start) continue;
		let id: string | undefined;
		if (timingIdx > 0) {
			const maybeId = (lines[0] ?? '').trim();
			if (maybeId && !maybeId.includes('-->')) id = maybeId;
		}
		const text = lines
			.slice(timingIdx + 1)
			.join('\n')
			.trim();
		if (!text) continue;
		cues.push({
			startSec: timing.start,
			endSec: timing.end,
			text,
			id
		});
	}
	return cues;
}

/**
 * Rewrite TextTrack cue times from the wire VTT (stable cue id = start ms).
 *
 * Wire cues are title-absolute. The media element is window-relative after a
 * mid-title land (ADR-0020): pass the **current** `landedMs` so painted times
 * are `title − land`. At land 0 this matches the wire. Also undoes hls.js
 * sticky load-cycle baseline (dogfood: displayed ≈ raw + firstFragStart).
 * Returns how many cues changed.
 */
export function applyAbsoluteCueTimesFromVtt(
	track: TextTrack,
	body: string,
	landedMs = 0
): number {
	const list = track.cues;
	if (!list) return 0;
	const landSec = Math.max(0, landedMs) / 1000;
	let fixed = 0;
	for (const raw of parseWebVttCues(body)) {
		if (!raw.id) continue;
		const cue = list.getCueById(raw.id);
		if (!cue) continue;
		const start = Math.max(0, raw.startSec - landSec);
		const end = Math.max(start, raw.endSec - landSec);
		if (
			Math.abs(cue.startTime - start) < 0.001 &&
			Math.abs(cue.endTime - end) < 0.001
		) {
			continue;
		}
		cue.startTime = start;
		cue.endTime = end;
		fixed += 1;
	}
	return fixed;
}
