<script lang="ts">
	import { onMount } from 'svelte';

	let health = $state<string>('…');

	onMount(async () => {
		try {
			const res = await fetch('/api/health');
			const body = await res.json();
			health = body.status === 'ok' ? `v${body.version}` : 'unreachable';
		} catch {
			health = 'unreachable';
		}
	});
</script>

<svelte:head>
	<title>nightjar</title>
</svelte:head>

<main>
	<p class="mark">nightjar</p>
	<p class="tag">Comes alive when the lights go out.</p>
	<p class="meta">{health}</p>
</main>

<style>
	main {
		min-height: 100vh;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 0.75rem;
		padding: 2rem;
		text-align: center;
	}

	.mark {
		margin: 0;
		font-family: 'Bricolage Grotesque', system-ui, sans-serif;
		font-size: 3rem;
		font-weight: 700;
		letter-spacing: -0.03em;
		text-transform: lowercase;
	}

	.tag {
		margin: 0;
		color: var(--moth-dim);
		font-size: 1.125rem;
	}

	.meta {
		margin: 1rem 0 0;
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--dusk);
	}
</style>
