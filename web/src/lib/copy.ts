/** User-visible product strings. Keep plain; no marketing voice. */

export const copy = {
	scanInProgress: 'Scanning your library. You can start watching as items appear.',
	emptyLibrary: 'Nothing roosting here yet.',
	emptyLibraryHint: 'Add a media folder and Nightjar will take care of the rest.',
	addFolder: 'Add folder',
	preparingPlayback: 'Preparing playback. Large files can take a few minutes.',
	preparingTranscode: 'Starting transcode session…',
	remuxFailed: "This file couldn't be prepared for playback. Check the logs for the file details.",
	transcodeFailed: "This file couldn't be transcoded. Check the logs for the file details.",
	badgeHint:
		'Badges: browser plays directly, remux is prepared on first play, transcode starts when you open the item. Embedded subtitles are not shown yet for remuxed files.',
	remuxSubtitleNote: 'Subtitles embedded in this file are not shown yet. Subtitle support is next.'
} as const;
