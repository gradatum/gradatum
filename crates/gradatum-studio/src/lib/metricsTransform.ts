/**
 * metricsTransform — fonctions PURES de transformation des métriques (v0.7.5 Slice 2b).
 * Dérivation taux/moyenne + regroupement catalog + alignement données de graphe.
 * Aucun effet de bord — entièrement testable isolément.
 */
import type { CatalogEntry, TimeseriesPoint } from '../types/api';

export interface LineSpec {
  label: string;
  derivation: 'rate' | 'gauge' | 'hist_avg';
  keys: string[]; // rate/gauge: [key] ; hist_avg: [sumKey, countKey]
}
export interface ChartSpec {
  title: string;
  group: string;
  unit: string;
  instrumented: boolean;
  lines: LineSpec[];
}
export interface ChartData {
  xs: number[]; // ts_ms triés (union)
  lines: { label: string; values: (number | null)[] }[];
}

/** Taux par minute d'un compteur cumulé monotone. 1er point omis. Reset (Δ<0) → 0. */
export function deriveRatePerMin(points: TimeseriesPoint[]): TimeseriesPoint[] {
  const out: TimeseriesPoint[] = [];
  for (let i = 1; i < points.length; i++) {
    const dv = points[i].value - points[i - 1].value;
    const dtMin = (points[i].ts_ms - points[i - 1].ts_ms) / 60_000;
    const rate = dv < 0 || dtMin <= 0 ? 0 : dv / dtMin;
    out.push({ ts_ms: points[i].ts_ms, value: rate });
  }
  return out;
}

/** Moyenne d'intervalle Δsum/Δcount aux ts alignés. Δcount==0 → point omis. */
export function deriveHistogramAvg(
  sum: TimeseriesPoint[],
  count: TimeseriesPoint[],
): TimeseriesPoint[] {
  const countByTs = new Map(count.map(p => [p.ts_ms, p.value]));
  const out: TimeseriesPoint[] = [];
  for (let i = 1; i < sum.length; i++) {
    const ts = sum[i].ts_ms;
    const prevTs = sum[i - 1].ts_ms;
    const c = countByTs.get(ts);
    const cPrev = countByTs.get(prevTs);
    if (c === undefined || cPrev === undefined) continue;
    const dCount = c - cPrev;
    if (dCount <= 0) continue;
    out.push({ ts_ms: ts, value: (sum[i].value - sum[i - 1].value) / dCount });
  }
  return out;
}

/**
 * Regroupe le catalog en graphes par FAMILLE (préfixe) et par GROUPE.
 * Famille counter/gauge → 1 ChartSpec, 1 LineSpec/clé (label = suffixe).
 * Paires histogramme `<fam>_sum.<lbl>` + `<fam>_count.<lbl>` → 1 LineSpec hist_avg.
 *
 * CORRECTION P1-A (Auditeur 2026-06-29) :
 * stripSuffix ET labelOf doivent retirer le token _sum/_count AVANT le lastIndexOf('.')
 * pour que les histogrammes label-less (http.duration_sum/_count) appairent correctement.
 */
export function groupCatalog(entries: CatalogEntry[]): ChartSpec[] {
  // Retire le token _sum/_count (suivi d'un '.' ou fin de chaîne) → clé de base.
  const stripSuffix = (key: string): string => key.replace(/_(sum|count)(?=\.|$)/, '');
  const familyOf = (key: string): string => {
    const s = stripSuffix(key);
    const dot = s.lastIndexOf('.');
    return dot === -1 ? s : s.slice(0, dot);
  };
  const labelOf = (key: string): string => {
    const s = stripSuffix(key);
    const dot = s.lastIndexOf('.');
    const lbl = dot === -1 ? s : s.slice(dot + 1);
    return lbl || 'avg';
  };

  // Bucket entries par (group, family).
  const buckets = new Map<string, { group: string; unit: string; instrumented: boolean; entries: CatalogEntry[] }>();
  for (const e of entries) {
    const fam = familyOf(e.key);
    const bk = `${e.group}::${fam}`;
    if (!buckets.has(bk)) buckets.set(bk, { group: e.group, unit: e.unit, instrumented: e.instrumented, entries: [] });
    const b = buckets.get(bk)!;
    b.entries.push(e);
    b.instrumented = b.instrumented && e.instrumented;
  }

  const specs: ChartSpec[] = [];
  for (const [bk, b] of buckets) {
    const fam = bk.split('::')[1];
    const lines: LineSpec[] = [];
    const sums = b.entries.filter(e => e.kind === 'histogram_sum');
    const counts = b.entries.filter(e => e.kind === 'histogram_count');
    const others = b.entries.filter(e => e.kind === 'counter' || e.kind === 'gauge');
    // Apparier sum/count par label commun.
    for (const s of sums) {
      const lbl = labelOf(s.key);
      const c = counts.find(cc => labelOf(cc.key) === lbl);
      if (c) lines.push({ label: lbl, derivation: 'hist_avg', keys: [s.key, c.key] });
    }
    for (const o of others) {
      lines.push({
        label: labelOf(o.key) || o.key,
        derivation: o.kind === 'gauge' ? 'gauge' : 'rate',
        keys: [o.key],
      });
    }
    if (lines.length === 0) continue;
    specs.push({ title: fam, group: b.group, unit: b.unit, instrumented: b.instrumented, lines });
  }
  // ordre stable : par groupe puis titre
  return specs.sort((a, b) => a.group.localeCompare(b.group) || a.title.localeCompare(b.title));
}

/** Applique les dérivations et aligne les lignes sur l'union triée des ts. */
export function buildChartData(
  spec: ChartSpec,
  seriesByKey: Map<string, TimeseriesPoint[]>,
): ChartData {
  const derived = spec.lines.map(line => {
    if (line.derivation === 'rate') return deriveRatePerMin(seriesByKey.get(line.keys[0]) ?? []);
    if (line.derivation === 'gauge') return seriesByKey.get(line.keys[0]) ?? [];
    // hist_avg
    return deriveHistogramAvg(seriesByKey.get(line.keys[0]) ?? [], seriesByKey.get(line.keys[1]) ?? []);
  });
  const tsSet = new Set<number>();
  for (const d of derived) for (const p of d) tsSet.add(p.ts_ms);
  const xs = [...tsSet].sort((a, b) => a - b);
  const lines = spec.lines.map((line, i) => {
    const byTs = new Map(derived[i].map(p => [p.ts_ms, p.value]));
    return { label: line.label, values: xs.map(ts => (byTs.has(ts) ? byTs.get(ts)! : null)) };
  });
  return { xs, lines };
}
