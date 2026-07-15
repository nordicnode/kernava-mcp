// Chain fixture: b.ts is the middle hop, called by a.ts, calls c.ts
import { step_c } from './c';

export function step_b(x: number): number {
  return step_c(x) * 2;
}
