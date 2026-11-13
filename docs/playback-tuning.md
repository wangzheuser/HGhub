# 播放缓冲调优记录

面向目标：视频播放缓冲足够快——初始播放、拖动进度条后、切换集数后都应在短时间内出画（拖动目标 <1s）。本文保留 2026-11-12 特定网络下的历史测量，不将其当作所有 CDN、片源与时段的定律。2026-11-13 的当前实现、真实失败、让槽/尾救援和恢复边界以[分批实施与真实验证](2026-11-13_分批加载优化实施与真实验证.md)为准。

## 实测基线（2026-11-12，本机网络）

用 `live_segment_download_benchmark` / `live_proxy_chain_benchmark`（`#[ignore]` 联网基准，`cargo test --lib <name> -- --ignored --nocapture` 复跑）测量：

| 路径 | 实测 |
| --- | --- |
| 单连接整包下载 2.9MB 分片 | 6.3–7.8s |
| 4 路分段 | 5.6–9.0s |
| 8 路分段（生产档） | 2.4–11s（典型 4s，波动来自 CDN） |
| 12 路分段 | 2.5–7s，**无稳定收益** |
| 温热连接池（8 路） | 比冷连接省 ~1.3s |
| 播放列表代理 | ~1.8s |
| 内存缓存命中 | 毫秒级 |
| 磁盘缓存命中 | 读盘 2.1MB <10ms |

CDN 行为要点：

- tp\*.drleao.cn / yd-lphu-\*.hzjvlo.cn / pic.drleao.cn / thu.adihsppe.cc **在该次采样中仅协商到 HTTP/1.1**（curl 带 h2 ALPN），当时没有测出 HTTP/2 收益；后续节点或协议变化应另取证。
- **风控对频率与并发敏感且阈值漂移**：同一 URL 连续下载必触发 403；同一主机密集连测时 6 次请求出现过 2 次 403。调基准或测试时每个分片只拉一次、阶段间留间隔，不要压测。
- 单连接吞吐受 hyper 小块读取 × RTT 限制（~23KB/RTT ≈ 0.35MB/s），该组测量中并发与预取收益明显；仅凭它不排除首探重叠、连接复用及交付时机的收益。后续本地渐进交付原型已给出另一条值得继续验证的路径。

## 现有机制（按请求路径）

