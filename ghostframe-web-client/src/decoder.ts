// Full-frame H.264 decode via WebCodecs. All wire parsing lives in the wasm
// core; this is only the browser-side VideoDecoder wrapper.

/**
 * What the decoder did with a chunk, as the wasm core's `h264Result` takes it
 * (`ClientCore::h264_result`): the frame ack, the reference gate and the IDR
 * request after a failure all hang off it.
 */
export const H264_DECODED = 0;
export const H264_NOT_OUTPUT = 1;
export const H264_DECODE_ERROR = 2;

export class FullFrameDecoder {
  private decoder!: VideoDecoder;
  private latestFrame: VideoFrame | null = null;
  /** Set by `close()`: the owner is done, never rebuild. */
  private disposed = false;
  /**
   * An async decode error closes the WebCodecs decoder for good, and the delta
   * chunks that follow cannot be decoded without the reference they lost. Until
   * the next key chunk, drop deltas; on it, build a fresh decoder. The failure
   * is reported through `onDecodeFailure` so the owner can ask the server for
   * an IDR now instead of waiting out the GOP (3 s by default).
   */
  private awaitingKeyframe = false;
  /** The frame_seq of the last chunk given to the decoder. */
  private lastFrameSeq = 0;
  /**
   * Chunks the decoder accepted and has not answered for, in decode order. An
   * output for chunk N answers every earlier chunk too: WebCodecs outputs in
   * decode order, so one that never came out never will.
   */
  private pending: number[] = [];

  constructor(
    /** `frameSeq` is the value given to `decode()` for the chunk this frame came from. */
    private onFrame: (frame: VideoFrame, frameSeq: number) => void,
    private width: number,
    private height: number,
    /** Called with the frame_seq last handed to the decoder when it fails. */
    private onDecodeFailure: (frameSeq: number) => void = () => {},
    /**
     * Called once per chunk handed to `decode()` that did not fail the
     * decoder: `H264_DECODED` when the output callback fired for its
     * timestamp, `H264_NOT_OUTPUT` when it was dropped or overtaken.
     */
    private onResult: (frameSeq: number, outcome: number) => void = () => {},
  ) {
    this.open();
  }

  private open(): void {
    // Each decoder's callbacks are bound to that instance, so a late callback
    // from a decoder we already replaced cannot flag the new one.
    const decoder: VideoDecoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        if (this.decoder !== decoder) {
          frame.close();
          return;
        }
        // M3.2b: the rAF tick is the sole closer of consumed VideoFrames.
        // Do NOT auto-close the previous frame here.
        this.latestFrame = frame;
        // The chunk's timestamp carries its frame_seq through the decoder, so
        // the association survives a chunk that was rejected or dropped.
        this.onFrame(frame, frame.timestamp);
        this.answer(frame.timestamp);
      },
      error: (e: DOMException) => {
        console.error('Full-frame H264 decode error:', e.message);
        if (this.decoder === decoder) {
          this.awaitingKeyframe = true;
          // Everything in flight before the chunk that failed produced
          // nothing; the failed chunk itself is the failure.
          this.answerAll(H264_NOT_OUTPUT, this.lastFrameSeq);
          this.onDecodeFailure(this.lastFrameSeq);
        }
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

  /** The output for `timestamp` came out: it is decoded, and overtakes earlier chunks. */
  private answer(timestamp: number): void {
    const i = this.pending.indexOf(timestamp);
    if (i < 0) return;
    for (const earlier of this.pending.splice(0, i)) this.onResult(earlier, H264_NOT_OUTPUT);
    this.pending.shift();
    this.onResult(timestamp, H264_DECODED);
  }

  /** Answer every chunk in flight (except `except`) with `outcome`. */
  private answerAll(outcome: number, except?: number): void {
    const all = this.pending;
    this.pending = [];
    for (const seq of all) if (seq !== except) this.onResult(seq, outcome);
  }

  /**
   * A key chunk to a healthy decoder is just decoded: WebCodecs takes an IDR
   * without a reset, and rebuilding here would close the decoder on every GOP
   * IDR and discard the P frames in flight. The decoder is rebuilt only after
   * a failure, on the key chunk that follows it (below). That is also where
   * the core's `reset_decoder` lands in the browser, so the flag is not passed.
   */
  decode(nalData: Uint8Array, isKeyframe: boolean, frameSeq: number) {
    if (this.disposed) return;
    if (this.awaitingKeyframe || this.decoder.state === 'closed') {
      if (!isKeyframe) {
        this.onResult(frameSeq, H264_NOT_OUTPUT);
        return;
      }
      if (this.decoder.state !== 'closed') this.decoder.close();
      this.answerAll(H264_NOT_OUTPUT);
      this.awaitingKeyframe = false;
      this.open();
    }
    this.lastFrameSeq = frameSeq;
    const chunk = new EncodedVideoChunk({
      type: isKeyframe ? 'key' : 'delta',
      timestamp: frameSeq,
      data: nalData,
    });
    // Queued before the call: a decoder may answer from inside decode().
    this.pending.push(frameSeq);
    try {
      this.decoder.decode(chunk);
    } catch (e) {
      // DataError: a delta arrived while the decoder still wants a key chunk
      // (lost session-start IDR, or a key chunk the browser rejected). Drop
      // the deltas that follow quietly instead of throwing for each one.
      this.awaitingKeyframe = true;
      this.pending.pop();
      this.answerAll(H264_NOT_OUTPUT);
      this.onDecodeFailure(frameSeq);
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

