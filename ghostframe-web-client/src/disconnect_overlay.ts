/**
 * Terminal disconnect overlay, shown when the server tells this client it
 * has been evicted (`WasmEvent.Evicted`, ghostframe-client-wasm/src/
 * boundary.rs) via ghostframe-protocol's eviction sentinel datagram.
 *
 * Deliberately has no reconnect button: reloading the page is the
 * reconnect, and a button invites two clients racing to evict each other,
 * which the user would experience as both windows flickering.
 */
export function showDisconnectedOverlay(message: string): void {
  if (document.getElementById('gf-disconnected')) return;
  const el = document.createElement('div');
  el.id = 'gf-disconnected';
  el.setAttribute('role', 'status');
  el.textContent = message;
  el.style.cssText = [
    'position:fixed', 'inset:0', 'display:flex',
    'align-items:center', 'justify-content:center',
    'background:rgba(0,0,0,0.82)', 'color:#fff',
    'font:16px system-ui,sans-serif', 'z-index:9999',
    'text-align:center', 'padding:2rem',
  ].join(';');
  document.body.appendChild(el);
}
