/**
 * Swallowed-exception accounting and a small console ring, readable from the
 * page as `window.__ghostframeHealth()`.
 *
 * The client deliberately keeps running after a throw (one bad event must not
 * freeze the session). The cost is that a tile the core has already ACKed can
 * vanish with nothing but a console line. This keeps a count, the first
 * message and the most recent ones, so a failing e2e run can say where.
 */

const MAX_RECENT = 20;
const MAX_CONSOLE = 300;
/** The first lines are kept apart, so a long run cannot rotate them out. */
const MAX_CONSOLE_HEAD = 100;

export interface HealthSnapshot {
  swallowed: number;
  bySite: Record<string, number>;
  first: string;
  recent: string[];
  console: string[];
  consoleHead: string[];
  tallies: Record<string, number>;
  marks: Array<{ tileX: number; tileY: number; frameSeq: number; what: string }>;
}

export class Health {
  swallowed = 0;
  bySite: Record<string, number> = {};
  first = '';
  recent: string[] = [];
  consoleRing: string[] = [];
  consoleHead: string[] = [];
  tallies: Record<string, number> = {};
  marks: Array<{ tileX: number; tileY: number; frameSeq: number; what: string }> = [];

  /** Remember a few interesting tiles (bounded), for pixel read-back on failure. */
  mark(tileX: number, tileY: number, frameSeq: number, what: string): void {
    if (this.marks.length < 12) this.marks.push({ tileX, tileY, frameSeq, what });
  }

  /** Count an observation (bounded: at most 64 distinct keys). */
  tally(key: string): void {
    if (!(key in this.tallies) && Object.keys(this.tallies).length >= 64) return;
    this.tallies[key] = (this.tallies[key] ?? 0) + 1;
  }

  record(site: string, err: unknown): void {
    this.swallowed++;
    this.bySite[site] = (this.bySite[site] ?? 0) + 1;
    const msg = `${site}: ${err instanceof Error ? `${err.message}` : String(err)}`;
    if (this.first === '') this.first = msg;
    this.recent.push(msg);
    if (this.recent.length > MAX_RECENT) this.recent.shift();
  }

  note(line: string): void {
    if (this.consoleHead.length < MAX_CONSOLE_HEAD) this.consoleHead.push(line);
    this.consoleRing.push(line);
    if (this.consoleRing.length > MAX_CONSOLE) this.consoleRing.shift();
  }

  snapshot(): HealthSnapshot {
    return {
      swallowed: this.swallowed,
      bySite: { ...this.bySite },
      first: this.first,
      recent: [...this.recent],
      console: [...this.consoleRing],
      consoleHead: [...this.consoleHead],
      tallies: { ...this.tallies },
      marks: [...this.marks],
    };
  }
}

export const health = new Health();

/** Mirror the console into the ring (still printing as before). */
export function installConsoleRing(target: Console = console): void {
  for (const level of ['log', 'info', 'warn', 'error'] as const) {
    if (typeof target[level] !== 'function') continue;
    const orig = target[level].bind(target);
    target[level] = (...args: unknown[]) => {
      try {
        health.note(`${level}: ${args.map((a) => (a instanceof Error ? a.message : String(a))).join(' ')}`);
      } catch {
        /* never let diagnostics throw */
      }
      orig(...args);
    };
  }
}