1. **内存缓存**：分片 LRU 80 条（~170MB）、Key 120 条、播放列表 60 份、封面 240 张。**缓存键归一化**（`cache_key()`）：①剥离 `auth_key` 签名——实测上游 m3u8 内分片签名按小时级轮换；②**归一 `tp<N>` 轮换子域**——实测同一分片路径不同时间从 tp1/tp3/tp6 等不同 CDN 池边缘下发（Feed 划动实测 6025ms/6141ms/3378ms 复取的根因）。不归一则重看/跨启动时 SEG_CACHE 与磁盘缓存全 miss；此归一仅适用已实证内容稳定的签名与指定轮换域；保留其他查询、路径、端口和主机差异。上游语义变化时需复核内容一致性并调整规则，不先假定只影响命中率。原始清单仍以完整 URL 做 singleflight 与 TTL 缓存。网络下载仍用完整签名 URL。
2. **磁盘缓存**：500MB（`segment-cache/`），按 mtime LRU 淘汰；跨启动生效，回看/重看直接读盘。注意：mtime 触碰用 `File::set_modified` 显式更新——「append 打开再关闭」在 Windows 上不更新 LastWriteTime（实测），曾导致 LRU 触碰失效、越常看的分片越先被淘汰。
3. **首片预热**（`warmup_head`）：**串行接力**结构——首片独占 8 路带宽（实测用户管道总带宽 ~5Mbps 是硬顶，两片并行会让首帧平分带宽拖到 7s+，串行后首片 ~3.4s 就绪），第二片在首片完成后接力 8 路（只需在首片播完 2–6s 前就绪），**AES 密钥**与首片并行整包预热（省解密前的一次往返）；被划走/未播放的视频由前台活跃门槛拦截链推进。
4. **自适应预取**（`schedule_prefetch`）：按最近下载耗时动态领先 3/5/10 片；换代机制保证远跳后旧队列作废；前台下载时让路；**30s 前台活跃门槛**——播放器取片窗口过期后链停止推进（真实日志反馈：Feed 划动时每个被划走的视频曾白拉 5–10 片 ≈ 10–20MB，直接挤占下一个视频的首片带宽）。
5. **受控分段并发**（`fetch_bytes_ranged`）：默认 8 路，启动前可用 `HGHUB_MEDIA_DOWNLOAD_PARTS=1|2|4|8` 选档（1 为单整包 GET），未开启自动选优。首探正文与后续区间重叠；仅剩一个异常慢区间时可消耗唯一恢复预算救援该尾块，保留完成字节，救援后禁止再整包回退。20 秒只指原分段相位，普通旧回退可另占一个相位。
6. **拖动/点击推测预取**（`speculative_prefetch`，media_prefetch 命令）：前端在拖动预览/点击按下时把目标分片交给 Rust 提前下载（120ms 去抖、单槽 abort 换目标）；松手后前台请求经分片锁复用同一次下载。未缓存分片在慢网络（8 路 >1s）时这是「拖动 1 秒内出画」的主要手段。
7. **意图驱动预热**：单候选按方向、切集模式与显式选集意图选择；先地址与轻清单，再按 8/4 秒墙钟余量迟滞申请重任务。`accepted/queued` 与 `ready` 分开，首片/key/init 全就绪才称 ready；仅当前标准 MSE 已出帧且有 20 秒连续余量时可短借缓冲填充权，8 秒退出、单次最多 12 秒。前台带宽不足时允许有界放弃，不保证所有源都能预热完成。
8. **连接池保活**（net.rs `ClientEntry`）：客户端长存 + 30s 后台 DNS 复检——答案一致保留连接池（省 8 次 TLS 握手+慢启动），答案变化重建换固定地址，复检失败保留旧客户端重试。安全语义：客户端只使用构建/复检时通过策略校验的地址。
9. **观测**：`playback.log` 的 `seg.serve`（source=mem/disk/network/key_mem）量化缓存命中率；`warmup.ok/fail`、`prefetch.speculative`、`prefetch.ok/fail`、`proxy.begin/end`、`network.begin/end` 覆盖全链路耗时。**设置 → 本地数据 → 播放缓存**展示实时快照（`cache_stats` 命令：内存片数/磁盘片数与体积/上限）。

## 已否决的方向（勿重复尝试）

- **HTTP/2**：CDN 不支持。
- **12+ 路并发**：既有测量收益不稳定，本轮保持默认 8 路，仅允许显式小规模 1/2/4/8 档比较，不将最大并发当自动最优。
- **同 URL 反复下载式基准**：触发 403，数据失真。
- **盲调 reqwest/参数**：该次瓶颈主要在 RTT 与读取模式，优先要求分阶段指标和可比较样本，而非先排除所有参数或后端改动。

## 验证入口

- 无头播放回归：`pnpm test:playback --browser-executable "<Chrome路径>"`（7 场景 seek/legacy/stall/lifecycle/mms/optimization/mobile；本机未装 Playwright 浏览器时必须传 Chrome 路径）。
- 联网基准：`cargo test --lib live_segment_download_benchmark -- --ignored --nocapture`；`cargo test --lib live_proxy_chain_benchmark -- --ignored --nocapture`（谨慎使用：会请求真实 CDN，勿连续压测）。
- 连接池保活联网验证：`cargo test --lib live_client_pool_survives_revalidation -- --ignored --nocapture`。
- 分片签名稳定性验证：`cargo test --lib live_playlist_signature_stability -- --ignored --nocapture`（只拉两次 m3u8，验证缓存键归一化假设是否仍成立）。
- 离线回归：`cargo test --lib`（本轮 91 通过、11 项联网用例保留忽略；前端另覆盖 lease/恢复/队列等纯模块断言）。
- **真机端到端**：`pnpm app:dev` 起真实实例后，经桥 `POST /api/api_play` → `GET /hls?u=` → `GET /seg?u=` 复现播放链路；诊断开关可用 `POST /api/playback_log_set {"enabled":true}` 经桥打开（**结束时记得关回**）。真机实测：预热后首片 43–59ms 命中（vs 冷 4–9s）。
- 已知干扰：App 的 Feed 页面在窗口打开时会自动播放，其分片下载会污染单视频测量并挤占 LRU；测量前先把窗口停在非播放页。当前调度使用意图单候选、有界状态轮询与精确取消；应关联当前页面/会话/源代际测量，避免将隐藏首页或不同方向候选当成当前目标。
