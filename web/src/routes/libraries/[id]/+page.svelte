<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/api/client';
	import { copy } from '$lib/copy';
	import type { components } from '$lib/api/schema';

	type Library = components['schemas']['Library'];
	type MediaItem = components['schemas']['MediaItem'];
	type ScanJob = components['schemas']['ScanJob'];

	let library = $state<Library | null>(null);
	let items = $state<MediaItem[]>([]);
	let error = $state<string | null>(null);
	let scanning = $state(false);
	let scanJob = $state<ScanJob | null>(null);

	const libraryId = $derived(Number(page.params.id));

	async function load() {
		library = await api.getLibrary(libraryId);
		const res = await api.listItems(libraryId);
		items = res.items;
	}

	onMount(() => {
		load().catch((e: Error) => {
			error = e.message;
		});
	});

	async function scan() {
		scanning = true;
		error = null;
		try {
			const accepted = await api.scanLibrary(libraryId);
			let job = await api.getScanJob(accepted.jobId);
			scanJob = job;
			while (job.state === 'queued' || job.state === 'indexing' || job.state === 'probing') {
				await load();
				await new Promise((r) => setTimeout(r, 400));
				job = await api.getScanJob(accepted.jobId);
				scanJob = job;
			}
			if (job.state === 'failed') {
				error = job.error ?? 'scan failed';
			}
			await load();
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
		} finally {
			scanning = false;
		}
	}

	function meta(item: MediaItem): string {
		const bits: string[] = [item.kind];
		if (item.year) bits.push(String(item.year));
		if (item.season != null && item.episode != null) {
			bits.push(
				`S${String(item.season).padStart(2, '0')}E${String(item.episode).padStart(2, '0')}`
			);
		}
		if (item.probeStatus === 'indexed') bits.push('indexing');
		if (item.container) bits.push(item.container);
		if (item.videoCodec) bits.push(item.videoCodec);
		if (item.audioCodec) bits.push(item.audioCodec);
		if (item.width && item.height) bits.push(`${item.width}x${item.height}`);
		if (item.scanError) bits.push('probe error');
		return bits.join(' · ');
	}

	function scanLine(job: ScanJob): string {
		const parts = [
			`+${job.added}`,
			`~${job.updated}`,
			`−${job.removed}`,
			`=${job.unchanged}`,
			`probed ${job.probed}`,
			`!${job.errors}`
		];
		if (job.indexDurationMs != null) parts.push(`index ${job.indexDurationMs}ms`);
		if (job.probeDurationMs != null) parts.push(`probe ${job.probeDurationMs}ms`);
		parts.push(job.state);
		return parts.join(' · ');
	}
</script>

<svelte:head>
	<title>{library?.name ?? 'library'} · nightjar</title>
</svelte:head>

<main>
	<p class="crumb"><a href="/">nightjar</a> / library</p>

	{#if error}
		<p class="error" role="alert">{error}</p>
	{/if}

	{#if library}
		<header>
			<h1>{library.name}</h1>
			<p class="path">{library.path}</p>
			<button type="button" onclick={scan} disabled={scanning}>
				{scanning ? 'Scanning…' : 'Scan library'}
			</button>
			{#if scanning}
				<p class="scan">{copy.scanInProgress}</p>
			{/if}
			{#if scanJob}
				<p class="scan">{scanLine(scanJob)}</p>
			{/if}
		</header>

		{#if items.length === 0}
			<p class="empty">No items yet. Run a scan.</p>
		{:else}
			<p class="hint">{copy.phase1Hint}</p>
			<ul class="grid">
				{#each items as item (item.id)}
					<li>
						<a href="/items/{item.id}">
							<span class="row">
								<span class="title">{item.title}</span>
								{#if item.directPlay}
									<span class="badge ok">browser</span>
								{:else if item.probeStatus === 'indexed'}
									<span class="badge">probing…</span>
								{:else if item.scanError || item.probeStatus === 'error'}
									<span class="badge bad">probe error</span>
								{:else}
									<span class="badge">needs transcode</span>
								{/if}
							</span>
							<span class="meta">{meta(item)}</span>
						</a>
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</main>

<style>
	main {
		max-width: 48rem;
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
	.path,
	.scan,
	.meta,
	.empty,
	.hint {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	.hint {
		margin: 1.5rem 0 0;
	}
	.row {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 0.75rem;
	}
	.badge {
		flex-shrink: 0;
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.75rem;
		padding: 0.15rem 0.4rem;
		border-radius: 4px;
		border: 1px solid var(--night-line);
		color: var(--moth-dim);
	}
	.badge.ok {
		border-color: var(--fern);
		color: var(--fern);
	}
	.badge.bad {
		border-color: var(--dusk);
		color: var(--dusk);
	}
	button {
		margin-top: 1rem;
		font: inherit;
		font-weight: 600;
		padding: 0.6rem 0.9rem;
		border: none;
		border-radius: 8px;
		background: var(--dusk);
		color: var(--night);
		cursor: pointer;
	}
	button:disabled {
		opacity: 0.6;
	}
	button:focus-visible {
		outline: 2px solid var(--dusk);
		outline-offset: 2px;
	}
	.grid {
		list-style: none;
		margin: 2rem 0 0;
		padding: 0;
		display: grid;
		gap: 0.5rem;
	}
	.grid a {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		padding: 0.85rem 1rem;
		border-radius: 8px;
		color: inherit;
		text-decoration: none;
	}
	.grid a:hover,
	.grid a:focus-visible {
		background: var(--night-raised);
	}
	.grid a:focus-visible {
		outline: 2px solid var(--dusk);
	}
	.title {
		font-size: 1.125rem;
	}
	.error {
		color: var(--dusk);
	}
</style>
