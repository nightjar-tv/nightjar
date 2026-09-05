<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/api/client';
	import { copy } from '$lib/copy';
	import type { components } from '$lib/api/schema';

	type Library = components['schemas']['Library'];
	type LibraryUnits = components['schemas']['LibraryUnits'];
	type LibraryUnit = components['schemas']['LibraryUnit'];
	type ScanProgress = components['schemas']['ScanProgress'];

	/// The counts behind this move in bursts, not smoothly: the index pass
	/// commits 200 rows at a time, so nothing changes between most requests at
	/// the ~2/second cadence `/sessions` uses. Five seconds is an order of
	/// magnitude under the longest observed gap between commits, so no burst
	/// waits more than one interval, and over a three-hour scan it is ~2,200
	/// requests rather than ~21,600. Polling harder does not make the server
	/// commit sooner.
	const PROGRESS_POLL_MS = 5000;
	/// One failed progress read is a blip worth retrying; a solid run of
	/// failures is an outage worth saying so about, not something to poll
	/// forever in silence.
	const MAX_CONSECUTIVE_POLL_FAILURES = 3;

	let library = $state<Library | null>(null);
	let listed = $state<LibraryUnits | null>(null);
	let progress = $state<ScanProgress | null>(null);
	let error = $state<string | null>(null);
	let scanning = $state(false);

	/// Poll lifecycle, kept outside reactive state because nothing renders
	/// from it. The onMount cleanup sets `disposed`, and every await in the
	/// poll loop re-checks it before touching the network or the DOM, so
	/// navigating away mid-scan cannot leave a poller hitting
	/// `/scan-progress` every five seconds behind, and each revisit cannot
	/// stack another one.
	let disposed = false;
	let pollTimer: ReturnType<typeof setTimeout> | undefined;
	let pollActive = false;

	const libraryId = $derived(Number(page.params.id));

	async function load() {
		library = await api.getLibrary(libraryId);
		listed = await api.listUnits(libraryId);
	}

	function scanRunning(p: ScanProgress | null): boolean {
		return (
			p?.state === 'queued' || p?.state === 'indexing' || p?.state === 'probing'
		);
	}

	function pause(ms: number): Promise<void> {
		return new Promise((resolve) => {
			pollTimer = setTimeout(resolve, ms);
		});
	}

	/// Poll until the server reports no running scan, or the page goes away.
	/// One transient failure is retried, not fatal: a scan's progress display
	/// must not die on the first network blip.
	async function pollProgress() {
		if (pollActive || disposed) return;
		pollActive = true;
		let failures = 0;
		try {
			while (!disposed) {
				let p: ScanProgress;
				try {
					p = await api.getScanProgress(libraryId);
				} catch (err) {
					if (disposed) return;
					failures += 1;
					if (failures >= MAX_CONSECUTIVE_POLL_FAILURES) throw err;
					await pause(PROGRESS_POLL_MS);
					continue;
				}
				if (disposed) return;
				failures = 0;
				progress = p;
				// The button mirrors the server's actual scan state, not just
				// a scan this page started: arriving at a library with a scan
				// already running must not offer an enabled second one.
				scanning = scanRunning(p);
				if (!scanning) break;
				await pause(PROGRESS_POLL_MS);
			}
			if (!disposed) await load();
		} finally {
			pollActive = false;
		}
	}

	onMount(() => {
		load()
			.then(pollProgress)
			.catch((e: Error) => {
				if (!disposed) error = e.message;
			});
		return () => {
			disposed = true;
			if (pollTimer) clearTimeout(pollTimer);
		};
	});

	async function scan() {
		scanning = true;
		error = null;
		try {
			await api.scanLibrary(libraryId);
			await pollProgress();
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
		} finally {
			scanning = false;
		}
	}

	/// A unit with exactly one file opens that file. A film with two versions
	/// has no single row to open, so it goes to the detail route like a show.
	function unitHref(unit: LibraryUnit): string {
		if (unit.itemId != null) return `/items/${unit.itemId}`;
		const params = new URLSearchParams({
			seriesKey: unit.seriesKey,
			from: String(libraryId)
		});
		return `/series?${params}`;
	}

	function unitMeta(unit: LibraryUnit): string {
		const bits: string[] = [];
		if (unit.year) bits.push(String(unit.year));
		if (unit.kind === 'series') bits.push(copy.episodeCount(unit.itemCount));
		else if (unit.itemCount > 1) bits.push(copy.versionCount(unit.itemCount));
		return bits.join(' · ');
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
			{#if !library.reachable}
				<p class="error" role="alert">{copy.folderUnreachable(library.path)}</p>
			{/if}
			<button type="button" onclick={scan} disabled={scanning}>
				{scanning ? 'Scanning…' : library.reachable ? 'Scan library' : copy.rescan}
			</button>
		</header>

		{#if progress}
			<!-- Two lines, never one figure over both: probe and metadata finish
			     at different times. The server says which display each supports. -->
			{#if progress.probe.display === 'bar' && progress.probe.total}
				<p class="scan" role="status">{copy.probingOf(progress.probe.done, progress.probe.total)}</p>
				<progress
					aria-label="Probe progress"
					value={progress.probe.done}
					max={progress.probe.total}
				></progress>
			{:else if progress.probe.display === 'count'}
				<p class="scan" role="status">
					{progress.indexPassComplete
						? copy.probing(progress.probe.done, progress.probe.queued)
						: copy.scanFound(progress.found)}
				</p>
			{/if}
			{#if progress.probe.errors > 0}
				<p class="scan" role="status">{copy.probeErrors(progress.probe.errors)}</p>
			{/if}
			{#if progress.metadata.display === 'count'}
				<p class="scan" role="status">{copy.metadataDraining(progress.metadata.pending)}</p>
			{/if}
		{/if}

		{#if listed}
			{#if listed.units.length === 0}
				<p class="empty">No items yet. Run a scan.</p>
			{:else}
				<p class="hint">{copy.unitsSummary(listed.counts.units, listed.counts.items)}</p>
				{#if listed.counts.entityOnly > 0 || listed.counts.unidentified > 0}
					<p class="hint">
						{listed.counts.bound} bound · {listed.counts.entityOnly}
						{copy.identityEntityOnly} · {listed.counts.unidentified}
						{copy.identityUnidentified}
					</p>
				{/if}
				{#if listed.counts.showEntitiesWithoutBinding}
					<p class="hint">
						{copy.showsWithoutBinding(listed.counts.showEntitiesWithoutBinding)}
					</p>
				{/if}
				<ul class="grid">
					{#each listed.units as unit, i (i)}
						<li>
							<a href={unitHref(unit)}>
								<span class="row">
									<span class="title">{unit.title}</span>
									{#if unit.identity === 'entityOnly'}
										<span class="badge">{copy.identityEntityOnly}</span>
									{:else if unit.identity === 'unidentified'}
										<span class="badge bad">{copy.identityUnidentified}</span>
									{/if}
								</span>
								<span class="meta">{unitMeta(unit)}</span>
							</a>
						</li>
					{/each}
				</ul>
			{/if}
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
	progress {
		width: 100%;
		height: 0.5rem;
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
