/** User-visible product strings. Keep plain; no marketing voice. */

export const copy = {
	scanInProgress: 'Scanning your library. You can start watching as items appear.',
	emptyLibrary: 'Nothing roosting here yet.',
	emptyLibraryHint: 'Add a media folder and Nightjar will take care of the rest.',
	addFolder: 'Add folder',
	preparingSession: 'Starting playback session…',
	sessionsBusy:
		'All playback sessions are in use. Close another player tab and try again.',
	sessionFailed: "This file couldn't be prepared for playback. Check the logs for the file details.",
	badgeHint:
		'Badges: browser plays directly, remux and transcode start a playback session when you open the item. Text subtitles (SRT and similar) show in the player caption menu; ASS and image subs are not available yet.',
	sessionSubtitlesPreparing:
		'Captions may take a moment on first play while the file is read.',
	subtitlesFoundNotRendered: 'Subtitle files found but not rendered yet:'
} as const;
