/** 与 Rust media::cache_key 对齐；仅用于 warmup 取消/当前目标交接保护，不用于会话 cleanup。 */
export function canonicalWarmupTarget(raw: string): string {
  let url: URL;
  try { url = new URL(raw); } catch { return raw; }
  const host = /^tp[0-9]+\.drleao\.cn$/.test(url.hostname) ? "tp.drleao.cn" : url.hostname;
  const port = url.port ? `:${url.port}` : "";
  const base = `${url.protocol}//${host}${port}${url.pathname}`;
  // 原样保留 query 的值、编码与顺序；只过滤字面名称 auth_key 和空名称。
  // URLSearchParams 会解码名称并重编码值，和后端 cache_key 不同。
  const kept = url.search.slice(1).split("&").filter(part => {
    const name = part.split("=", 1)[0];
    return name !== "auth_key" && name !== "";
  });
  return kept.length ? `${base}?${kept.join("&")}` : base;
}
