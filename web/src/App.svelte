<script lang="ts">
  import { onMount } from 'svelte';
  import type Graph from 'graphology';
  import type Sigma from 'sigma';
  import { api, type Meta, type Health } from './lib/api';
  import {
    buildGraph,
    layout,
    createSigma,
    type VizFilters,
    type HealthMarks,
    EDGE_COLORS,
  } from './lib/viz';
  import FilterRail from './components/FilterRail.svelte';
  import DetailPanel from './components/DetailPanel.svelte';

  let container: HTMLElement;
  let graph: Graph | null = $state(null);
  let sigma: Sigma | null = null;
  let meta: Meta | null = $state(null);
  let health: Health | null = $state(null);
  let healthMarks: HealthMarks | null = null;
  let loading = $state(true);
  let error: string | null = $state(null);

  let selection: string | null = $state(null);
  let hover: string | null = null;

  // Filters (mutated by FilterRail, read by sigma reducers).
  const filters: VizFilters = $state({
    kinds: new Set<string>(),
    statuses: new Set<string>(),
    tags: new Set<string>(),
    edgeTypes: new Set<string>(),
    searchHits: null,
    healthOverlay: false,
    timeTint: false,
    timeRange: null,
    semanticOverlay: false,
    simThreshold: 0.75,
    simUnlinkedOnly: true,
  });

  let semanticLoaded = false;
  let semanticUnavailable = $state(false);

  async function toggleSemantic() {
    filters.semanticOverlay = !filters.semanticOverlay;
    if (filters.semanticOverlay && !semanticLoaded && graph) {
      try {
        const { pairs, no_embeddings } = await api.similarity(5, 0.6);
        if (no_embeddings) {
          semanticUnavailable = true;
          filters.semanticOverlay = false;
          return;
        }
        for (const p of pairs) {
          if (graph.hasNode(p.source) && graph.hasNode(p.target)) {
            graph.addEdge(p.source, p.target, {
              edgeType: 'SIMILAR',
              score: p.score,
              linked: p.linked,
            });
          }
        }
        semanticLoaded = true;
      } catch {
        semanticUnavailable = true;
        filters.semanticOverlay = false;
        return;
      }
    }
    refresh();
  }

  let searchText = $state('');
  let searchTimer: ReturnType<typeof setTimeout> | undefined;

  function refresh() {
    sigma?.refresh({ skipIndexation: true });
  }

  async function runSearch(q: string) {
    if (!q.trim()) {
      filters.searchHits = null;
      refresh();
      return;
    }
    try {
      const { hits } = await api.search(q);
      filters.searchHits = new Set(hits.map((h) => h.id));
      refresh();
    } catch {
      // FTS may be unavailable; fall back to label substring match.
      const set = new Set<string>();
      const needle = q.toLowerCase();
      graph?.forEachNode((n, a) => {
        if (
          n.toLowerCase().includes(needle) ||
          ((a.label as string) ?? '').toLowerCase().includes(needle)
        )
          set.add(n);
      });
      filters.searchHits = set;
      refresh();
    }
  }

  function onSearchInput() {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => runSearch(searchText), 250);
  }

  async function toggleHealth() {
    filters.healthOverlay = !filters.healthOverlay;
    if (filters.healthOverlay && !health) {
      health = await api.health();
      healthMarks = {
        orphans: new Set(health.orphans),
        staleIds: new Set(health.stale.map((s) => s.id)),
        contradictionEdges: new Set(
          health.contradictions.map((c) => `${c.source}->${c.target}`),
        ),
      };
    }
    refresh();
  }

  function toggleTime() {
    filters.timeTint = !filters.timeTint;
    refresh();
  }

  function focusNode(id: string) {
    selection = id;
    if (!sigma || !graph?.hasNode(id)) return;
    const pos = sigma.getNodeDisplayData(id);
    if (pos) {
      sigma.getCamera().animate({ x: pos.x, y: pos.y, ratio: 0.25 }, { duration: 500 });
    }
    refresh();
  }

  onMount(async () => {
    try {
      meta = await api.meta();
      const payload = await api.graph(false);
      const g = buildGraph(payload);
      layout(g);
      graph = g;
      filters.edgeTypes = new Set(meta.edges);
      sigma = createSigma(
        g,
        container,
        () => filters,
        () => (filters.healthOverlay ? healthMarks : null),
        () => selection,
        () => hover,
      );
      sigma.on('clickNode', ({ node }) => {
        selection = selection === node ? null : node;
        refresh();
      });
      sigma.on('clickStage', () => {
        selection = null;
        refresh();
      });
      sigma.on('enterNode', ({ node }) => {
        hover = node;
        refresh();
      });
      sigma.on('leaveNode', () => {
        hover = null;
        refresh();
      });
      loading = false;
    } catch (e) {
      error = String(e);
      loading = false;
    }
  });
