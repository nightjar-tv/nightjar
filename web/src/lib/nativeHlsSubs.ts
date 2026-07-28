/**
 * Safari native-HLS post-seek subtitle helpers.
 *
 * WebKit does not reliably reload EXT-X-MEDIA TextTrack cues after a seek
 * (ADR-0013). After the first user scrub we fetch the same 2s VTT slices the
 * server already serves and inject them onto a client TextTrack.
 *
 * SEGMENT_MS must match server/crates/transcode/src/hls.rs.
 */

/** Locked to the HLS video segment length (ADR-0008 / ADR-0013). */
export const SEGMENT_MS = 2000;

export type ParsedVttCue = {
	startSec: number;
	endSec: number;
	text: string;
	/** Stable id from the slice (cue start ms), when present. */
	id?: string;
};

/** Title-absolute segment index for a playhead (same math as the server). */
export function segmentIndexAtSeconds(seconds: number): number {
	const ms = Math.max(0, Math.floor(seconds * 1000));
	return Math.floor(ms / SEGMENT_MS);
}

/** `segNNN.vtt` — matches `segment_vtt_name` in hls.rs (`:03` minimum width). */
export function segmentVttName(index: number): string {
	const n = Math.max(0, Math.floor(index));
	return `seg${String(n).padStart(3, '0')}.vtt`;
}

/** `segNNN.m4s` — matches `segment_name` in hls.rs. */
function segmentM4sName(index: number): string {
	const n = Math.max(0, Math.floor(index));
	return `seg${String(n).padStart(3, '0')}.m4s`;
}

/** Session directory containing `master.m3u8` (no trailing slash). */
export function sessionBaseFromMaster(playlistBase: string): string {
	const noHash = playlistBase.split('#')[0] ?? playlistBase;
	const noQuery = noHash.split('?')[0] ?? noHash;
	return noQuery.replace(/\/master\.m3u8$/i, '');
}

function videoSegmentUrl(sessionBase: string, segmentIndex: number): string {
	return `${sessionBase}/${segmentM4sName(segmentIndex)}`;
}

/**
 * Same segment URL with a log-only `njFetcher` query. Serving ignores it;
 * Safari native HLS never adds it — dogfood logs can tell probe from WebKit.
 */
function videoSegmentUrlWithFetcher(
	sessionBase: string,
	segmentIndex: number,
	fetcher: 'land-ensure' | 'attach-wait'
): string {
	const base = videoSegmentUrl(sessionBase, segmentIndex);
	const sep = base.includes('?') ? '&' : '?';
	return `${base}${sep}njFetcher=${fetcher}`;
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
		const m = /URI="subs\/([^"/]+)\.m3u8"/.exec(line);
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
