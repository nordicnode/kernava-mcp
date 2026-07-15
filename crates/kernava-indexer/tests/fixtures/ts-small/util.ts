export function helper(value: string): string {
  const trimmed = value.trim();
  const upper = trimmed.toUpperCase();
  if (upper.length > 10) {
    return upper.substring(0, 10);
  }
  return upper;
}

// Dead code: never called, not exported
function dead_function(x: number): number {
  return x * 42;
}
