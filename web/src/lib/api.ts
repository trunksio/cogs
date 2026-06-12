export interface GraphNode {
  id: string;
  label: string;
  kind: string | null;
  status: string | null;
  updated: string | null;
  tags: string[] | null;
  dir: string | null;
  path: string;
}

export interface GraphEdge {
  source: string;
  target: string;
  type: string;
}

export interface GraphPayload {
  nodes: GraphNode[];
  edges: GraphEdge[];
  resources: { id: string; label: string }[];
}

export interface Meta {
  vault_root: string;
  kinds: string[];
  edges: string[];
  has_resources: boolean;
  stale_after_days: number | null;
}

export interface NoteDetail {
  id: string;
  path: string;
  abs_path: string;
  title: string;
  kind: string | null;
  status: string | null;
  updated: string | null;
  tags: string[] | null;
  markdown: string;
  outlinks: { type: string; id: string; title: string | null }[];
  backlinks: { type: string; id: string; title: string | null }[];
}

export interface Health {
  orphans: string[];
  contradictions: { source: string; target: string }[];
  stale: { id: string; updated: string }[];
}

async function get<T>(url: string): Promise<T> {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status} ${await res.text()}`);
  return res.json();
}

export interface SimilarityPair {
  source: string;
  target: string;
  score: number;
  linked: boolean;
}

export const api = {
  meta: () => get<Meta>('/api/meta'),
  similarity: (k = 5, minScore = 0.6) =>
    get<{ pairs: SimilarityPair[]; no_embeddings?: boolean }>(
      `/api/similarity?k=${k}&min_score=${minScore}`,
    ),
  similar: (id: string, k = 8) =>
    get<{ neighbors: { id: string; score: number; linked: boolean }[]; no_embedding?: boolean }>(
      `/api/similar?note=${encodeURIComponent(id)}&k=${k}`,
    ),
  graph: (resources = false) => get<GraphPayload>(`/api/graph?resources=${resources}`),
  note: (id: string) => get<NoteDetail>(`/api/notes/${encodeURIComponent(id)}`),
  search: (q: string) =>
    get<{ hits: { id: string; title: string; kind: string | null; score: number }[] }>(
      `/api/search?q=${encodeURIComponent(q)}`,
    ),
  health: () => get<Health>('/api/health'),
  open: (id: string) =>
    fetch('/api/open', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id }),
    }),
};
