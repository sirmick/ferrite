// Shared fetch-error type for the REST helpers. Surfaces the HTTP
// status separately from the message so callers can branch on it
// (e.g. 404 during teardown races, 400 for reconfigure-failed).

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

/** Build a same-origin `ws://` or `wss://` URL from a path. */
export function wsUrlFor(path: string): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}${path}`;
}
