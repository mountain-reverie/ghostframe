// Full-frame H.264 decode via WebCodecs. All wire parsing lives in the wasm
// core; this is only the browser-side VideoDecoder wrapper.

export class FullFrameDecoder {
  private decoder!: VideoDecoder;
  private latestFrame: VideoFrame | null = null;
  /** Set by `close()`: the owner is done, never rebuild. */
  private disposed = false;
  /**
   * An async decode error closes the WebCodecs decoder for good, and the delta
   * chunks that follow cannot be decoded without the reference they lost. Until
   * the next key chunk, drop deltas; on it, build a fresh decoder. The server
   * has no keyframe-request message, so this waits for its natural keyframe
   * (every GOP, 3 s by default, while H.264 is streaming).
   */
  private awaitingKeyframe = false;

  constructor(
    /** `frameSeq` is the value given to `decode()` for the chunk this frame came from. */
    private onFrame: (frame: VideoFrame, frameSeq: number) => void,
    private width: number,
    private height: number,
  ) {
    this.open();
  }

  private open(): void {
    // Each decoder's callbacks are bound to that instance, so a late callback
    // from a decoder we already replaced cannot flag the new one.
    const decoder: VideoDecoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        // M3.2b: the rAF tick is the sole closer of consumed VideoFrames.
        // Do NOT auto-close the previous frame here.
        this.latestFrame = frame;
        // The chunk's timestamp carries its frame_seq through the decoder, so
        // the association survives a chunk that was rejected or dropped.
        this.onFrame(frame, frame.timestamp);
      },
      error: (e: DOMException) => {
        console.error('Full-frame H264 decode error:', e.message);
        if (this.decoder === decoder) this.awaitingKeyframe = true;
      },
    });
    this.decoder = decoder;
    this.decoder.configure({
      codec: 'avc1.42001e',
      codedWidth: this.width,
      codedHeight: this.height,
      optimizeForLatency: true,
    });
  }

  decode(nalData: Uint8Array, isKeyframe: boolean, frameSeq: number) {
    if (this.disposed) return;
    if (this.awaitingKeyframe || this.decoder.state === 'closed') {
      if (!isKeyframe) return;
      if (this.decoder.state !== 'closed') this.decoder.close();
      this.awaitingKeyframe = false;
      this.open();
    }
    const chunk = new EncodedVideoChunk({
      type: isKeyframe ? 'key' : 'delta',
      timestamp: frameSeq,
      data: nalData,
    });
    try {
      this.decoder.decode(chunk);
    } catch (e) {
      // DataError: a delta arrived while the decoder still wants a key chunk
      // (lost session-start IDR, or a key chunk the browser rejected). Drop
      // the deltas that follow quietly instead of throwing for each one.
      this.awaitingKeyframe = true;
      throw e;
    }
  }

  close() {
    this.disposed = true;
    if (this.decoder.state !== 'closed') {
      this.decoder.close();
    }
    if (this.latestFrame) {
      this.latestFrame.close();
      this.latestFrame = null;
    }
  }
}

