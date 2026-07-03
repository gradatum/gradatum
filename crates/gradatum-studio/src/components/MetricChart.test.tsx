import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render } from '@testing-library/react';
import { MetricChart } from './MetricChart';
import type { ChartSpec, ChartData } from '../lib/metricsTransform';

const destroy = vi.fn();
const setData = vi.fn();
const setSize = vi.fn();
const ctor = vi.fn();
vi.mock('uplot', () => ({
  default: class {
    constructor(opts: unknown, data: unknown, el: unknown) { ctor(opts, data, el); }
    destroy = destroy;
    setData = setData;
    setSize = setSize;
  },
}));

const spec: ChartSpec = { title: 't', group: 'usage', unit: 'calls', instrumented: true, lines: [{ label: 'a', derivation: 'rate', keys: ['k.a'] }] };
const data: ChartData = { xs: [60_000, 120_000], lines: [{ label: 'a', values: [6, 6] }] };

beforeEach(() => { ctor.mockReset(); destroy.mockReset(); });

describe('MetricChart', () => {
  it('creates a uPlot instance with x-axis in seconds', () => {
    render(<MetricChart spec={spec} data={data} bucketSecs={60} />);
    expect(ctor).toHaveBeenCalledTimes(1);
    const passedData = ctor.mock.calls[0][1] as number[][];
    expect(passedData[0]).toEqual([60, 120]); // ms → s
  });
  it('destroys the instance on unmount', () => {
    const { unmount } = render(<MetricChart spec={spec} data={data} bucketSecs={60} />);
    unmount();
    expect(destroy).toHaveBeenCalledTimes(1);
  });
  it('renders a "non instrumenté" badge when not instrumented', () => {
    const { getByText } = render(<MetricChart spec={{ ...spec, instrumented: false }} data={data} bucketSecs={60} />);
    expect(getByText(/non instrumenté/i)).toBeTruthy();
  });
});