</script>

<div class="shell">
  <header>
    <span class="logo">cogs</span>
    <input
      type="search"
      placeholder="search notes…"
      bind:value={searchText}
      oninput={onSearchInput}
    />
    <div class="modes">
      <button
        class:active={filters.semanticOverlay}
        disabled={semanticUnavailable}
        title={semanticUnavailable
          ? 'no embeddings — run cogs sync --with-embeddings'
          : 'embedding-similarity overlay'}
        onclick={toggleSemantic}>semantic</button
      >
      <button class:active={filters.healthOverlay} onclick={toggleHealth}>health</button>
      <button class:active={filters.timeTint} onclick={toggleTime}>time</button>
    </div>
    {#if filters.semanticOverlay}
      <div class="sim-controls">
        <input
          type="range"
          min="0.6"
          max="0.95"
          step="0.01"
          bind:value={filters.simThreshold}
          oninput={refresh}
        />
        <span class="sim-label">≥ {filters.simThreshold.toFixed(2)}</span>
        <label class="facet">
          <input
            type="checkbox"
            bind:checked={filters.simUnlinkedOnly}
            onchange={refresh}
          />
          unlinked only
        </label>
      </div>
    {/if}
    <span class="status">
      {#if graph}{graph.order} notes · {graph.size} edges{/if}
      {#if loading}loading…{/if}
    </span>
    <span class="legend">
      {#each Object.entries(EDGE_COLORS).filter(([k]) => meta?.edges.includes(k)) as [name, color]}
        <span class="edge-key"><i style="background:{color}"></i>{name.toLowerCase()}</span>
      {/each}
    </span>
  </header>

  <div class="body">
    {#if graph}
      <FilterRail {graph} {filters} edgeTypes={meta?.edges ?? []} onchange={refresh} />
    {/if}

    <main bind:this={container}>
      {#if error}
        <div class="error">{error}</div>
      {/if}
    </main>

    {#if selection}
      <DetailPanel
        id={selection}
        onnavigate={(id) => focusNode(id)}
        onclose={() => {
          selection = null;
          refresh();
        }}
      />
    {/if}
  </div>
</div>

<style>
  .shell {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  header {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 8px 14px;
    background: var(--panel);
    border-bottom: 1px solid var(--panel-border);
  }

  .logo {
    font-weight: 700;
    font-size: 15px;
    color: var(--accent);
    letter-spacing: 0.04em;
  }

  .modes {
    display: flex;
    gap: 6px;
  }

  .sim-controls {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    color: var(--text-dim);
  }

  .sim-controls input[type='range'] {
    width: 110px;
    accent-color: #2ea88a;
  }

  .sim-label {
    min-width: 44px;
  }

  .status {
    font-size: 11px;
    color: var(--text-dim);
  }

  .legend {
    margin-left: auto;
    display: flex;
    gap: 10px;
    font-size: 10px;
    color: var(--text-dim);
  }

  .edge-key {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }

  .edge-key i {
    width: 14px;
    height: 2px;
    display: inline-block;
  }

  .body {
    flex: 1;
    display: flex;
    min-height: 0;
    position: relative;
  }

  main {
    flex: 1;
    position: relative;
  }

  .error {
    position: absolute;
    inset: 40%;
    color: #e8684a;
    font-size: 13px;
  }
</style>
