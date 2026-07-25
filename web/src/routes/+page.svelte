<script lang="ts">
	import { onMount } from 'svelte';
	import { api } from '$lib/api/client';
	import type { components } from '$lib/api/schema';

	type Library = components['schemas']['Library'];

	let libraries = $state<Library[]>([]);
	let error = $state<string | null>(null);
	let name = $state('');
	let path = $state('');
	let kind = $state<'movies' | 'shows'>('movies');
	let busy = $state(false);

	async function refresh() {
		const res = await api.listLibraries();
		libraries = res.libraries;
	}

	onMount(() => {
		refresh().catch((e: Error) => {
			error = e.message;
		});
	});

	async function addLibrary(e: Event) {
		e.preventDefault();
		busy = true;
		error = null;
		try {
			await api.createLibrary({ name, path, kind });
			name = '';
			path = '';
			await refresh();
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head>
	<title>nightjar</title>
</svelte:head>

<main>
	<header>
		<a class="mark" href="/">nightjar</a>
		<p class="tag">Comes alive when the lights go out.</p>
	</header>

	{#if error}
		<p class="error" role="alert">{error}</p>
	{/if}

	<section>
		<h1>Libraries</h1>
		{#if libraries.length === 0}
			<p class="empty">Nothing roosting here yet. Add a media folder below.</p>
		{:else}
			<ul class="libs">
				{#each libraries as lib (lib.id)}
					<li>
						<a href="/libraries/{lib.id}">
							<span class="name">{lib.name}</span>
							<span class="meta">{lib.kind} · {lib.itemCount} items</span>
						</a>
					</li>
				{/each}
			</ul>
		{/if}
	</section>

	<section>
		<h2>Add folder</h2>
		<form onsubmit={addLibrary}>
			<label>
				Name
				<input bind:value={name} required autocomplete="off" />
			</label>
			<label>
				Path on this machine
				<input bind:value={path} required placeholder="/media/movies" autocomplete="off" />
			</label>
			<label>
				Kind
				<select bind:value={kind}>
					<option value="movies">movies</option>
					<option value="shows">shows</option>
				</select>
			</label>
			<button type="submit" disabled={busy}>{busy ? 'Adding…' : 'Add folder'}</button>
		</form>
	</section>
</main>

<style>
	main {
		max-width: 40rem;
		margin: 0 auto;
		padding: 2rem 1.25rem 4rem;
	}
	header {
		margin-bottom: 2.5rem;
	}
	.mark {
		font-family: 'Bricolage Grotesque', system-ui, sans-serif;
		font-size: 2rem;
		font-weight: 700;
		letter-spacing: -0.03em;
		color: var(--moth);
		text-decoration: none;
	}
	.tag {
		margin: 0.35rem 0 0;
		color: var(--moth-dim);
	}
	h1,
	h2 {
		font-family: 'Bricolage Grotesque', system-ui, sans-serif;
		font-weight: 600;
		font-size: 1.25rem;
		margin: 0 0 1rem;
	}
	section + section {
		margin-top: 2.5rem;
	}
	.empty {
		color: var(--moth-dim);
	}
	.libs {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.libs li + li {
		border-top: 1px solid var(--night-line, #2a2e36);
	}
	.libs a {
		display: flex;
		flex-direction: column;
		gap: 0.2rem;
		padding: 0.85rem 0;
		color: inherit;
		text-decoration: none;
	}
	.libs a:focus-visible {
		outline: 2px solid var(--dusk);
		outline-offset: 4px;
	}
	.name {
		font-size: 1.125rem;
	}
	.meta {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	form {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	label {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	input,
	select,
	button {
		font: inherit;
		padding: 0.6rem 0.75rem;
		border-radius: 8px;
		border: 1px solid var(--night-line, #2a2e36);
		background: var(--night-raised, #191c22);
		color: var(--moth);
	}
	button {
		background: var(--dusk);
		color: var(--night);
		border: none;
		font-weight: 600;
		cursor: pointer;
		align-self: start;
	}
	button:disabled {
		opacity: 0.6;
		cursor: not-allowed;
	}
	button:focus-visible,
	input:focus-visible,
	select:focus-visible {
		outline: 2px solid var(--dusk);
		outline-offset: 2px;
	}
	.error {
		color: var(--dusk);
	}
</style>
