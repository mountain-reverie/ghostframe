export interface IdleSkipState {
  rawQueueLen: number;
  solidQueueLen: number;
  palRleQueueLen: number;
  cdf53QueueLen: number;
  h264QueueLen: number;
  framebufferDirty: boolean;
  errorsReadbackInFlight: boolean;
}

export function shouldSkipEncodePresent(s: IdleSkipState): boolean {
  if (s.framebufferDirty) return false;
  if (s.errorsReadbackInFlight) return false;
  return (
    s.rawQueueLen === 0
    && s.solidQueueLen === 0
    && s.palRleQueueLen === 0
    && s.cdf53QueueLen === 0
    && s.h264QueueLen === 0
  );
}
