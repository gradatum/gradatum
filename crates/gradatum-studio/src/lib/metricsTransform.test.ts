import { describe, it, expect } from 'vitest';
import {
  deriveRatePerMin,
  deriveHistogramAvg,
  groupCatalog,
  buildChartData,
  type ChartSpec,
} from './metricsTransform';
import type { CatalogEntry, TimeseriesPoint } from '../types/api';

const pt = (ts_ms: number, value: number): TimeseriesPoint => ({ ts_ms, value });

describe('deriveRatePerMin', () => {
  it('derives per-minute delta, omitting first point', () => {
    // cumulative 10,20,50 at 60s spacing → rate 10/min then 30/min
    const out = deriveRatePerMin([pt(0, 10), pt(60_000, 20), pt(120_000, 50)]);
    expect(out).toEqual([pt(60_000, 10), pt(120_000, 30)]);
  });
  it('clamps to 0 on counter reset (value drops)', () => {
    const out = deriveRatePerMin([pt(0, 50), pt(60_000, 5)]);
    expect(out).toEqual([pt(60_000, 0)]);
  });
  it('returns [] for <2 points', () => {
    expect(deriveRatePerMin([pt(0, 1)])).toEqual([]);
    expect(deriveRatePerMin([])).toEqual([]);
  });
});

describe('deriveHistogramAvg', () => {
  it('computes interval average dsum/dcount at aligned ts', () => {
    // sum 0,1.5 ; count 0,30 → avg (1.5-0)/(30-0)=0.05 at ts 60_000
    const out = deriveHistogramAvg([pt(0, 0), pt(60_000, 1.5)], [pt(0, 0), pt(60_000, 30)]);
    expect(out.length).toBe(1);
    expect(out[0].ts_ms).toBe(60_000);
    expect(out[0].value).toBeCloseTo(0.05, 9);
  });
  it('omits points where dcount == 0', () => {
    const out = deriveHistogramAvg([pt(0, 0), pt(60_000, 1.5)], [pt(0, 30), pt(60_000, 30)]);
    expect(out).toEqual([]);
  });
});

describe('groupCatalog', () => {
  it('groups a counter family into one multi-line rate chart', () => {
    const cat: CatalogEntry[] = [
      { key: 'mcp_tool_calls.a', group: 'usage', kind: 'counter', unit: 'calls', instrumented: true },
      { key: 'mcp_tool_calls.b', group: 'usage', kind: 'counter', unit: 'calls', instrumented: true },
    ];
    const specs = groupCatalog(cat);
    const mcp = specs.find(s => s.title.includes('mcp_tool_calls'));
    expect(mcp).toBeDefined();
    expect(mcp!.group).toBe('usage');
    expect(mcp!.lines.map(l => l.derivation)).toEqual(['rate', 'rate']);
    expect(mcp!.lines.map(l => l.label).sort()).toEqual(['a', 'b']);
  });
  it('pairs histogram _sum/_count into one hist_avg line', () => {
    const cat: CatalogEntry[] = [
      { key: 'vault_context.duration_sum.assembled', group: 'context', kind: 'histogram_sum', unit: 'seconds', instrumented: true },
      { key: 'vault_context.duration_count.assembled', group: 'context', kind: 'histogram_count', unit: 'count', instrumented: true },
    ];
    const specs = groupCatalog(cat);
    const hist = specs.find(s => s.lines.some(l => l.derivation === 'hist_avg'));
    expect(hist).toBeDefined();
    const line = hist!.lines.find(l => l.derivation === 'hist_avg')!;
    expect(line.keys).toEqual([
      'vault_context.duration_sum.assembled',
      'vault_context.duration_count.assembled',
    ]);
  });
  it('marks a gauge as gauge derivation', () => {
    const cat: CatalogEntry[] = [
      { key: 'event_log.rows', group: 'server', kind: 'gauge', unit: 'rows', instrumented: true },
    ];
    const specs = groupCatalog(cat);
    expect(specs[0].lines[0].derivation).toBe('gauge');
  });
  it('pairs a LABEL-LESS histogram (http.duration_sum/_count) into one hist_avg line', () => {
    const cat: CatalogEntry[] = [
      { key: 'http.duration_sum', group: 'server', kind: 'histogram_sum', unit: 'seconds', instrumented: true },
      { key: 'http.duration_count', group: 'server', kind: 'histogram_count', unit: 'count', instrumented: true },
    ];
    const specs = groupCatalog(cat);
    const histLines = specs.flatMap(s => s.lines).filter(l => l.derivation === 'hist_avg');
    expect(histLines.length).toBe(1);
    expect(histLines[0].keys).toEqual(['http.duration_sum', 'http.duration_count']);
  });
});

describe('buildChartData', () => {
  it('aligns derived lines on the union of timestamps, null-filling gaps', () => {
    const spec: ChartSpec = {
      title: 'x', group: 'usage', unit: 'calls', instrumented: true,
      lines: [
        { label: 'a', derivation: 'rate', keys: ['k.a'] },
        { label: 'b', derivation: 'gauge', keys: ['k.b'] },
      ],
    };
    const byKey = new Map<string, TimeseriesPoint[]>([
      ['k.a', [pt(0, 0), pt(60_000, 6), pt(120_000, 12)]], // rate → [(60k,6),(120k,6)]
      ['k.b', [pt(120_000, 99)]],                          // gauge → [(120k,99)]
    ]);
    const cd = buildChartData(spec, byKey);
    expect(cd.xs).toEqual([60_000, 120_000]);
    expect(cd.lines[0].values).toEqual([6, 6]);       // rate line a
    expect(cd.lines[1].values).toEqual([null, 99]);   // gauge line b, null at 60k
  });
});
