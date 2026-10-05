<script lang="ts">
	// #72: the local Brainprint Web UI. A thin view: every fact is the
	// bridge's JSON (the daemon's answer), every word is the shared
	// catalogue's (`brainprint_core::present`). Nothing is computed here.
	import { onMount } from 'svelte';

	type Json = any;
	type Connection = 'unknown' | 'connected' | 'disconnected' | 'incompatible';

	let messages: Record<string, string> = $state({});
	let locales: string[] = $state(['en']);
	let changes: string[] = $state([]);
	let locale = $state('en');
	let connection: Connection = $state('unknown');
	let detail = $state('');
	let error = $state('');
	let tab: 'overview' | 'explorer' | 'context' = $state('overview');
	let sub: 'inspect' | 'relations' | 'impact' = $state('inspect');

	let status: Json = $state(null);
	let work: Json = $state(null);
	let rules: Json = $state(null);
	let query = $state('');
	let found: Json = $state(null);
	let target: { label: string; token: string } | null = $state(null);
	let inspected: Json = $state(null);
	let relations: Json = $state(null);
	let impact: Json = $state(null);
	let change = $state(0);

	// A missing key shows itself rather than an empty label.
	const t = (key: string | null | undefined) => (key ? (messages[key] ?? `[${key}]`) : '');

	function forget() {
		status = work = rules = found = inspected = relations = impact = null;
		target = null;
	}

	async function api(path: string): Promise<Json | null> {
		let reply: Json;
		try {
			reply = await (await fetch(path)).json();
		} catch (e) {
			connection = 'disconnected';
			detail = String(e);
			forget();
			return null;
		}
		if (reply.connection !== 'connected') {
			// Gone or incompatible: nothing on screen may stay "current".
			connection = reply.connection ?? 'disconnected';
			detail = reply.detail ?? reply.error ?? '';
			forget();
			return null;
		}
		connection = 'connected';
		detail = '';
		if (reply.error) {
			error = reply.error;
			return null;
		}
		return reply.data;
	}

	async function catalogue(tag?: string) {
		const reply = await (await fetch(`/api/catalogue${tag ? `?locale=${tag}` : ''}`)).json();
		messages = reply.messages;
		locales = reply.locales;
		changes = reply.changes;
		locale = reply.locale;
		document.documentElement.lang = locale;
	}

	async function refresh() {
		error = '';
		if (tab === 'overview') {
			status = await api('/api/status');
			if (connection === 'connected') work = await api('/api/work');
		} else if (tab === 'context') {
			work = await api('/api/work');
			if (connection === 'connected') rules = await api('/api/rules');
		} else if (target) {
			await open(sub);
		}
	}

	async function search(event: Event) {
		event.preventDefault();
		if (!query.trim()) return;
		error = '';
		target = null;
		inspected = relations = impact = null;
		found = await api(`/api/find?q=${encodeURIComponent(query.trim())}`);
	}

	const tokenParam = () => `target=${encodeURIComponent(target!.token)}`;

	async function open(view: typeof sub) {
		sub = view;
		if (!target) return;
		error = '';
		if (view === 'inspect') inspected = await api(`/api/inspect?${tokenParam()}`);
		if (view === 'relations') relations = await api(`/api/relations?${tokenParam()}`);
		if (view === 'impact') impact = await api(`/api/impact?${tokenParam()}&change=${change}`);
	}

	async function choose(candidate: { label: string; token: string }) {
		target = candidate;
		inspected = relations = impact = null;
		await open(sub);
	}

	async function more(view: 'inspect' | 'impact') {
		const current = view === 'inspect' ? inspected : impact;
		if (!current?.continuation) return;
		const extra = view === 'impact' ? `&change=${change}` : '';
		const next = await api(
			`/api/${view}?${tokenParam()}${extra}&continuation=${encodeURIComponent(current.continuation)}`
		);
		if (!next) return;
		next.body = [...current.body, '', ...next.body];
		next.pages = (current.pages ?? 1) + 1;
		if (view === 'inspect') inspected = next;
		else impact = next;
	}

	async function switchTab(next: typeof tab) {
		tab = next;
		await refresh();
	}

	// The daemon's own reason text, untranslated.
	function reason(value: Json): string {
		if (!value || typeof value !== 'object') return '';
		const inner = Object.values(value)[0] as Json;
		return inner && typeof inner === 'object' ? (inner.reason ?? inner.detail ?? '') : '';
	}

	const report = $derived(status?.status?.workspace?.Initialized ?? null);
	const unresolved = $derived(
		status?.status?.workspace?.NotInitialized ?? status?.status?.workspace?.Ambiguous ?? null
	);
	const basis = $derived(report?.basis?.Stable ?? null);

	onMount(async () => {
		await catalogue();
		await refresh();
	});
</script>

