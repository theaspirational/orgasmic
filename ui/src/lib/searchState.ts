export type AppSearch = Record<string, unknown>;

export function searchList(value: unknown): string[] {
  if (Array.isArray(value)) return value.map((item) => String(item).trim()).filter(Boolean);
  if (typeof value === 'string') return value.split(',').map((item) => item.trim()).filter(Boolean);
  return [];
}

export function withDrawerStack(search: AppSearch, stack: string[]): AppSearch {
  return {
    ...search,
    drawer_stack: stack.length > 0 ? stack : undefined,
  };
}

export function appendDrawerStack(search: AppSearch, id: string): AppSearch {
  return withDrawerStack(search, [...searchList(search.drawer_stack), id]);
}

export function routeSearch(reducer: (search: AppSearch) => AppSearch): never {
  return reducer as never;
}
