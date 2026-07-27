<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/api/client';
	import { copy } from '$lib/copy';
	import { attachHls, parkForSwitch, type HlsHandle } from '$lib/hlsPlayer';
	import {
		attachModeFromSearch,
		LatencyProbe,
		probeEnabled,
		type AttachMode
	} from '$lib/latencyProbe';
	import type { components } from '$lib/api/schema';

	type MediaItem = components['schemas']['MediaItem'];
	type PlaybackInfo = components['schemas']['PlaybackInfo'];
	type TranscodeSession = components['schemas']['TranscodeSession'];
	type AudioTrack = components['schemas']['AudioTrack'];
	// Not in lib.dom: only Safari exposes the media element track list today.
	type BrowserAudioTracks = { length: number; [index: number]: { enabled: boolean } };

	let item = $state<MediaItem | null>(null);
	let playback = $state<PlaybackInfo | null>(null);
	let error = $state<string | null>(null);
	let playlistUrl = $state<string | null>(null);
	let sessionEncoder = $state<Pick<TranscodeSession, 'videoEncoder' | 'encoderKind'> | null>(
		null
	);
	let preparingSession = $state(false);
	let switchingAudio = $state(false);
	let audioNote = $state<string | null>(null);
	let selectedAudioTrackId = $state<string | null>(null);
	let videoEl = $state<HTMLVideoElement | null>(null);
	// Mutable holder so onMount cleanup / pagehide always DELETE the live
	// session even if the $state read in a stale closure is still null.
	const sessionRef: { id: string | null } = { id: null };
	// Non-reactive so changing it does not re-run the attach effect on its own.
	const resumeRef = { seconds: 0 };
	// Current attach handle; its positionSeconds() is title-absolute where
	// raw currentTime is not after a mid-title switch (see hlsPlayer).
	const playerRef: { handle: HlsHandle | null } = { handle: null };
	// Read by every await loop so an unmount mid-flight stops the loop and
	// reaps whatever it already started.
	const liveRef = { alive: true };

	const itemId = $derived(Number(page.params.id));
	const audioTracks = $derived(playback?.audioTracks ?? []);
	// Investigation: ?njAttach=land|first|two and ?njProbe=1 (see latencyProbe.ts).
	const attachMode = $derived(attachModeFromSearch(page.url.search));
	const probeOn = $derived(probeEnabled(page.url.search));

	function releaseSession() {
		const id = sessionRef.id;
		sessionRef.id = null;
		sessionEncoder = null;
		if (id) void api.deleteTranscodeSession(id);
	}

	function audioTrackLabel(track: AudioTrack): string {
		const name = track.label ?? track.language ?? track.trackId;
		return track.channelLayout ? `${name} · ${track.channelLayout}` : name;
	}

	/** Poll until FFmpeg has written a servable response (playlist or segment). */
	async function waitForReady(url: string): Promise<boolean> {
		for (let i = 0; liveRef.alive && i < 100; i++) {
			const res = await fetch(url);
			if (res.ok) return true;
			// Gone for good (deleted / never created). 503 means still cooking.
			if (res.status === 404) return false;
			await new Promise((r) => setTimeout(r, 200));
		}
		return false;
	}

	function sessionAssetUrl(playlistUrl: string, name: string): string {
		return playlistUrl.replace(/\/master\.m3u8$/, `/${name}`);
	}

	function selectAudio(trackId: string) {
		if (trackId === selectedAudioTrackId) return;
		selectedAudioTrackId = trackId;
		audioNote = null;
		const track = audioTracks.find((t) => t.trackId === trackId);
		// Direct play is free only while the selected track fits the client
		// ceiling. An over-ceiling secondary (e.g. 5.1 commentary on a stereo
		// default) needs a hybrid session so the pan downmix still runs
		// (ADR-0012).
		if (
			playback?.playbackMethod === 'directPlay' &&
			track != null &&
			track.channels <= 2
		) {
			switchDirectPlayAudio(trackId);
		} else {
			void switchSessionAudio(trackId);
		}
	}

	/** Direct play: the container already holds every track, so the switch
	 *  is client-side and free where the browser exposes the list. */
	function switchDirectPlayAudio(trackId: string) {
		const list = (videoEl as (HTMLVideoElement & { audioTracks?: BrowserAudioTracks }) | null)
			?.audioTracks;
		const index = audioTracks.findIndex((t) => t.trackId === trackId);
		if (!list || index < 0) {
			audioNote = copy.audioSwitchUnsupported;
			return;
		}
		for (let i = 0; i < list.length; i++) {
			list[i].enabled = i === index;
		}
	}

	/** Sessions: a fresh session at the current position, then drop the old
	 *  one. Init and prior segments carry the old audio config, so this is
	 *  never a window move inside the seek path (ADR-0012). */
	async function switchSessionAudio(trackId: string) {
		const seconds = playerRef.handle?.positionSeconds() ?? videoEl?.currentTime ?? 0;
		const startMs = Math.max(0, Math.floor(seconds * 1000));
		const previous = sessionRef.id;
		const probe = new LatencyProbe(attachMode, probeOn);
		const unspy = probe.installFetchSpy();
		probe.mark('switch_requested', `startMs=${startMs} mode=${attachMode}`);
		switchingAudio = true;
		// Park immediately so the old track cannot keep playing past the
		// switch point and then jump backwards when the new session lands.
		parkForSwitch(videoEl, playerRef.handle);
		playerRef.handle = null;
		playlistUrl = null;
		try {
			const started = await api.startTranscodeSession(itemId, startMs, trackId);
			probe.mark('session_post_ok', started.sessionId);
			// The old session is no longer honest UI once the player is parked.
			// Reap it now so a hardware encoder slot is not held while the new
			// session cooks.
			if (previous) void api.deleteTranscodeSession(previous);
			probe.mark('old_session_deleted', previous ?? 'none');
			probe.mark('wait_begin', attachMode);
			const landIdx = Math.floor(startMs / 2000);
			const windowIdx = Math.max(0, landIdx - 8);
			const ready = await waitForAttachReady(
				started.playlistUrl,
				windowIdx,
				landIdx,
				attachMode,
				probe
			);
			// Never adopt a session the page no longer owns; leaving it for
			// the idle reaper burns a cap slot for a minute.
			if (!ready || !liveRef.alive) {
				void api.deleteTranscodeSession(started.sessionId);
				if (liveRef.alive) error = copy.sessionFailed;
				return;
			}
			sessionRef.id = started.sessionId;
			sessionEncoder = started;
			resumeRef.seconds = startMs / 1000;
			probe.mark('attach', started.playlistUrl);
			if (videoEl) probe.wireVideo(videoEl);
			playlistUrl = started.playlistUrl;
			if (probeOn) {
				// Defer summary until after first frame likely lands.
				setTimeout(() => {
					console.info('[nj-probe-summary]', JSON.stringify(probe.summary()));
				}, 8000);
			}
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			unspy();
			switchingAudio = false;
		}
	}

	/** Investigation attach gate: land (shipped), first window seg, or two segs. */
	async function waitForAttachReady(
		playlist: string,
		windowIdx: number,
		landIdx: number,
		mode: AttachMode,
		probe: LatencyProbe
	): Promise<boolean> {
		const masterOk = await waitForReady(playlist);
		if (masterOk) probe.mark('master_ready');
		if (!masterOk) return false;
		if (mode === 'land') {
			const landName = `seg${String(landIdx).padStart(3, '0')}.m4s`;
			const ok = await waitForReady(sessionAssetUrl(playlist, landName));
			if (ok) probe.mark('land_seg_ready', landName);
			return ok;
		}
		const firstName = `seg${String(windowIdx).padStart(3, '0')}.m4s`;
		const firstOk = await waitForReady(sessionAssetUrl(playlist, firstName));
		if (firstOk) probe.mark('first_seg_ready', firstName);
		if (!firstOk) return false;
		if (mode === 'first') return true;
		const secondName = `seg${String(windowIdx + 1).padStart(3, '0')}.m4s`;
		const secondOk = await waitForReady(sessionAssetUrl(playlist, secondName));
		if (secondOk) probe.mark('second_seg_ready', secondName);
		return secondOk;
	}

	const playable = $derived(
		playback != null &&
			(playback.playbackMethod === 'directPlay' ||
				playlistUrl != null ||
				switchingAudio)
	);

	// Discovered but not served (ASS/SSA sidecars): say so instead of hiding.
	const unrenderedSubtitles = $derived(
		(playback?.subtitleTracks ?? [])
			.filter((t) => !t.url)
			.map((t) => `${t.language ?? t.trackId} (${t.codec})`)
			.join(', ')
	);

	onMount(() => {
		liveRef.alive = true;

		const onPageHide = () => releaseSession();
		// pagehide DELETE (keepalive): refresh and close must stop the
		// session. keepalive reduces orphans; the idle reaper is still the
		// Gate 2 backstop when the request never leaves the browser.
		window.addEventListener('pagehide', onPageHide);

		(async () => {
			item = await api.getItem(itemId);
			playback = await api.getPlaybackInfo(itemId);
			selectedAudioTrackId =
				playback.audioTracks?.find((t) => t.default)?.trackId ?? null;

			// Remux and transcode both play through a session (ADR-0011).
			if (playback.playbackMethod !== 'directPlay') {
				preparingSession = true;
				let started: TranscodeSession | null = null;
				for (let attempt = 0; liveRef.alive && attempt < 5; attempt++) {
					try {
						started = await api.startTranscodeSession(itemId);
						sessionRef.id = started.sessionId;
						sessionEncoder = started;
						break;
					} catch (e) {
						const msg = e instanceof Error ? e.message : String(e);
						if (msg.includes('retry shortly') || msg.includes('in use')) {
							await new Promise((r) => setTimeout(r, 1000));
							continue;
						}
						throw e;
					}
				}
				if (!started) {
					preparingSession = false;
					error = copy.sessionsBusy;
					return;
				}
				// Wait until init is ready so the VOD playlist is servable.
				if (await waitForReady(started.playlistUrl)) {
					playlistUrl = started.playlistUrl;
					preparingSession = false;
					return;
				}
				preparingSession = false;
				error = copy.sessionFailed;
			}
		})().catch((e: Error) => {
			preparingSession = false;
			error = e.message;
		});

		return () => {
			liveRef.alive = false;
			window.removeEventListener('pagehide', onPageHide);
			releaseSession();
		};
	});

	$effect(() => {
		const video = videoEl;
		const url = playlistUrl;
		if (!video || !url) {
			return;
		}
		const handle = attachHls(video, url, resumeRef.seconds);
		playerRef.handle = handle;
		return () => {
			playerRef.handle = null;
			handle.destroy();
		};
	});
