//! The panel in a real browser: mounted, toggled, and wired to a real core.
//! Run with `just test-web-ui` (headless Chrome through wasm-pack).
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use ghostframe_client_core::{ClientConfig, ClientCore, PollOutput};
use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, StatsSample, StatsSchema,
};
use ghostframe_web_ui::{mount_stats_panel, CoreBackend};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

const STORAGE_KEY: &str = "ghostframe.statsPanel.open";

fn document() -> web_sys::Document {
    web_sys::window().unwrap().document().unwrap()
}

fn clear_storage() {
    let s = web_sys::window().unwrap().local_storage().unwrap().unwrap();
    s.remove_item(STORAGE_KEY).unwrap();
}

fn stored() -> Option<String> {
    let s = web_sys::window().unwrap().local_storage().unwrap().unwrap();
    s.get_item(STORAGE_KEY).unwrap()
}

/// The page's `performance.now()` clock in microseconds, as `main.ts` gives it
/// to the core.
fn now_us() -> u64 {
    (web_sys::window().unwrap().performance().unwrap().now() * 1000.0) as u64
}

fn new_container() -> web_sys::HtmlElement {
    let el: web_sys::HtmlElement = document().create_element("div").unwrap().unchecked_into();
    document().body().unwrap().append_child(&el).unwrap();
    el
}

fn new_core() -> Rc<RefCell<ClientCore>> {
    let mut core = ClientCore::new(ClientConfig::default(), now_us());
    while core.poll_transmit(0).is_some() {} // the Hello
    Rc::new(RefCell::new(core))
}

fn mount(core: &Rc<RefCell<ClientCore>>) -> web_sys::HtmlElement {
    let container = new_container();
    let backend = Rc::new(CoreBackend::new(core.clone()));
    mount_stats_panel(container.clone(), backend).expect("mounts");
    container
}

fn query(container: &web_sys::HtmlElement, selector: &str) -> Option<web_sys::Element> {
    container.query_selector(selector).unwrap()
}

fn tab(container: &web_sys::HtmlElement) -> web_sys::HtmlElement {
    query(container, ".gf-tab")
        .expect("the tab is always there")
        .unchecked_into()
}

/// Let Leptos run the effects a click queued.
async fn settle() {
    leptos::task::tick().await;
    leptos::task::tick().await;
}

fn next_stream(core: &Rc<RefCell<ClientCore>>) -> Option<Vec<u8>> {
    match core.borrow_mut().poll_transmit(0) {
        Some(PollOutput::Stream(b)) => Some(b),
        Some(PollOutput::Datagram(_)) | None => None,
    }
}

fn feed_server_stats(core: &Rc<RefCell<ClientCore>>, server_version: &str) {
    let schema = StatsSchema {
        schema_id: 1,
        server_version: server_version.to_string(),
        host_name: "evangeline".to_string(),
        metrics: vec![
            MetricDescriptor {
                id: 1,
                name: "ghostframe.rtt".to_string(),
                unit: "ms".to_string(),
                kind: MetricKind::Gauge,
                group: MetricGroup::Connection,
                label: "Round trip".to_string(),
            },
            MetricDescriptor {
                id: 2,
                name: "ghostframe.mode".to_string(),
                unit: String::new(),
                kind: MetricKind::Text,
                group: MetricGroup::Connection,
                label: "Mode".to_string(),
            },
        ],
    };
    let sample = StatsSample {
        schema_id: 1,
        timestamp_us: 1_000_000,
        values: vec![MetricValue::Num(18.4), MetricValue::Text("h264".to_string())],
    };
    let mut bytes = schema.encode_frame();
    bytes.extend(sample.encode_frame());
    core.borrow_mut().handle_server_stream(&bytes, now_us());
}

