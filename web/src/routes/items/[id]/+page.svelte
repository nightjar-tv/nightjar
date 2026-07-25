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
	let sessionId = $state<string | null>(null);
	let sessionEncoder = $state<Pick<TranscodeSession, 'videoEncoder' | 'encoderKind'> | null>(
		null
	);
	let preparingTranscode = $state(false);
	let videoEl = $state<HTMLVideoElement | null>(null);
	// Mutable holder so onMount cleanup / pagehide always DELETE the live
	// session even if the $state read in a stale closure is still null.
	const sessionRef: { id: string | null } = { id: null };

	const itemId = $derived(Number(page.params.id));

	function holdSession(id: string | null) {
		sessionRef.id = id;
		sessionId = id;
	}

	function releaseSession() {
		const id = sessionRef.id;
		sessionRef.id = null;
		sessionId = null;
		sessionEncoder = null;
		if (id) void api.deleteTranscodeSession(id);
	}

	const playable = $derived(
		playback != null &&
			(playback.playbackMethod === 'directPlay' ||
				(playback.playbackMethod === 'remux' && playback.remuxState === 'ready') ||
				(playback.playbackMethod === 'transcode' && playlistUrl != null))
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
		// pagehide DELETE: refresh/close must drop the ref (the refs=4 leak
		// was remounts without teardown). Cannot unit-test in node; sequence
		// is holdSession → pagehide/releaseSession → DELETE → refs--.
		window.addEventListener('pagehide', onPageHide);

		(async () => {
			item = await api.getItem(itemId);
			playback = await api.getPlaybackInfo(itemId);

			if (playback.playbackMethod === 'remux') {
				while (
					alive &&
					playback.remuxState !== 'ready' &&
					playback.remuxState !== 'failed'
				) {
					if (playback.remuxState === 'notStarted') {
						await api.startRemux(itemId);
					}
					await new Promise((r) => setTimeout(r, 400));
					if (!alive) return;
					playback = await api.getPlaybackInfo(itemId);
				}
				return;
			}

			if (playback.playbackMethod === 'transcode') {
				preparingTranscode = true;
				let started: TranscodeSession | null = null;
				for (let attempt = 0; alive && attempt < 5; attempt++) {
					try {
						started = await api.startTranscodeSession(itemId);
						holdSession(started.sessionId);
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
					preparingTranscode = false;
					error = copy.sessionsBusy;
					return;
				}
				// Wait until init is ready so the VOD playlist is servable.
				for (let i = 0; alive && i < 100; i++) {
					const res = await fetch(started.playlistUrl);
					if (res.ok) {
						playlistUrl = started.playlistUrl;
						preparingTranscode = false;
						return;
					}
					await new Promise((r) => setTimeout(r, 200));
				}
				preparingTranscode = false;
				error = copy.transcodeFailed;
			}
		})().catch((e: Error) => {
			preparingTranscode = false;
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
		const id = itemId;
		if (!video || !url || playback?.playbackMethod !== 'transcode') {
			return;
		}
		// Expected scrub sequence with a second browser on the same title:
		// reuse (refs++) on load → seeked → playlist 409 → forkAt POSTs new
		// startMs + DELETE prior id (refs-- on shared) → one new session.
		const handle = attachHls(video, url, {
			forkAt: async (absoluteStartMs) => {
				const prev = sessionRef.id;
				try {
					const forked = await api.startTranscodeSession(id, absoluteStartMs);
					holdSession(forked.sessionId);
					sessionEncoder = forked;
					if (prev && prev !== forked.sessionId) {
						void api.deleteTranscodeSession(prev);
					}
					for (let i = 0; i < 50; i++) {
						const res = await fetch(forked.playlistUrl);
						if (res.ok) break;
						await new Promise((r) => setTimeout(r, 100));
					}
					return forked;
				} catch (e) {
					const msg = e instanceof Error ? e.message : String(e);
					if (msg.includes('retry shortly') || msg.includes('in use')) {
						error = copy.sessionsBusy;
						return null;
					}
					throw e;
				}
			},
			onSession: (sid) => holdSession(sid)
		});
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
				{#if playback.playbackMethod === 'transcode' && sessionEncoder}
					· transcoding · {sessionEncoder.videoEncoder} ({sessionEncoder.encoderKind})
				{/if}
			</p>
			<p class="reason">{playback.reason}</p>
		</header>

		{#if playable && playback.playbackMethod === 'transcode' && playlistUrl}
			<!-- svelte-ignore a11y_media_has_caption -->
			<video bind:this={videoEl} controls playsinline></video>
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
			{#if playback.playbackMethod === 'remux' && (playback.subtitleTracks?.length ?? 0) === 0}
				<p class="preparing">{copy.remuxSubtitleNote}</p>
			{/if}
			{#if unrenderedSubtitles}
				<p class="preparing">{copy.subtitlesFoundNotRendered} {unrenderedSubtitles}</p>
			{/if}
		{:else if playback.playbackMethod === 'remux' && playback.remuxState === 'failed'}
			<p class="error" role="alert">
				{copy.remuxFailed}
				{#if playback.remuxError}({playback.remuxError}){/if}
			</p>
		{:else if playback.playbackMethod === 'remux'}
			<p class="preparing" role="status">{copy.preparingPlayback}</p>
		{:else if preparingTranscode}
			<p class="preparing" role="status">{copy.preparingTranscode}</p>
		{:else if playback.playbackMethod === 'transcode'}
			<p class="error">{copy.transcodeFailed}</p>
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
