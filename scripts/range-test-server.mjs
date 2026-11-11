// 最小 Range 测试服务器：验证桥的分段并发下载逻辑（B 方案）。
// 用法：node scripts/range-test-server.mjs
// 输出：监听端口与每个请求的 Range 头，便于核对是否真的并发分段。
import { createServer } from "node:http";
import { createHash } from "node:crypto";

const PORT = Number(process.env.RANGE_PORT || 18080);
const SIZE = Number(process.env.RANGE_SIZE || 2 * 1024 * 1024);
// 确定性内容：每字节 = i % 251，便于校验拼接是否正确
const body = Buffer.alloc(SIZE);
for (let i = 0; i < SIZE; i += 1) body[i] = i % 251;
const expect = createHash("sha256").update(body).digest("hex");

const requests = [];
const server = createServer((req, res) => {
  const range = req.headers.range;
  requests.push({ url: req.url, range: range || null, at: Date.now() });
  console.log(`[req] ${req.method} ${req.url} range=${range || "-"}`);

  if (req.url === "/__stats") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ count: requests.length, requests, sha256: expect, size: SIZE }));
    return;
  }
  if (req.url === "/__reset") {
    requests.length = 0;
    res.writeHead(200, { "content-type": "text/plain" });
    res.end("ok");
    return;
  }

  const headers = {
    "content-type": "video/mp2t",
    "accept-ranges": "bytes",
  };
  const match = /^bytes=(\d+)-(\d*)$/.exec(range || "");
  if (match) {
    const start = Number(match[1]);
    const end = match[2] === "" ? SIZE - 1 : Math.min(Number(match[2]), SIZE - 1);
    if (start >= SIZE || start > end) {
      res.writeHead(416, { "content-range": `bytes */${SIZE}` });
      res.end();
      return;
    }
    const slice = body.subarray(start, end + 1);
    res.writeHead(206, {
      ...headers,
      "content-range": `bytes ${start}-${end}/${SIZE}`,
      "content-length": String(slice.length),
    });
    res.end(slice);
    return;
  }
  res.writeHead(200, { ...headers, "content-length": String(SIZE) });
  res.end(body);
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`range-test-server listening on http://127.0.0.1:${PORT}`);
  console.log(`  segment url: http://127.0.0.1:${PORT}/test.ts`);
  console.log(`  expected sha256: ${expect}`);
  console.log(`  size: ${SIZE}`);
});
