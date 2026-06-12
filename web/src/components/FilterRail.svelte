<script lang="ts">
  import type Graph from 'graphology';
  import type { VizFilters } from '../lib/viz';

  let {
    graph,
    filters,
    edgeTypes,
    onchange,
  }: {
    graph: Graph;
    filters: VizFilters;
    edgeTypes: string[];
    onchange: () => void;
  } = $props();

  function facetCounts(attr: 'kind' | 'status'): [string, number][] {
    const counts = new Map<string, number>();
    graph.forEachNode((_, a) => {
      const v = (a[attr] as string) || '(none)';
      counts.set(v, (counts.get(v) ?? 0) + 1);
    });
    return [...counts.entries()].sort((a, b) => b[1] - a[1]);
  }

  function tagCounts(): [string, number][] {
    const counts = new Map<string, number>();
    graph.forEachNode((_, a) => {
      for (const t of (a.tags as string[]) ?? []) {
        counts.set(t, (counts.get(t) ?? 0) + 1);
      }
    });
    return [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 30);
  }

  const kinds = facetCounts('kind');
  const statuses = facetCounts('status');
  const tags = tagCounts();

  function toggle(set: Set<string>, value: string) {
    if (set.has(value)) set.delete(value);
    else set.add(value);
    onchange();
  }
</script>

<aside>
  <section>
    <h3>kind</h3>
    {#each kinds as [value, count]}
      <label class="facet">
        <input
          type="checkbox"
          checked={filters.kinds.has(value)}
          onchange={() => toggle(filters.kinds, value)}
        />
        {value}
        <span class="count">{count}</span>
      </label>
    {/each}
  </section>

  {#if statuses.length > 1}
    <section>
      <h3>status</h3>
      {#each statuses as [value, count]}
        <label class="facet">
          <input
            type="checkbox"
            checked={filters.statuses.has(value)}
            onchange={() => toggle(filters.statuses, value)}
          />
          {value}
          <span class="count">{count}</span>
        </label>
      {/each}
    </section>
  {/if}

  {#if edgeTypes.length > 1}
    <section>
      <h3>edges</h3>
      {#each edgeTypes as value}
        <label class="facet">
          <input
            type="checkbox"
            checked={filters.edgeTypes.has(value)}
            onchange={() => toggle(filters.edgeTypes, value)}
          />
          {value.toLowerCase()}
        </label>
      {/each}
    </section>
  {/if}

  {#if tags.length}
    <section>
      <h3>tags</h3>
      {#each tags as [value, count]}
        <label class="facet">
          <input
            type="checkbox"
            checked={filters.tags.has(value)}
            onchange={() => toggle(filters.tags, value)}
          />
          #{value}
          <span class="count">{count}</span>
        </label>
      {/each}
    </section>
  {/if}
</aside>

<style>
  aside {
    width: 200px;
    background: var(--panel);
    border-right: 1px solid var(--panel-border);
    overflow-y: auto;
    padding: 10px 12px;
    flex-shrink: 0;
  }

  h3 {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--text-dim);
    margin: 14px 0 4px;
  }

  section:first-child h3 {
    margin-top: 0;
  }

  input[type='checkbox'] {
    accent-color: var(--accent);
  }
</style>
