/** hls.js在MMS/WKWebView可用<source>而非video.src；两种绑定都要识别。 */
export function mediaElementSource(video: HTMLMediaElement): string {
  return video.getAttribute("src") ? video.src : video.querySelector("source")?.src || video.src;
}
export function matchesMediaElementSource(video: HTMLMediaElement, resource: string): boolean {
  return !!resource && mediaElementSource(video) === resource && (!video.currentSrc || video.currentSrc === resource);
}
