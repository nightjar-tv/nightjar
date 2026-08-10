/** User-visible product strings. Keep plain; no marketing voice. */

export const copy = {
	scanInProgress: 'Scanning your library. You can start watching as items appear.',
	emptyLibrary: 'Nothing roosting here yet.',
	emptyLibraryHint: 'Add a media folder and Nightjar will take care of the rest.',
	addFolder: 'Add folder',
	folderUnreachable: (path: string) =>
		`The folder ${path} isn't reachable. Check that the drive is mounted, then rescan.`,
	rescan: 'Rescan',
	play: 'Play',
	/** A session is transient; the surface is not. Resuming is the ordinary
	 *  case after an idle reap, not an error the viewer has to recover from. */
	playResume: (fromSec: number) => `Resume from ${formatClock(fromSec)}`,
	preparingSession: 'Starting playback session…',
	sessionsBusy:
		'All playback sessions are in use. Close another player tab and try again.',
	sessionFailed: "This file couldn't be prepared for playback. Check the logs for the file details.",
	badgeHint:
		'Badges: browser plays directly, remux and transcode start a playback session when you open the item. Text subtitles (SRT and similar) use the Subtitles control. ASS and image (PGS) tracks burn into the video when you select them — that starts a session even on titles that otherwise play in the browser.',
	subtitlesPreparing: 'Subtitles are being prepared.',
	subtitlesFoundNotRendered: 'Subtitle files found but not rendered yet:',
	burnInNeedsSession: 'This subtitle track is burned into the video and needs a playback session.',
	audioTrack: 'Audio track',
	subtitleTrack: 'Subtitles',
	subtitleOff: 'Off',
	switchingAudio: 'Switching audio track…',
	audioSwitchUnsupported:
		'This browser cannot switch audio tracks on a file it plays directly.',
	titleDamagedUsable: (usableSec: number, claimedSec: number) =>
		`This file looks damaged. Playback works through about ${formatClock(usableSec)} of the claimed ${formatClock(claimedSec)}.`,
	/** Walk still running: a count, never a percentage, because the total is
	 * still growing and a percentage built on it would move backwards. */
	scanFound: (found: number) => `Scanning. ${found.toLocaleString()} found.`,
	probing: (done: number, queued: number) =>
		`Probing. ${done.toLocaleString()} done, ${queued.toLocaleString()} waiting.`,
	probingOf: (done: number, total: number) =>
		`Probing ${done.toLocaleString()} of ${total.toLocaleString()}.`,
	probeErrors: (errors: number) => `${errors.toLocaleString()} probe errors.`,
	/** Its own line: metadata finishes on its own schedule, not the probe's. */
	metadataDraining: (pending: number) =>
		`Matching metadata. ${pending.toLocaleString()} left.`,
	episodeCount: (n: number) => (n === 1 ? '1 episode' : `${n.toLocaleString()} episodes`),
	/** Two rips of one film are one unit (ADR-0025 §2), so say how many files. */
	versionCount: (n: number) => `${n.toLocaleString()} versions`,
	/** The folder has no binding, but its episodes name a show (ADR-0039 item 6). */
	identityEntityOnly: 'show known, folder not bound',
	identityUnidentified: 'no match',
	unitsSummary: (units: number, items: number) =>
		`${units.toLocaleString()} units over ${items.toLocaleString()} items.`,
	showsWithoutBinding: (n: number) =>
		`${n.toLocaleString()} shows have metadata but no folder bound to them anywhere.`,
	seriesUnnumbered: 'No canonical episode numbers, so these are listed rather than ordered.',
	movieVersions: 'Files bound to this film. Which one plays is not decided yet.',
	/** The demo client's login, kept alive until Block 3 supplies the real
	 *  one. Every route but four needs a session now (ADR-0034 item 11). */
	checkingSession: 'Checking your session…',
	loginHint: 'Sign in to browse this server.',
	bootstrapHint: 'No account exists yet. The first one you make owns this server.',
	signIn: 'Sign in',
	createOwner: 'Create owner account',
	signOut: 'Sign out',
	/** Accessible name for the title-time scrub control (ADR-0020). */
	scrubPosition: 'Position',
	/** Fullscreen the player container (keeps our scrub bar). */
	playerFullscreen: 'Fullscreen',
	playerExitFullscreen: 'Exit fullscreen'
};

/** Clock for durations / scrub labels (`m:ss` or `h:mm:ss`). */
export function formatClock(totalSec: number): string {
	const s = Math.max(0, Math.floor(totalSec));
	const h = Math.floor(s / 3600);
	const m = Math.floor((s % 3600) / 60);
	const sec = s % 60;
	if (h > 0) {
		return `${h}:${String(m).padStart(2, '0')}:${String(sec).padStart(2, '0')}`;
	}
	return `${m}:${String(sec).padStart(2, '0')}`;
}
