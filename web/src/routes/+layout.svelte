<script lang="ts">
	import { onMount } from 'svelte';
	import { api } from '$lib/api/client';
	import { copy } from '$lib/copy';
	import { clearToken, storeToken, storedToken } from '$lib/session';

	let { children } = $props();

	// `unknown` until the first check finishes, so the app is not drawn behind
	// a login form and is not drawn without one either.
	type Gate = 'unknown' | 'bootstrap' | 'login' | 'in';

	let gate = $state<Gate>('unknown');
	let username = $state('');
	let password = $state('');
	let error = $state<string | null>(null);
	let busy = $state(false);

	async function settle() {
		const setup = await api.getSetupState();
		if (!setup.adminExists) {
			gate = 'bootstrap';
			return;
		}
		if (!storedToken()) {
			gate = 'login';
			return;
		}
		try {
			const session = await api.getSession();
			// Account scope cannot play, so a session that never narrowed is
			// not "logged in" as far as this client is concerned.
			if (session.scope !== 'profile') await narrow();
			gate = 'in';
		} catch {
			// Expired, revoked, or from a database that has since been
			// replaced. There is no refresh token by design (ADR-0034 item 5),
			// so the answer is always to log in again.
			clearToken();
			gate = 'login';
		}
	}

	/// One profile, chosen without asking. Choosing between profiles is
	/// product UI and belongs to Block 3; having *a* profile is what makes the
	/// byte routes work at all.
	async function narrow() {
		const { profiles } = await api.listProfiles();
		if (profiles.length === 0) throw new Error('account has no profile');
		await api.selectProfile(profiles[0].profileRef);
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = null;
		try {
			const body = { username, password, clientLabel: 'demo client' };
			const result =
				gate === 'bootstrap' ? await api.bootstrap(body) : await api.login(body);
			storeToken(result.token);
			password = '';
			await narrow();
			gate = 'in';
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	async function signOut() {
		try {
			await api.logout();
		} catch {
			// Already gone server-side is the same outcome as just revoked.
		}
		clearToken();
		gate = 'login';
	}

	onMount(() => {
		settle().catch((e: Error) => {
			error = e.message;
			gate = 'login';
		});
	});
</script>

{#if gate === 'in'}
	<p class="who">
		<button type="button" onclick={signOut}>{copy.signOut}</button>
	</p>
	{@render children()}
{:else if gate === 'unknown'}
	<p class="settling">{copy.checkingSession}</p>
{:else}
	<main>
		<h1>nightjar</h1>
		<p class="hint">
			{gate === 'bootstrap' ? copy.bootstrapHint : copy.loginHint}
		</p>
		<form onsubmit={submit}>
			<label>
				Username
				<input bind:value={username} autocomplete="username" required />
			</label>
			<label>
				Password
				<input
					type="password"
					bind:value={password}
					autocomplete="current-password"
					required
				/>
			</label>
			<button type="submit" disabled={busy}>
				{gate === 'bootstrap' ? copy.createOwner : copy.signIn}
			</button>
		</form>
		{#if error}
			<p class="error" role="alert">{error}</p>
		{/if}
	</main>
{/if}

<style>
	main {
		max-width: 22rem;
		margin: 0 auto;
		padding: 4rem 1.25rem;
	}
	h1 {
		font-family: 'Bricolage Grotesque', system-ui, sans-serif;
		font-size: 2rem;
		font-weight: 700;
		margin: 0;
	}
	.hint,
	.settling {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	.settling {
		padding: 4rem 1.25rem;
		text-align: center;
	}
	form {
		display: grid;
		gap: 0.75rem;
		margin-top: 1.5rem;
	}
	label {
		display: grid;
		gap: 0.25rem;
		font-size: 0.875rem;
	}
	input {
		font: inherit;
		padding: 0.5rem 0.6rem;
		border-radius: 8px;
		border: 1px solid var(--night-line);
		background: var(--night-raised);
		color: inherit;
	}
	button {
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
	button:focus-visible,
	input:focus-visible {
		outline: 2px solid var(--dusk);
		outline-offset: 2px;
	}
	.who {
		display: flex;
		justify-content: flex-end;
		max-width: 56rem;
		margin: 0 auto;
		padding: 0.75rem 1.25rem 0;
	}
	.who button {
		background: none;
		color: var(--moth-dim);
		font-weight: 400;
		padding: 0.25rem 0.4rem;
	}
	.error {
		color: var(--dusk);
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
	}
</style>
