import type { components } from './api/schema.d.ts';

export type SubtitleTrack = components['schemas']['SubtitleTrack'];

/** ASS/SSA: house-styled WebVTT by default; burn-in behind Original styling. */
export function isAssCodec(codec: string): boolean {
	const c = codec.toLowerCase();
	return c === 'ass' || c === 'ssa';
}

/** Image tracks (PGS): always burned; no Original styling control. */
export function isImageBurnCodec(codec: string): boolean {
	return codec.toLowerCase() === 'hdmv_pgs_subtitle';
}

export function subtitleFormatLabel(codec: string): string {
	switch (codec.toLowerCase()) {
		case 'subrip':
		case 'srt':
			return 'SRT';
		case 'webvtt':
		case 'vtt':
			return 'VTT';
		case 'mov_text':
			return 'MOV_TEXT';
		case 'ass':
			return 'ASS';
		case 'ssa':
			return 'SSA';
		case 'hdmv_pgs_subtitle':
			return 'PGS';
		default:
			return codec.toUpperCase();
	}
}

export function subtitlePrimaryLabel(track: SubtitleTrack): string {
	const lang = track.language?.trim();
	if (lang) return lang;
	// Keep in sync with copy.subtitleUnknownLanguage.
	return 'Unknown';
}

/**
 * Mono secondary line: format and consequence. Never repeats language.
 * `originalStyling` only affects ASS/SSA rows. Strings match copy.ts.
 */
export function subtitleSecondaryLine(
	track: SubtitleTrack,
	originalStyling: boolean
): string {
	const format = subtitleFormatLabel(track.codec);
	if (isImageBurnCodec(track.codec) || (track.render === 'burnIn' && !isAssCodec(track.codec))) {
		return `${format} · always burned in`;
	}
	if (isAssCodec(track.codec)) {
		return originalStyling
			? `${format} · burned in`
			: `${format} · house style`;
	}
	return `${format} · house style`;
}

/**
 * Original styling defaults off when a soft path exists. Burn-in-only ASS
 * (no soft WebVTT yet) defaults on so the honest path is selected.
 */
export function defaultOriginalStyling(track: SubtitleTrack | null): boolean {
	if (!track || !isAssCodec(track.codec)) return false;
	if (track.render === 'soft' && track.url) return false;
	return track.render === 'burnIn';
}

export function showOriginalStylingControl(track: SubtitleTrack | null): boolean {
	return track != null && isAssCodec(track.codec);
}

/** Soft delivery ready for DirectPlay `<track>` (partial or complete). */
export function isSoftReady(track: SubtitleTrack): boolean {
	return track.render === 'soft' && Boolean(track.url);
}

/**
 * Soft tracks present in the HLS master (ADR-0013): complete + url only.
 * Partial stays on preparing UI; index into hls.js must match this filter.
 */
export function isHlsSelectable(track: SubtitleTrack): boolean {
	return (
		track.render === 'soft' &&
		track.readiness === 'complete' &&
		Boolean(track.url)
	);
}

/**
 * Whether selecting this row (with the given Original styling) needs burn-in.
 */
export function selectionNeedsBurnIn(
	track: SubtitleTrack,
	originalStyling: boolean
): boolean {
	if (isImageBurnCodec(track.codec) || track.render === 'burnIn') {
		if (isAssCodec(track.codec)) return originalStyling;
		return true;
	}
	if (isAssCodec(track.codec) && originalStyling) return true;
	return false;
}
