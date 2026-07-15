export function process(input: string): string {
  const trimmed = input.trim();
  const parts = trimmed.split(":");
  if (parts.length > 1) {
    return parts[0];
  }
  return trimmed;
}
