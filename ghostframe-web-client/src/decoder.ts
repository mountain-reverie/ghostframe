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
