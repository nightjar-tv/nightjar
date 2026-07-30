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
		'Badges: browser plays directly, remux and transcode start a playback session when you open the item. Text subtitles (SRT and similar) use the Subtitles control. ASS and image (PGS) tracks burn into the video when you select them — that starts a session even on titles that otherwise play in the browser.',
	subtitlesPreparing: 'Preparing subtitles',
	subtitlesFoundNotRendered: 'Subtitle files found but not rendered yet:',
	burnInNeedsSession: 'This subtitle track is burned into the video and needs a playback session.',
	audioTrack: 'Audio track',
	subtitleTrack: 'Subtitles',
	subtitleOff: 'Off',
	subtitleUnknownLanguage: 'Unknown',
	subtitleForced: 'Forced',
	subtitleSdh: 'SDH',
	subtitleOriginalStyling: 'Original styling',
	subtitleSecondaryHouse: (format: string) => `${format} · house style`,
	subtitleSecondaryBurned: (format: string) => `${format} · burned in`,
	subtitleSecondaryAlwaysBurned: (format: string) => `${format} · always burned in`,
	subtitleWaitEstimate: (low: number, high: number) =>
		`About ${low}–${high} minutes`,
	subtitleWait: 'Wait',
	subtitleStartWithout: 'Start without captions',
	subtitleCaptionsFromAttach: 'Captions start from where burn-in attaches.',
	switchingAudio: 'Switching audio track…',
	switchingSubtitles: 'Switching subtitles…',
	audioSwitchUnsupported:
		'This browser cannot switch audio tracks on a file it plays directly.'
};
