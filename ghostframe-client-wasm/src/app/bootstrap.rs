// SPDX-License-Identifier: MIT OR Apache-2.0
//! Port of `bootstrap.ts`: fetch `/config.json` afresh, pin its certificate
//! hashes, open the same-origin WebTransport.

use js_sys::{Array, Reflect, Uint8Array};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{RequestCache, RequestInit, Response};

use crate::app::certs::{hex_to_bytes, pick_cert_hashes};
use crate::app::js::WebTransport;
use crate::web::raw::{js_message, obj};

/// Fetched on every call, never cached: the server rotates its certificate,
/// and a hash remembered from an earlier day pins a cert that is gone
/// (`no-store` also overrides any intermediary's heuristics).
pub async fn bootstrap() -> Result<WebTransport, String> {
    let window = web_sys::window().ok_or("bootstrap: no window")?;
    let init = RequestInit::new();
    init.set_cache(RequestCache::NoStore);
    let resp: Response = JsFuture::from(window.fetch_with_str_and_init("/config.json", &init))
        .await
        .map_err(|e| format!("bootstrap: fetch /config.json: {}", js_message(&e)))?
        // `unchecked_into`, not `dyn_into`: the tests stub `fetch` with a plain
        // object that has the members this reads (`ok`, `status`, `json`).
        .unchecked_into();
    if !resp.ok() {
        return Err(format!(
            "bootstrap: /config.json returned HTTP {}",
            resp.status()
        ));
    }
    let cfg = JsFuture::from(resp.json().map_err(|e| js_message(&e))?)
        .await
        .map_err(|e| format!("bootstrap: /config.json is not JSON: {}", js_message(&e)))?;
    let listed: Option<Vec<Option<String>>> = Reflect::get(&cfg, &"certHashes".into())
        .ok()
        .filter(Array::is_array)
        .map(|v| Array::from(&v).iter().map(|h| h.as_string()).collect());
    let single = Reflect::get(&cfg, &"certHash".into())
        .ok()
        .and_then(|v| v.as_string());
    let hashes = pick_cert_hashes(listed.as_deref(), single.as_deref())?;

    let pins = Array::new();
    for h in &hashes {
        let bytes = hex_to_bytes(h)?;
        pins.push(&obj(&[
            ("algorithm", "sha-256".into()),
            ("value", Uint8Array::from(bytes.as_slice()).buffer().into()),
        ]));
    }
    let options = obj(&[
        // A hint to the browser's own congestion controller (its sending
        // direction: input, ACKs, NACKs): favour latency. A browser that does
        // not know the member ignores it.
        ("congestionControl", "low-latency".into()),
        ("serverCertificateHashes", pins.into()),
    ]);
    let host = window.location().host().map_err(|e| js_message(&e))?;
    WebTransport::new(&format!("https://{host}/"), &options).map_err(|e| js_message(&e))
}
