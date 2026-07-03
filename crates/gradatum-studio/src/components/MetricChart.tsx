import { useEffect, useRef } from 'react';
import uPlot from 'uplot';
import 'uplot/dist/uPlot.min.css';
import type { ChartData, ChartSpec } from '../lib/metricsTransform';

// Palette dérivée des tokens (cohérence design system).
const STROKES = ['#2563eb', '#15803d', '#b54708', '#7c3aed', '#b42318', '#0891b2', '#65a30d', '#be185d'];

interface Props { spec: ChartSpec; data: ChartData; bucketSecs: number; }

export function MetricChart({ spec, data }: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const plotRef = useRef<uPlot | null>(null);

  // uPlot data : [xs_seconds, ...lineValues]
  const toPlotData = (d: ChartData): uPlot.AlignedData =>
    [d.xs.map(ms => ms / 1000), ...d.lines.map(l => l.values)] as uPlot.AlignedData;

  useEffect(() => {
    if (!containerRef.current) return;
    const opts: uPlot.Options = {
      width: containerRef.current.clientWidth || 600,
      height: 180,
      title: spec.title,
      series: [
        {},
        ...spec.lines.map((l, i) => ({ label: l.label, stroke: STROKES[i % STROKES.length], width: 1.5 })),
      ],
      axes: [{}, { label: spec.unit }],
      scales: { x: { time: true } },
    };
    const plot = new uPlot(opts, toPlotData(data), containerRef.current);
    plotRef.current = plot;
    const ro = new ResizeObserver(() => {
      if (plotRef.current && containerRef.current) {
        plotRef.current.setSize({ width: containerRef.current.clientWidth || 600, height: 180 });
      }
    });
    ro.observe(containerRef.current);
    return () => { ro.disconnect(); plot.destroy(); plotRef.current = null; };
    // recreate on series shape change; data-only updates handled below
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [spec.title, spec.lines.length]);

  // Data updates without full recreate.
  useEffect(() => {
    if (plotRef.current) plotRef.current.setData(toPlotData(data));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data]);

  return (
    <div className="metric-chart" style={spec.instrumented ? undefined : { opacity: 0.5 }}>
      {!spec.instrumented && (
        <span className="metric-chart-stub" style={{ fontSize: '0.75rem', color: 'var(--color-text-muted)' }}>
          non instrumenté
        </span>
      )}
      <div ref={containerRef} />
    </div>
  );
}
