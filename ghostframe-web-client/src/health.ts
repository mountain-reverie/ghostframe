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

export interface HealthSnapshot {
  swallowed: number;
  bySite: Record<string, number>;
  first: string;
  recent: string[];
  console: string[];
}

export class Health {
  swallowed = 0;
  bySite: Record<string, number> = {};
  first = '';
  recent: string[] = [];
  consoleRing: string[] = [];

  record(site: string, err: unknown): void {
    this.swallowed++;
    this.bySite[site] = (this.bySite[site] ?? 0) + 1;
    const msg = `${site}: ${err instanceof Error ? `${err.message}` : String(err)}`;
    if (this.first === '') this.first = msg;
    this.recent.push(msg);
    if (this.recent.length > MAX_RECENT) this.recent.shift();
  }

  note(line: string): void {
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
    };
  }
}

export const health = new Health();

/** Mirror console.warn/error into the ring (still printing as before). */
export function installConsoleRing(target: Console = console): void {
  for (const level of ['warn', 'error'] as const) {
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
