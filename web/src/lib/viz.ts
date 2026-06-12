import Graph from 'graphology';
import louvain from 'graphology-communities-louvain';
import { circlepack } from 'graphology-layout';
import forceAtlas2 from 'graphology-layout-forceatlas2';
import Sigma from 'sigma';
import type { GraphPayload } from './api';

// Community palette (dark-theme friendly).
const PALETTE = [
  '#5b8ff9', '#61ddaa', '#f6bd16', '#7262fd', '#78d3f8',
  '#9661bc', '#f6903d', '#008685', '#f08bb4', '#65789b',
  '#e8684a', '#6dc8ec', '#9270ca', '#269a99', '#ff9d4d',
];

export const EDGE_COLORS: Record<string, string> = {
  CITES: '#3d4252',
  LINKS_TO: '#3d4252',
  CONTRADICTS: '#e8684a',
  SUPERSEDES: '#f6903d',
  SOURCE_OF: '#2b5e8c',
  SIMILAR: '#2ea88a',
};

export interface VizFilters {
  kinds: Set<string>;
  statuses: Set<string>;
  tags: Set<string>;
  edgeTypes: Set<string>;
  searchHits: Set<string> | null;
  healthOverlay: boolean;
  timeTint: boolean;
  timeRange: [number, number] | null; // [fromMs, toMs] on `updated`
  semanticOverlay: boolean;
  /** Minimum similarity score for SIMILAR edges (0.5–0.95). */
  simThreshold: number;
  /** Hide similarity edges between already-linked notes. */
  simUnlinkedOnly: boolean;
}

export interface HealthMarks {
  orphans: Set<string>;
  staleIds: Set<string>;
  contradictionEdges: Set<string>;
}

export function buildGraph(payload: GraphPayload): Graph {
  const g = new Graph({ multi: true, type: 'directed' });
  for (const n of payload.nodes) {
    g.addNode(n.id, {
      label: n.label || n.id,
      kind: n.kind ?? '',
      status: n.status ?? '',
      tags: n.tags ?? [],
      dir: n.dir ?? '',
      updated: n.updated ?? null,
      updatedMs: n.updated ? Date.parse(n.updated) : null,
    });
  }
  for (const e of payload.edges) {
    if (g.hasNode(e.source) && g.hasNode(e.target)) {
      g.addEdge(e.source, e.target, { type: 'arrow', edgeType: e.type });
    }
  }
  // Degree-based size + Louvain community colors.
  louvain.assign(g, { nodeCommunityAttribute: 'community', getEdgeWeight: null });
  g.forEachNode((node) => {
    const deg = g.degree(node);
    g.setNodeAttribute(node, 'size', Math.max(2.5, Math.min(14, 2 + Math.sqrt(deg) * 1.6)));
    const community = (g.getNodeAttribute(node, 'community') as number) ?? 0;
    g.setNodeAttribute(node, 'color', PALETTE[community % PALETTE.length]);
  });
  return g;
}

/** Circlepack seed (grouped by community) then bounded ForceAtlas2. */
export function layout(g: Graph) {
  circlepack.assign(g, { hierarchyAttributes: ['community'] });
  const settings = forceAtlas2.inferSettings(g);
  forceAtlas2.assign(g, {
    iterations: g.order > 3000 ? 200 : 400,
    settings: { ...settings, barnesHutOptimize: g.order > 1500 },
  });
}

const DIM = '#22242c';
const DIM_LABEL = 'rgba(160,160,170,0.0)';

