<script lang="ts">
  import { marked } from 'marked';
  import DOMPurify from 'dompurify';
  import { api, type NoteDetail } from '../lib/api';

  let {
    id,
    onnavigate,
    onclose,
  }: {
    id: string;
    onnavigate: (id: string) => void;
    onclose: () => void;
  } = $props();

  let note: NoteDetail | null = $state(null);
  let error: string | null = $state(null);
  let similar: { id: string; score: number; linked: boolean }[] = $state([]);

  $effect(() => {
    note = null;
    error = null;
    similar = [];
    api
      .note(id)
      .then((n) => (note = n))
      .catch((e) => (error = String(e)));
    api
      .similar(id, 8)
      .then((r) => (similar = r.neighbors ?? []))
      .catch(() => {});
  });

  function rendered(md: string): string {
    // Strip frontmatter for display, then render wikilinks as plain emphasis.
    const body = md.replace(/^---\n[\s\S]*?\n---\n/, '');
    const html = marked.parse(body, { async: false }) as string;
    return DOMPurify.sanitize(html);
  }

  async function openInZed() {
    if (note) await api.open(note.id);
  }
</script>

<aside>
  <div class="head">
    <button class="close" onclick={onclose}>✕</button>
    {#if note}
      <h2>{note.title}</h2>
      <div class="meta">
        {#if note.kind}<span class="chip">{note.kind}</span>{/if}
        {#if note.status}<span class="chip">{note.status}</span>{/if}
        {#if note.updated}<span class="chip">{note.updated}</span>{/if}
      </div>
      <div class="actions">
        <button onclick={openInZed}>open in Zed</button>
      </div>
    {:else if error}
      <p class="error">{error}</p>
    {:else}
      <p>loading…</p>
    {/if}
  </div>

  {#if note}
    <div class="links">
      {#if note.backlinks.length}
        <h4>{note.backlinks.length} backlinks</h4>
        {#each note.backlinks as l}
          <button class="link" onclick={() => onnavigate(l.id)}>
            ← {l.title ?? l.id}
            <span class="etype">{l.type.toLowerCase()}</span>
          </button>
        {/each}
      {/if}
      {#if note.outlinks.length}
        <h4>{note.outlinks.length} outlinks</h4>
        {#each note.outlinks as l}
          <button class="link" onclick={() => onnavigate(l.id)}>
            → {l.title ?? l.id}
            <span class="etype">{l.type.toLowerCase()}</span>
          </button>
        {/each}
      {/if}
      {#if similar.length}
        <h4>similar</h4>
        {#each similar as s}
          <button class="link" onclick={() => onnavigate(s.id)}>
            ≈ {s.id}
            <span class="etype" class:unlinked={!s.linked}>
              {s.score.toFixed(2)}{s.linked ? '' : ' · not linked'}
            </span>
          </button>
        {/each}
      {/if}
    </div>

    <article>
      <!-- eslint-disable-next-line svelte/no-at-html-tags — sanitized above -->
      {@html rendered(note.markdown)}
    </article>
  {/if}
</aside>

<style>
  aside {
    width: 380px;
    background: var(--panel);
    border-left: 1px solid var(--panel-border);
    overflow-y: auto;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
  }

  .head {
    padding: 14px 16px 8px;
    border-bottom: 1px solid var(--panel-border);
    position: relative;
  }

  .close {
    position: absolute;
    top: 10px;
    right: 10px;
    border: none;
    background: none;
    color: var(--text-dim);
    font-size: 14px;
  }

  h2 {
    margin: 0 24px 6px 0;
    font-size: 16px;
  }

  .meta {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }

  .chip {
    font-size: 10px;
    background: var(--bg);
    border: 1px solid var(--panel-border);
    border-radius: 10px;
    padding: 1px 8px;
    color: var(--text-dim);
  }

  .actions {
    margin-top: 8px;
  }

  .links {
    padding: 8px 16px;
    border-bottom: 1px solid var(--panel-border);
    max-height: 220px;
    overflow-y: auto;
  }

  .links h4 {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--text-dim);
    margin: 8px 0 4px;
  }

  button.link {
    display: flex;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    padding: 2px 0;
    font-size: 12px;
    color: var(--text);
    gap: 6px;
  }

  button.link:hover {
    color: var(--accent);
  }

  .etype {
    margin-left: auto;
    font-size: 9px;
    color: var(--text-dim);
  }

  .etype.unlinked {
    color: #2ea88a;
  }

  article {
    padding: 12px 16px 40px;
    font-size: 13px;
    line-height: 1.55;
  }

  article :global(h1),
  article :global(h2),
  article :global(h3) {
    font-size: 14px;
    margin: 14px 0 6px;
  }

  article :global(a) {
    color: var(--accent);
  }

  article :global(code) {
    background: var(--bg);
    padding: 1px 4px;
    border-radius: 3px;
    font-size: 12px;
  }

  article :global(pre) {
    background: var(--bg);
    padding: 8px;
    border-radius: 6px;
    overflow-x: auto;
  }

  .error {
    color: #e8684a;
  }
</style>
