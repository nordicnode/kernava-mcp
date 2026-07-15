// Chain fixture: c.ts is the leaf, called by b.ts, calls nothing
export function step_c(x: number): number {
  return x * x;
}