export function createSigma(
  g: Graph,
  container: HTMLElement,
  getFilters: () => VizFilters,
  getHealth: () => HealthMarks | null,
  getSelection: () => string | null,
  getHover: () => string | null,
): Sigma {
  const sigma = new Sigma(g, container, {
    renderEdgeLabels: false,
    labelColor: { color: '#c8cad4' },
    labelFont: 'ui-sans-serif, system-ui',
    labelSize: 11,
    labelRenderedSizeThreshold: 7,
    defaultEdgeColor: '#3d4252',
    minCameraRatio: 0.03,
    maxCameraRatio: 8,
    allowInvalidContainer: true,
    nodeReducer(node, data) {
      const f = getFilters();
      const health = getHealth();
      const selected = getSelection();
      const hovered = getHover();
      const res: Record<string, unknown> = { ...data };

      const passes = nodePasses(node, data, f);
      if (!passes) {
        res.color = DIM;
        res.label = '';
        res.size = Math.min(2, data.size as number);
        res.zIndex = 0;
        return res;
      }

      if (f.timeTint && data.updatedMs) {
        res.color = timeColor(data.updatedMs as number);
      }
      if (f.healthOverlay && health) {
        if (health.orphans.has(node)) {
          res.color = '#e8684a';
          res.size = (data.size as number) + 2;
        } else if (health.staleIds.has(node)) {
          res.color = '#5a5e6e';
        }
      }
      if (selected === node || hovered === node) {
        res.highlighted = true;
        res.zIndex = 2;
      }
      // Neighborhood emphasis on hover/selection.
      const focus = hovered ?? selected;
      if (focus && focus !== node) {
        const isNeighbor =
          (g.hasNode(focus) && g.areNeighbors(focus, node));
        if (!isNeighbor) {
          res.color = DIM;
          res.label = '';
          res.zIndex = 0;
        }
      }
      return res;
    },
    edgeReducer(edge, data) {
      const f = getFilters();
      const res: Record<string, unknown> = { ...data };
      const t = data.edgeType as string;
      res.color = EDGE_COLORS[t] ?? '#3d4252';
      res.size = t === 'CONTRADICTS' ? 2 : 0.6;

      const [s, tgt] = g.extremities(edge);
      if (t === 'SIMILAR') {
        const score = (data.score as number) ?? 0;
        if (
          !f.semanticOverlay ||
          score < f.simThreshold ||
          (f.simUnlinkedOnly && (data.linked as boolean)) ||
          !nodePasses(s, g.getNodeAttributes(s), f) ||
          !nodePasses(tgt, g.getNodeAttributes(tgt), f)
        ) {
          res.hidden = true;
          return res;
        }
        res.size = 1 + (score - 0.6) * 5;
        return res;
      }
      if (
        !f.edgeTypes.has(t) ||
        !nodePasses(s, g.getNodeAttributes(s), f) ||
        !nodePasses(tgt, g.getNodeAttributes(tgt), f)
      ) {
        res.hidden = true;
        return res;
      }
      const focus = getHover() ?? getSelection();
      if (focus && s !== focus && tgt !== focus) {
        res.color = '#23252e';
        res.size = 0.4;
      }
      return res;
    },
  });
  return sigma;
}

export function nodePasses(
  _node: string,
  data: Record<string, unknown>,
  f: VizFilters,
): boolean {
  if (f.kinds.size && !f.kinds.has((data.kind as string) || '(none)')) return false;
  if (f.statuses.size && !f.statuses.has((data.status as string) || '(none)')) return false;
  if (f.tags.size) {
    const tags = (data.tags as string[]) ?? [];
    if (!tags.some((t) => f.tags.has(t))) return false;
  }
  if (f.searchHits && !f.searchHits.has(_node)) return false;
  if (f.timeRange && data.updatedMs) {
    const ms = data.updatedMs as number;
    if (ms < f.timeRange[0] || ms > f.timeRange[1]) return false;
  }
  return true;
}

/** Cold (old) → warm (recent) ramp over the last 18 months. */
function timeColor(updatedMs: number): string {
  const now = Date.now();
  const span = 1000 * 60 * 60 * 24 * 548;
  const t = Math.max(0, Math.min(1, 1 - (now - updatedMs) / span));
  const cold = [70, 78, 110];
  const warm = [246, 189, 22];
  const c = cold.map((v, i) => Math.round(v + (warm[i] - v) * t));
  return `rgb(${c[0]},${c[1]},${c[2]})`;
}
