/** User-visible product strings. Keep plain; no marketing voice. */

export const copy = {
	scanInProgress: 'Scanning your library. You can start watching as items appear.',
	emptyLibrary: 'Nothing roosting here yet.',
	emptyLibraryHint: 'Add a media folder and Nightjar will take care of the rest.',
	addFolder: 'Add folder',
	preparingPlayback: 'Preparing playback. Large files can take a few minutes.',
	remuxFailed: "This file couldn't be prepared for playback. Check the logs for the file details.",
	needsTranscode:
		"This file needs transcoding, which isn't built yet. It will play in a later release.",
	badgeHint:
		'Badges: browser plays directly, remux is prepared on first play, needs transcode waits for a later release.'
} as const;
