/** User-visible product strings. Keep plain; no marketing voice. */

export const copy = {
	scanInProgress: 'Scanning your library. You can start watching as items appear.',
	emptyLibrary: 'Nothing roosting here yet.',
	emptyLibraryHint: 'Add a media folder and Nightjar will take care of the rest.',
	addFolder: 'Add folder',
	preparingPlayback: 'Preparing playback. Large files can take a few minutes.',
	preparingTranscode: 'Starting transcode session…',
	sessionsBusy:
		'All transcode sessions are in use. Close another player tab and try again.',
	remuxFailed: "This file couldn't be prepared for playback. Check the logs for the file details.",
	transcodeFailed: "This file couldn't be transcoded. Check the logs for the file details.",
	badgeHint:
		'Badges: browser plays directly, remux is prepared on first play, transcode starts when you open the item. Text subtitles (SRT and similar) show as captions on direct play and remux; ASS and image subs are not available yet.',
	remuxSubtitleNote:
		'Styled or image subtitles in this file are not shown yet. Text tracks (SRT and similar) appear in the player caption menu when present.'
} as const;
