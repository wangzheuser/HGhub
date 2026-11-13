/** 与Rust清洗白名单一一对应；未知原因不保留任意文本。 */
const CAUSES: Readonly<Record<string, number>> = Object.freeze({
  cache_evicted: 1, session_changed: 2, session_inactive: 3, session_unavailable: 4, retryable: 5,
  requested: 6, foreground: 7, background_busy: 8, cache_not_retained: 9, cache_cleared: 10,
  unsupported_audio_rendition: 11, unsupported_byte_range: 12, unsupported_encryption: 13,
  missing_key_uri: 14, missing_init_uri: 15, no_media_segment: 16, playlist_depth: 17,
  playlist_expired: 18, invalid_playlist: 19, http_auth: 20, http: 21, timeout: 22, network: 23,
  resolve_failed: 101, prepare_failed: 102, admission_budget: 103, media_budget: 104,
  playlist_refresh_budget: 105, playlist_refresh_failed: 106, failure_budget: 107,
  prepare_error: 108, rpc_error: 109,
  exhausted: 201, ready: 202, unsupported: 203, candidate: 204, inactive: 205, stop: 206,
  low_buffer: 207, waiting: 208, seek: 209, seek_preview: 210, external_pause: 211,
  external_resume: 212, released: 213, not_paused: 214, acquire_failed: 215,
});
export function playbackCause(reason: unknown): number | undefined {
  return typeof reason === "string" && Object.prototype.hasOwnProperty.call(CAUSES, reason) ? CAUSES[reason] : undefined;
}
export function diagnosticCause(data: Record<string, unknown>): Record<string, unknown> {
  const { reason, ...rest } = data;
  const code = playbackCause(reason);
  return code === undefined ? rest : { ...rest, cause_code: code };
}