<header>
	<strong>Brainprint</strong>
	<nav>
		<button class:active={tab === 'overview'} onclick={() => switchTab('overview')}>{t('tab.overview')}</button>
		<button class:active={tab === 'explorer'} onclick={() => switchTab('explorer')}>{t('tab.explorer')}</button>
		<button class:active={tab === 'context'} onclick={() => switchTab('context')}>{t('tab.context')}</button>
	</nav>
	<span class="connection {connection}">
		{connection === 'connected'
			? t('connection.connected')
			: connection === 'incompatible'
				? t('connection.incompatible')
				: connection === 'disconnected'
					? t('connection.disconnected')
					: t('connection.not_yet')}
	</span>
	<button onclick={refresh}>{t('action.refresh')}</button>
	<label>
		{t('label.locale')}
		<select value={locale} onchange={(event) => catalogue(event.currentTarget.value)}>
			{#each locales as tag (tag)}<option value={tag}>{tag}</option>{/each}
		</select>
	</label>
</header>

<main>
	{#if connection === 'disconnected' || connection === 'incompatible'}
		<section class="notice">
			<p>{detail}</p>
			<p>{t('connection.cleared')}</p>
		</section>
	{/if}
	{#if error}<p class="error">{t('result.error')}: {error}</p>{/if}

	{#if tab === 'overview' && status}
		<section>
			<dl>
				<dt>{t('label.daemon')}</dt>
				<dd>brainprintd {status.status.daemon_version} (pid {status.status.pid})</dd>
				<dt>{t('label.protocol')}</dt>
				<dd>{status.status.protocol_version} / {t('label.client_protocol')} {status.client_protocol_version}</dd>
				<dt>{t('label.uptime')}</dt>
				<dd>{status.status.uptime_seconds}</dd>
			</dl>
		</section>
		{#if unresolved}
			<section>
				<dl>
					<dt>{t('label.workspace')}</dt>
					<dd>{t(status.keys.workspace)} -- {unresolved.reason ?? unresolved.workspaces?.join(', ')}</dd>
					<dt>{t('label.root')}</dt>
					<dd>{unresolved.path}</dd>
				</dl>
			</section>
		{:else if report}
			<section>
				<dl>
					<dt>{t('label.project')}</dt>
					<dd>{report.project_id}</dd>
					<dt>{t('label.workspace')}</dt>
					<dd>{report.workspace_id}</dd>
					<dt>{t('label.root')}</dt>
					<dd>{report.workspace_root}</dd>
					{#if basis}
						<dt>{t('label.revision')}</dt>
						<dd>{basis.workspace_revision}</dd>
						<dt>{t('label.generation')}</dt>
						<dd>{basis.generation_no} ({t('label.basis_revision')} {basis.generation_basis_revision})</dd>
						<dt>{t('label.incarnation')}</dt>
						<dd>{basis.index_incarnation}</dd>
					{:else}
						<dt>{t('label.generation')}</dt>
						<dd>{t(status.keys.basis)} {reason(report.basis)}</dd>
					{/if}
					<dt>{t('label.currentness')}</dt>
					<dd>{t(status.keys.currentness)} {reason(report.index)}</dd>
					<dt>{t('label.runtime')}</dt>
					<dd>{t(status.keys.runtime)} {reason(report.runtime)}</dd>
					{#if status.keys.watcher}
						<dt>{t('label.watcher')}</dt>
						<dd>{t(status.keys.watcher)} {reason(report.runtime.Active?.watcher)}</dd>
					{/if}
				</dl>
				<h3>{t('label.capabilities')}</h3>
				<dl>
					{#each status.keys.capabilities as capability (capability.name)}
						<dt>{t(capability.name)}</dt>
						<dd>{t(capability.state)}</dd>
					{/each}
				</dl>
				<h3>{t('label.semantic')}</h3>
				<dl>
					{#each status.keys.backends as backend (backend.family)}
						<dt>{backend.family}</dt>
						<dd>{t(backend.state)}{backend.reason ? ` -- ${backend.reason}` : ''}</dd>
					{/each}
				</dl>
			</section>
		{/if}
	{/if}

	{#if (tab === 'overview' || tab === 'context') && work}
		<section>
			<h3>{t('label.working_state')}</h3>
			{#if work.items.length === 0}<p>{t('work.none')}</p>{/if}
			<ul>
				{#each work.items as entry (entry.item.uid)}
					<li>[{t(entry.status)}] {entry.item.title ?? entry.item.goal}{entry.item.source_ref ? ` (${entry.item.source_ref})` : ''}</li>
				{/each}
			</ul>
			{#if work.truncated}<p>{t('work.truncated')}</p>{/if}
		</section>
	{/if}

	{#if tab === 'context' && rules}
		<section>
			<h3>{t('label.rules')}</h3>
			{#if rules.policies.length === 0}<p>{t('label.no_rules')}</p>{/if}
			<ul>
				{#each rules.policies as policy (policy.uid)}<li><strong>{policy.title}</strong> -- {policy.rule_text}</li>{/each}
			</ul>
			<h3>{t('label.decisions')}</h3>
			{#if rules.decisions.length === 0}<p>{t('label.no_decisions')}</p>{/if}
			<ul>
				{#each rules.decisions as decision (decision.uid)}<li><strong>{decision.topic}</strong> -- {decision.chosen_summary}</li>{/each}
			</ul>
			{#if rules.gaps > 0}<p>{t('label.gaps')}: {rules.gaps}</p>{/if}
		</section>
	{/if}

	{#if tab === 'explorer'}
		<section>
			<form onsubmit={search}>
				<label>{t('label.search')} <input bind:value={query} placeholder="helper, src/app.ts" /></label>
				<button type="submit">{t('label.search')}</button>
			</form>
			{#if found}
				<p>{t('label.candidates')}: {t(found.resolution)} · {t(found.currentness)}</p>
				<ul class="candidates">
					{#each found.candidates as candidate (candidate.token)}
						<li><button class:active={target?.token === candidate.token} onclick={() => choose(candidate)}>{candidate.label}</button></li>
					{/each}
				</ul>
			{:else}
				<p>{t('hint.search')}</p>
			{/if}
		</section>

		{#if target}
			<section>
				<p><strong>{t('label.target')}:</strong> {target.label}</p>
				<nav>
					<button class:active={sub === 'inspect'} onclick={() => open('inspect')}>{t('tab.inspect')}</button>
					<button class:active={sub === 'relations'} onclick={() => open('relations')}>{t('tab.relations')}</button>
					<button class:active={sub === 'impact'} onclick={() => open('impact')}>{t('tab.impact')}</button>
				</nav>

				{#if sub === 'impact'}
					<label>
						{t('label.change_kind')}
						<select
							value={change}
							onchange={(event) => {
								change = Number(event.currentTarget.value);
								open('impact');
							}}
						>
							{#each changes as key, index (key)}<option value={index}>{t(key)}</option>{/each}
						</select>
					</label>
				{/if}

				{#if sub === 'relations' && relations}
					<dl>
						<dt>{t('label.currentness')}</dt>
						<dd>{t(relations.currentness)}</dd>
						<dt>{t('label.resolution')}</dt>
						<dd>{t(relations.resolution)}</dd>
						{#each relations.answers as answer (answer.direction)}
							<dt>{t(answer.direction)}</dt>
							<dd>
								{#if answer.none}
									{t(answer.none)}
								{:else}
									{t('label.confirmed')} {answer.confirmed}
									({answer.kinds.map((kind: Json) => `${kind.kind} ${kind.count}`).join(', ')})
									· {t('label.coverage')}: {t(answer.coverage)}
								{/if}
								{#if answer.gaps > 0} · {t('label.gaps')}: {answer.gaps}{/if}
							</dd>
						{/each}
					</dl>
					<pre>{relations.body.join('\n')}</pre>
				{/if}

				{#each [['inspect', inspected], ['impact', impact]] as [view, paged] (view)}
					{#if sub === view && paged}
						<dl>
							<dt>{t('label.currentness')}</dt>
							<dd>{t(paged.currentness)}</dd>
							<dt>{t('label.resolution')}</dt>
							<dd>{t(paged.resolution)}</dd>
							<dt>{t('label.page')}</dt>
							<dd>{paged.pages ?? 1}</dd>
						</dl>
						{#if paged.more_available}
							<p>
								{t('delivery.more_available')}
								<button onclick={() => more(view as 'inspect' | 'impact')}>{t('action.load_more')}</button>
							</p>
						{:else}
							<p>{t('delivery.complete')}</p>
						{/if}
						<pre>{paged.body.join('\n')}</pre>
					{/if}
				{/each}
			</section>
		{/if}
	{/if}
</main>

<style>
	:global(body) {
		margin: 0;
		font: 14px/1.45 system-ui, sans-serif;
		color: #1d1d1f;
		background: #fafafa;
	}
	header {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem 1rem;
		align-items: center;
		padding: 0.6rem 1rem;
		border-bottom: 1px solid #ddd;
		background: #fff;
	}
	nav {
		display: flex;
		flex-wrap: wrap;
		gap: 0.25rem;
	}
	main {
		padding: 0.5rem 1rem 2rem;
		max-width: 72rem;
	}
	section {
		background: #fff;
		border: 1px solid #e3e3e3;
		border-radius: 6px;
		padding: 0.5rem 1rem;
		margin: 0.75rem 0;
	}
	dl {
		display: grid;
		grid-template-columns: minmax(8rem, max-content) 1fr;
		gap: 0.15rem 1rem;
		margin: 0.5rem 0;
	}
	dt {
		font-weight: 600;
	}
	dd {
		margin: 0;
		overflow-wrap: anywhere;
	}
	pre {
		overflow-x: auto;
		background: #f4f4f4;
		padding: 0.5rem;
		font-size: 12px;
	}
	button.active {
		font-weight: 700;
		text-decoration: underline;
	}
	.candidates button {
		text-align: left;
		font-family: ui-monospace, monospace;
	}
	.connection.connected {
		color: #1a7f37;
	}
	.connection.disconnected,
	.connection.incompatible,
	.error {
		color: #b42318;
	}
	.notice {
		border-color: #b42318;
	}
	@media (max-width: 40rem) {
		dl {
			grid-template-columns: 1fr;
		}
		dt {
			margin-top: 0.4rem;
		}
	}
</style>
