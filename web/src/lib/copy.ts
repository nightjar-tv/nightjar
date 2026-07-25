/** User-visible product strings (Phase 1). Keep plain; no marketing voice. */

export const copy = {
	scanInProgress: 'Scanning your library. You can start watching as items appear.',
	emptyLibrary: 'Nothing roosting here yet.',
	emptyLibraryHint: 'Add a media folder and Nightjar will take care of the rest.',
	addFolder: 'Add folder',
	playbackFailed:
		"This file couldn't be played. Nightjar will try transcoding it in a later release. Check the logs for the file details.",
	phase1Hint:
		'Phase 1 plays H.264 + AAC in MP4 only. Look for the green badge. Everything else needs Phase 2 transcoding.'
} as const;
