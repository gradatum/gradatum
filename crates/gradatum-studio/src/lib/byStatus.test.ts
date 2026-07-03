import { describe, it, expect } from 'vitest';
import { parseByStatusResponse } from './byStatus';

describe('parseByStatusResponse', () => {
  it('mappe les entries vers SearchHit', () => {
    const data = {
      entries: [
        { ulid: '01ABC', section: 'decisions', title: 'T1', status: 'downgraded', snippet: 's1', modified_at: '2026-06-30T00:00:00Z' },
      ],
      next_cursor: null,
      total: 1,
    };
    const hits = parseByStatusResponse(data);
    expect(hits).toHaveLength(1);
    expect(hits[0].ulid).toBe('01ABC');
    expect(hits[0].section).toBe('decisions');
    expect(hits[0].status).toBe('downgraded');
    expect(hits[0].path).toBe('decisions/01ABC');
  });

  it('défensif : data invalide → []', () => {
    expect(parseByStatusResponse(null)).toEqual([]);
    expect(parseByStatusResponse({})).toEqual([]);
    expect(parseByStatusResponse({ entries: 'x' })).toEqual([]);
  });
});
