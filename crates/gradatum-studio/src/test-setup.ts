import '@testing-library/jest-dom';

// Polyfill ResizeObserver (absent de jsdom) — requis par MetricChart (uPlot resize).
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
(globalThis as unknown as { ResizeObserver: unknown }).ResizeObserver =
  (globalThis as unknown as { ResizeObserver?: unknown }).ResizeObserver ?? ResizeObserverStub;

// Polyfill window.matchMedia (absent de jsdom) — requis par uPlot à l'initialisation.
// uPlot appelle matchMedia({monochrome}) pour déterminer le mode HiDPI (setPxRatio).
if (!window.matchMedia) {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    configurable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    }),
  });
}
