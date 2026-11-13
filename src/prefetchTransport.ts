/** 按后端资源身份串行化命令ACK；跨播放器重挂也不能让旧cancel晚于新同片prefetch。 */
export function createPrefetchTransport(prefetch: (url: string) => Promise<boolean>, cancel: (url: string) => Promise<void>, resourceKey: (url: string) => string) {
  const tails = new Map<string, Promise<unknown>>();
  const enqueue = <T>(url: string, operation: () => Promise<T>): Promise<T> => {
    const key = resourceKey(url), previous = tails.get(key);
    const current = (previous ? previous.catch(() => {}) : Promise.resolve()).then(operation);
    tails.set(key, current);
    const cleanup = () => { if (tails.get(key) === current) tails.delete(key); };
    void current.then(cleanup, cleanup);
    return current;
  };
  return { prefetch: (url: string) => enqueue(url, () => prefetch(url)), cancel: (url: string) => enqueue(url, () => cancel(url)) };
}
