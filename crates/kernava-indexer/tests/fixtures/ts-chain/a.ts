// Chain fixture: a.ts is the entry point, calls into b.ts
import { step_b } from './b';

export function step_a(x: number): number {
  return step_b(x) + 1;
}