#[wasm_bindgen_test]
async fn the_tab_opens_and_closes_the_panel_and_the_core_is_told() {
    clear_storage();
    let core = new_core();
    let container = mount(&core);
    assert_eq!(tab(&container).text_content().as_deref(), Some("<"));
    assert!(
        query(&container, ".gf-body").is_none(),
        "closed: only the tab"
    );
    assert_eq!(next_stream(&core), None, "closed at mount: nothing to send");

    tab(&container).click();
    settle().await;
    assert_eq!(tab(&container).text_content().as_deref(), Some(">"));
    assert!(query(&container, ".gf-body").is_some());
    assert_eq!(
        next_stream(&core),
        Some(vec![0x0B, 1]),
        "subscribe reached the core"
    );
    assert_eq!(stored().as_deref(), Some("1"));

    tab(&container).click();
    settle().await;
    assert_eq!(tab(&container).text_content().as_deref(), Some("<"));
    assert!(query(&container, ".gf-body").is_none());
    assert_eq!(
        next_stream(&core),
        Some(vec![0x0B, 0]),
        "unsubscribe reached the core"
    );
    assert_eq!(stored().as_deref(), Some("0"));
}

#[wasm_bindgen_test]
fn a_mousedown_on_the_tab_is_cancelled_so_it_cannot_take_focus() {
    clear_storage();
    let container = mount(&new_core());
    let init = web_sys::MouseEventInit::new();
    init.set_bubbles(true);
    init.set_cancelable(true);
    let ev = web_sys::MouseEvent::new_with_mouse_event_init_dict("mousedown", &init).unwrap();
    tab(&container).dispatch_event(&ev).unwrap();
    assert!(ev.default_prevented());
}

#[wasm_bindgen_test]
async fn a_panel_that_starts_open_subscribes_at_once_and_shows_server_rows() {
    clear_storage();
    web_sys::window()
        .unwrap()
        .local_storage()
        .unwrap()
        .unwrap()
        .set_item(STORAGE_KEY, "1")
        .unwrap();
    let core = new_core();
    core.borrow_mut().set_client_version("0.1.0 (aab2fa2d)");
    feed_server_stats(&core, "0.1.0 (aab2fa2d)");
    let container = mount(&core);
    settle().await;
    assert_eq!(next_stream(&core), Some(vec![0x0B, 1]));
    let text = container.text_content().unwrap();
    assert!(text.contains("Round trip"), "{text}");
    assert!(text.contains("18.4 ms"), "{text}");
    assert!(text.contains("Connection"), "{text}");
    assert!(text.contains("evangeline"), "{text}");
    assert!(query(&container, ".gf-alert").is_none(), "versions match");
    assert!(
        query(&container, ".gf-spark polyline").is_some(),
        "a sparkline is drawn"
    );

    // The raw-state hooks PR 3's e2e reads. They are asserted here, in the PR
    // that ships them: PR 3 adds only the hooks this PR lacks.
    assert!(
        query(&container, "[data-testid=\"stats-tab\"]").is_some(),
        "the tab carries its e2e hook"
    );
    let rtt = query(
        &container,
        "[data-testid=\"stats-row\"][data-label=\"Round trip\"]",
    )
    .expect("the Round trip row carries data-testid and data-label");
    assert_eq!(
        rtt.get_attribute("data-current").as_deref(),
        Some("18.4"),
        "the raw, unformatted number"
    );
    assert_eq!(rtt.get_attribute("data-stale").as_deref(), Some("false"));
    let mode = query(
        &container,
        "[data-testid=\"stats-row\"][data-label=\"Mode\"]",
    )
    .expect("the Mode row");
    assert_eq!(
        mode.get_attribute("data-current").as_deref(),
        Some(""),
        "a text row has no number: the attribute is there and empty"
    );
    assert!(mode.text_content().unwrap_or_default().contains("h264"));
}

#[wasm_bindgen_test]
async fn differing_versions_raise_the_alert_with_the_hard_reload_hint() {
    clear_storage();
    web_sys::window()
        .unwrap()
        .local_storage()
        .unwrap()
        .unwrap()
        .set_item(STORAGE_KEY, "1")
        .unwrap();
    let core = new_core();
    core.borrow_mut().set_client_version("0.1.0 (aaaaaaaa)");
    feed_server_stats(&core, "0.1.0 (bbbbbbbb)");
    let container = mount(&core);
    settle().await;
    assert!(query(&container, ".gf-alert").is_some());
    assert!(container.text_content().unwrap().contains("hard-reload"));
}