</script>

<svelte:head>
	<title>{item?.title ?? 'item'} · nightjar</title>
</svelte:head>

<main>
	<p class="crumb">
		<a href="/">nightjar</a>
		{#if item}
			/ <a href="/libraries/{item.libraryId}">library</a> /
		{/if}
		item
	</p>

	{#if error}
		<p class="error" role="alert">{error}</p>
	{/if}

	{#if item && playback}
		<header>
			<h1>{item.title}</h1>
			<p class="meta">
				{item.kind}
				{#if item.year}· {item.year}{/if}
				{#if playback.videoCodec}· {playback.videoCodec}{/if}
				{#if playback.audioCodec}· {playback.audioCodec}{/if}
				{#if sessionEncoder?.encoderKind === 'copy'}
					· stream copy
				{:else if sessionEncoder}
					· transcoding · {sessionEncoder.videoEncoder} ({sessionEncoder.encoderKind})
				{/if}
			</p>
			<p class="reason">{playback.reason}</p>
		</header>

		{#if playable && playlistUrl}
			<!-- svelte-ignore a11y_media_has_caption -->
			<video bind:this={videoEl} controls playsinline></video>
			{#if playback.subtitleStatus === 'pending'}
				<p class="preparing">{copy.subtitlesPreparing}</p>
			{/if}
			{#if unrenderedSubtitles}
				<p class="preparing">{copy.subtitlesFoundNotRendered} {unrenderedSubtitles}</p>
			{/if}
		{:else if playable && playback.streamUrl}
			{#if playback.subtitleStatus === 'pending'}
				<p class="preparing">{copy.subtitlesPreparing}</p>
			{/if}
			{#if (playback.subtitleTracks?.length ?? 0) > 0}
				<!-- svelte-ignore a11y_media_has_caption (language subtitles are not captions) -->
				<video
					bind:this={videoEl}
					controls
					playsinline
					src={playback.streamUrl}
					crossorigin="anonymous"
				>
					{#each playback.subtitleTracks ?? [] as track, i (track.trackId)}
						{#if track.url}
							<track
								kind="subtitles"
								src={track.url}
								srclang={track.language ?? 'und'}
								label={track.label
									?? track.language
									?? `Subtitles ${track.trackId}`}
								default={i === 0}
							/>
						{/if}
					{/each}
					Your browser cannot play this file directly.
				</video>
			{:else}
				<!-- svelte-ignore a11y_media_has_caption -->
				<video bind:this={videoEl} controls playsinline src={playback.streamUrl}>
					Your browser cannot play this file directly.
				</video>
			{/if}
			{#if unrenderedSubtitles}
				<p class="preparing">{copy.subtitlesFoundNotRendered} {unrenderedSubtitles}</p>
			{/if}
		{:else if switchingAudio}
			<p class="preparing" role="status">{copy.switchingAudio}</p>
		{:else if preparingSession}
			<p class="preparing" role="status">{copy.preparingSession}</p>
		{:else if playback.playbackMethod !== 'directPlay'}
			<p class="error">{copy.sessionFailed}</p>
		{/if}

		{#if playable && audioTracks.length > 1}
			<fieldset class="tracks">
				<legend>{copy.audioTrack}</legend>
				{#each audioTracks as track (track.trackId)}
					<label>
						<input
							type="radio"
							name="audio-track"
							value={track.trackId}
							checked={track.trackId === selectedAudioTrackId}
							disabled={switchingAudio}
							onchange={() => selectAudio(track.trackId)}
						/>
						{audioTrackLabel(track)}
					</label>
				{/each}
			</fieldset>
			{#if audioNote}
				<p class="preparing" role="status">{audioNote}</p>
			{/if}
		{/if}
	{/if}
</main>

<style>
	main {
		max-width: 56rem;
		margin: 0 auto;
		padding: 2rem 1.25rem 4rem;
	}
	.crumb {
		color: var(--moth-dim);
		margin: 0 0 1.5rem;
	}
	.crumb a {
		color: var(--moth-dim);
	}
	h1 {
		font-family: 'Bricolage Grotesque', system-ui, sans-serif;
		font-size: 2rem;
		font-weight: 700;
		margin: 0;
	}
	.meta,
	.reason {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	video {
		display: block;
		width: 100%;
		margin-top: 1.5rem;
		background: #000;
		border-radius: 8px;
	}
	.error {
		color: var(--dusk);
	}
	.preparing {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	.tracks {
		display: flex;
		flex-wrap: wrap;
		gap: 0.25rem 1rem;
		align-items: center;
		margin-top: 1rem;
		padding: 0.75rem 1rem;
		border: 1px solid var(--moth-dim);
		border-radius: 8px;
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
	}
	.tracks legend {
		color: var(--moth-dim);
		padding: 0 0.35rem;
	}
	.tracks label {
		display: flex;
		align-items: center;
		gap: 0.4rem;
	}
	.tracks input:focus-visible {
		outline: 2px solid currentColor;
		outline-offset: 2px;
	}
</style>
