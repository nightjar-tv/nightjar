<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/api/client';
	import { copy } from '$lib/copy';
	import { attachHls } from '$lib/hlsPlayer';
	import type { components } from '$lib/api/schema';

	type MediaItem = components['schemas']['MediaItem'];
	type PlaybackInfo = components['schemas']['PlaybackInfo'];
	type TranscodeSession = components['schemas']['TranscodeSession'];

	let item = $state<MediaItem | null>(null);
	let playback = $state<PlaybackInfo | null>(null);
	let error = $state<string | null>(null);
	let playlistUrl = $state<string | null>(null);
	let sessionEncoder = $state<Pick<TranscodeSession, 'videoEncoder' | 'encoderKind'> | null>(
		null
	);
	let preparingSession = $state(false);
	let videoEl = $state<HTMLVideoElement | null>(null);
	// Mutable holder so onMount cleanup / pagehide always DELETE the live
	// session even if the $state read in a stale closure is still null.
	const sessionRef: { id: string | null } = { id: null };

	const itemId = $derived(Number(page.params.id));

	function releaseSession() {
		const id = sessionRef.id;
		sessionRef.id = null;
		sessionEncoder = null;
		if (id) void api.deleteTranscodeSession(id);
	}

	const playable = $derived(
		playback != null && (playback.playbackMethod === 'directPlay' || playlistUrl != null)
	);

	// Discovered but not served (ASS/SSA sidecars): say so instead of hiding.
	const unrenderedSubtitles = $derived(
		(playback?.subtitleTracks ?? [])
			.filter((t) => !t.url)
			.map((t) => `${t.language ?? t.trackId} (${t.codec})`)
			.join(', ')
	);

	onMount(() => {
		let alive = true;

		const onPageHide = () => releaseSession();
		// pagehide DELETE: refresh and close must stop the session. Remounts
		// without teardown used to leave FFmpeg running until the idle reaper.
		window.addEventListener('pagehide', onPageHide);

		(async () => {
			item = await api.getItem(itemId);
			playback = await api.getPlaybackInfo(itemId);

			// Remux and transcode both play through a session (ADR-0011).
			if (playback.playbackMethod !== 'directPlay') {
				preparingSession = true;
				let started: TranscodeSession | null = null;
				for (let attempt = 0; alive && attempt < 5; attempt++) {
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
				for (let i = 0; alive && i < 100; i++) {
					const res = await fetch(started.playlistUrl);
					if (res.ok) {
						playlistUrl = started.playlistUrl;
						preparingSession = false;
						return;
					}
					await new Promise((r) => setTimeout(r, 200));
				}
				preparingSession = false;
				error = copy.sessionFailed;
			}
		})().catch((e: Error) => {
			preparingSession = false;
			error = e.message;
		});

		return () => {
			alive = false;
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
		const handle = attachHls(video, url);
		return () => handle.destroy();
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
			{#if (playback.subtitleTracks ?? []).some((t) => t.url)}
				<p class="preparing">{copy.sessionSubtitlesPreparing}</p>
			{/if}
			{#if unrenderedSubtitles}
				<p class="preparing">{copy.subtitlesFoundNotRendered} {unrenderedSubtitles}</p>
			{/if}
		{:else if playable && playback.streamUrl}
			{#if (playback.subtitleTracks?.length ?? 0) > 0}
				<!-- svelte-ignore a11y_media_has_caption (language subtitles are not captions) -->
				<video controls playsinline src={playback.streamUrl} crossorigin="anonymous">
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
				<video controls playsinline src={playback.streamUrl}>
					Your browser cannot play this file directly.
				</video>
			{/if}
			{#if unrenderedSubtitles}
				<p class="preparing">{copy.subtitlesFoundNotRendered} {unrenderedSubtitles}</p>
			{/if}
		{:else if preparingSession}
			<p class="preparing" role="status">{copy.preparingSession}</p>
		{:else if playback.playbackMethod !== 'directPlay'}
			<p class="error">{copy.sessionFailed}</p>
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
</style>
