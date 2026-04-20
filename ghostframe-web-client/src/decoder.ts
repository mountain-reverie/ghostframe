export const DATAGRAM_HEADER_SIZE = 12;
export const TILE_HEADER_SIZE = 8;
export const TILE_SIZE = 32;

export const enum Codec {
  Skip = 0, H264 = 1, PalRle = 2, Bc1 = 3, Solid = 4, Raw = 5, Cdf53 = 6,
}

export interface DatagramHeader {
  frameSeq: number;
  fragIdx: number;
  fragTotal: number;
  timestampUs: number;
}

export interface TileHeader {
  tileX: number;
  tileY: number;
  codec: Codec;
  lz4: boolean;
  generation: number;
  payloadLen: number;
}

export function decodeDatagramHeader(view: DataView, offset: number): DatagramHeader {
  return {
    frameSeq: view.getUint32(offset, false),      // big-endian
    fragIdx: view.getUint16(offset + 4, false),
    fragTotal: view.getUint16(offset + 6, false),
    timestampUs: view.getUint32(offset + 8, false),
  };
}

export function decodeTileHeader(view: DataView, offset: number): TileHeader {
  const codecLz4 = view.getUint8(offset + 2);
  return {
    tileX: view.getUint8(offset),
    tileY: view.getUint8(offset + 1),
    codec: (codecLz4 >> 1) as Codec,
    lz4: (codecLz4 & 1) !== 0,
    generation: view.getUint8(offset + 3),
    payloadLen: view.getUint32(offset + 4, false),
  };
}

export function tileKey(frameSeq: number, tileX: number, tileY: number): string {
  return `${frameSeq}:${tileX}:${tileY}`;
}

export interface TileAssembly {
  header: TileHeader;
  fragments: (Uint8Array | null)[];
  received: number;
}

/**
 * Per-tile H.264 decoder using WebCodecs VideoDecoder.
 * Each tile position gets its own decoder instance to maintain inter-frame state.
 */
export class H264TileDecoder {
  private decoder: VideoDecoder;
  private latestFrame: VideoFrame | null = null;

  constructor(private onFrame: (frame: VideoFrame) => void) {
    this.decoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        if (this.latestFrame) {
          this.latestFrame.close();
        }
        this.latestFrame = frame;
        this.onFrame(frame);
      },
      error: (e: DOMException) => {
        console.error('H264 decode error:', e.message);
      },
    });

    this.decoder.configure({
      codec: 'avc1.42001e', // Baseline profile, level 3.0
      codedWidth: 32,
      codedHeight: 32,
      optimizeForLatency: true,
    });
  }

  decode(nalData: Uint8Array) {
    if (this.decoder.state === 'closed') return;

    const isKey = this.isKeyframe(nalData);

    const chunk = new EncodedVideoChunk({
      type: isKey ? 'key' : 'delta',
      timestamp: 0,
      data: nalData,
    });

    this.decoder.decode(chunk);
  }

  private isKeyframe(data: Uint8Array): boolean {
    // Scan for NAL start code (0x00 0x00 0x01 or 0x00 0x00 0x00 0x01)
    // then check NAL type: 5 = IDR slice, 7 = SPS (precedes keyframe)
    for (let i = 0; i < data.length - 4; i++) {
      if (data[i] === 0 && data[i + 1] === 0) {
        let nalStart: number;
        if (data[i + 2] === 1) {
          nalStart = i + 3;
        } else if (data[i + 2] === 0 && data[i + 3] === 1) {
          nalStart = i + 4;
        } else {
          continue;
        }
        if (nalStart < data.length) {
          const nalType = data[nalStart] & 0x1f;
          if (nalType === 5 || nalType === 7) return true;
        }
      }
    }
    return false;
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
