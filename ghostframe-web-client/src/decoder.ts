// Full-frame H.264 decode via WebCodecs. All wire parsing lives in the wasm
// core; this is only the browser-side VideoDecoder wrapper.

export class FullFrameDecoder {
  private decoder: VideoDecoder;
  private latestFrame: VideoFrame | null = null;

  constructor(
    private onFrame: (frame: VideoFrame) => void,
    width: number,
    height: number,
  ) {
    this.decoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        // M3.2b: the rAF tick is the sole closer of consumed VideoFrames.
        // Do NOT auto-close the previous frame here.
        this.latestFrame = frame;
        this.onFrame(frame);
      },
      error: (e: DOMException) => {
        console.error('Full-frame H264 decode error:', e.message);
      },
    });

    this.decoder.configure({
      codec: 'avc1.42001e',
      codedWidth: width,
      codedHeight: height,
      optimizeForLatency: true,
    });
  }

  decode(nalData: Uint8Array, isKeyframe: boolean) {
    if (this.decoder.state === 'closed') return;
    const chunk = new EncodedVideoChunk({
      type: isKeyframe ? 'key' : 'delta',
      timestamp: 0,
      data: nalData,
    });
    this.decoder.decode(chunk);
  }

  close() {
    if (this.decoder.state !== 'closed') {
      this.decoder.close();
    }
    if (this.latestFrame) {
      this.latestFrame.close();
      this.latestFrame = null;
    }
  }
}

