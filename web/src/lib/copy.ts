/** User-visible product strings. Keep plain; no marketing voice. */

export const copy = {
	scanInProgress: 'Scanning your library. You can start watching as items appear.',
	emptyLibrary: 'Nothing roosting here yet.',
	emptyLibraryHint: 'Add a media folder and Nightjar will take care of the rest.',
	addFolder: 'Add folder',
	folderUnreachable: (path: string) =>
		`The folder ${path} isn't reachable. Check that the drive is mounted, then rescan.`,
	rescan: 'Rescan',
	preparingSession: 'Starting playback session…',
	sessionsBusy:
		'All playback sessions are in use. Close another player tab and try again.',
	sessionFailed: "This file couldn't be prepared for playback. Check the logs for the file details.",
	badgeHint:
		'Badges: browser plays directly, remux and transcode start a playback session when you open the item. Text subtitles (SRT and similar) use the Subtitles control on the item page; ASS and image subs are not available yet.',
	subtitlesPreparing: 'Subtitles are being prepared.',
	subtitlesFoundNotRendered: 'Subtitle files found but not rendered yet:',
	audioTrack: 'Audio track',
	subtitleTrack: 'Subtitles',
	subtitleOff: 'Off',
	switchingAudio: 'Switching audio track…',
	audioSwitchUnsupported:
		'This browser cannot switch audio tracks on a file it plays directly.'
};
