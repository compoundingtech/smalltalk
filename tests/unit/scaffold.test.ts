import { describe, it, expect } from 'vitest';
import { VERSION } from '../../src/index.ts';

describe('scaffold', () => {
  it('exports the current package VERSION', () => {
    expect(VERSION).toBe('0.8.0');
  });
});
