/**
 * Model picker search. Every whitespace-separated word must occur in the id (case-insensitive).
 * Ids starting with the first word, directly or after a `provider/` prefix, come first; both
 * groups keep the given order.
 */
export function filterModels(models: readonly string[], query: string): string[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return [...models];
  const first: string[] = [];
  const rest: string[] = [];
  for (const m of models) {
    const id = m.toLowerCase();
    if (!words.every((w) => id.includes(w))) continue;
    const tail = id.slice(id.lastIndexOf('/') + 1);
    (id.startsWith(words[0]) || tail.startsWith(words[0]) ? first : rest).push(m);
  }
  return [...first, ...rest];
}

/** Picker order: the default model first, then a custom current value, then the host's list. */
export function pickerModels(models: readonly string[], defaultModel?: string, current?: string): string[] {
  const head = [defaultModel, current && !models.includes(current) ? current : undefined].filter((m): m is string => !!m);
  return [...new Set([...head, ...models])];
}
