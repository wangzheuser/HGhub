export type PlaybackQuality = { width: number; height: number; codec: string };
export function qualityLabel({ width, height }: PlaybackQuality): string {
  const short = Math.min(width, height);
  return short > 0 ? `${short}p` : "标准/自动";
}
export function codecLabel(codec: string): string {
  if (/hvc1|hev1|hevc/i.test(codec)) return "HEVC";
  if (/avc1|avc3|h264/i.test(codec)) return "AVC";
  if (/av01/i.test(codec)) return "AV1";
  if (/vp09|vp9/i.test(codec)) return "VP9";
  return "";
}
const capabilities = new Map<string, Promise<MediaCapabilitiesDecodingInfo | null>>();
/** 探测仅用于观测，MSE gate 仍是入口；缓存异步结果，不阻塞首播。 */
export function decodingCapability(quality: PlaybackQuality): Promise<MediaCapabilitiesDecodingInfo | null> {
  const { codec, width, height } = quality;
  if (!codec || !width || !height || !navigator.mediaCapabilities?.decodingInfo) return Promise.resolve(null);
  const key = `${codec}:${width}:${height}`;
  let promise = capabilities.get(key);
  if (!promise) {
    promise = navigator.mediaCapabilities.decodingInfo({ type: "media-source", video: {
      contentType: `video/mp4; codecs="${codec}"`, width, height, bitrate: 4_000_000, framerate: 30,
    } }).catch(() => null);
    capabilities.set(key, promise);
    if (capabilities.size > 16) capabilities.delete(capabilities.keys().next().value!);
  }
  return promise;
}
/** 网络与解码错误分离；只有明确编解码/媒体错误触发 HEVC 回退。 */
export function isCodecFailure(message: string): boolean {
  return !/networkError|fragLoad|manifestLoad|levelLoad|keyLoad|timeout|HTTP|连接|网络/i.test(message) &&
    /bufferAddCodecError|manifestIncompatibleCodecsError|mediaError|decode|解码|不支持.*格式/i.test(message);
}
