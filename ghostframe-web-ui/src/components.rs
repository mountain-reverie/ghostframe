// SPDX-License-Identifier: MIT OR Apache-2.0
//! The panel's Leptos components. Each owns one piece of the DOM and reads
//! the one shared `StatsView` signal; none of them touches the transport.

use std::rc::Rc;

use ghostframe_client_core::{MetricKind, StatsView};
use leptos::prelude::*;

use crate::format::{format_row, DASH};
use crate::spark::{points_attr, sparkline_points, HEIGHT, WIDTH};
use crate::{storage, PanelBackend};

const CSS: &str = include_str!("style.css");

/// How often the panel re-reads the core while open.
const REFRESH_MS: i32 = 1000;

/// A one-line inline SVG polyline over a metric's history.
#[component]
pub fn Sparkline(history: Signal<Vec<f64>>) -> impl IntoView {
    let points = move || points_attr(&sparkline_points(&history.get(), WIDTH, HEIGHT));
    view! {
        <svg class="gf-spark" viewBox=format!("0 0 {WIDTH} {HEIGHT}") preserveAspectRatio="none">
            <polyline fill="none" stroke="currentColor" stroke-width="1.5" points=points />
        </svg>
    }
}

/// One metric: label, formatted value, and (except for text) a sparkline.
/// The `data-*` attributes carry the raw state for the e2e suite: the label,
/// the unformatted number (empty when there is none) and whether the row is
/// stale. It looks its row up by `id` on every refresh, so the keyed list above it
/// keeps this component alive while the numbers move.
#[component]
pub fn MetricRow(id: String, view: RwSignal<StatsView>) -> impl IntoView {
    let row = Memo::new(move |_| view.with(|v| v.row(&id).cloned()));
    let history = Signal::derive(move || row.get().map(|r| r.history).unwrap_or_default());
    let has_chart = move || {
        row.get()
            .is_some_and(|r| !matches!(r.kind, MetricKind::Text))
    };
    view! {
        <div
            class="gf-row"
            data-testid="stats-row"
            data-label=move || row.get().map(|r| r.label).unwrap_or_default()
            data-current=move || {
                row.get().and_then(|r| r.current).map(|v| v.to_string()).unwrap_or_default()
            }
            data-stale=move || row.get().is_some_and(|r| r.stale).to_string()
            class:gf-stale=move || row.get().is_some_and(|r| r.stale)
            class:gf-local=move || row.get().is_some_and(|r| r.local)
        >
            <span class="gf-label">{move || row.get().map(|r| r.label).unwrap_or_default()}</span>
            <span class="gf-value">
                {move || row.get().map(|r| format_row(&r)).unwrap_or_else(|| DASH.to_string())}
            </span>
            <Show when=has_chart>
                <Sparkline history=history />
            </Show>
        </div>
    }
}

/// One titled section. Its rows are keyed by id, so a metric the server adds
/// appears here without client code.
#[component]
pub fn Group(key: u8, title: String, view: RwSignal<StatsView>) -> impl IntoView {
    let ids = Memo::new(move |_| {
        view.with(|v| {
            v.group(key)
                .map(|g| g.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    view! {
        <section>
            <h3>{title}</h3>
            <For
                each=move || ids.get()
                key=|id| id.clone()
                children=move |id| view! { <MetricRow id=id view=view /> }
            />
        </section>
    }
}

/// Client and server versions; an alert and the way out when they differ.
#[component]
pub fn VersionRow(view: RwSignal<StatsView>) -> impl IntoView {
    let differ = move || !view.with(|v| v.versions_match);
    view! {
        <div class="gf-versions" class:gf-alert=differ>
            <div>"Client " {move || view.with(|v| v.client_version.clone())}</div>
            <div>
                "Server "
                {move || view.with(|v| v.server_version.clone().unwrap_or_else(|| DASH.to_string()))}
                {move || view.with(|v| v.server_host.clone().map(|h| format!(" on {h}")))}
            </div>
            <Show when=differ>
                <div class="gf-hint">"versions differ \u{2014} hard-reload (Ctrl+Shift+R)"</div>
            </Show>
        </div>
    }
}

/// The root: the edge tab, and the body while open.
///
/// Opening asks the core to subscribe (it queues the message; the page's
/// transmit loop sends it) and closing to unsubscribe. A panel that starts
/// open, from `localStorage`, subscribes at once.
#[component]
pub fn StatsPanel(backend: Rc<dyn PanelBackend>) -> impl IntoView {
    // Rc is not Send; the panel runs on the one browser thread.
    let backend = StoredValue::new_local(backend);
    let open = RwSignal::new(storage::load_open());
    let stats = RwSignal::new(StatsView::default());
    let refresh = move || backend.with_value(|b| stats.set(b.view()));

    if open.get_untracked() {
        backend.with_value(|b| b.set_subscribed(true));
        refresh();
    }

    // One refresh a second while open. Closed, the core's own sampling goes
    // on and nothing here runs.
    let timer = set_interval(
        move || {
            if open.get_untracked() {
                refresh();
            }
        },
        std::time::Duration::from_millis(REFRESH_MS as u64),
    )
    .ok();
    on_cleanup(move || {
        if let Some(t) = timer {
            t.clear();
        }
    });

    let toggle = move |_| {
        let next = !open.get_untracked();
        open.set(next);
        storage::save_open(next);
        backend.with_value(|b| b.set_subscribed(next));
        if next {
            refresh();
        }
    };

    // The groups present, in display order. A memo, so the keyed list below
    // is rebuilt only when a group appears or goes, not every second.
    let groups = Memo::new(move |_| {
        stats.with(|v| {
            v.groups
                .iter()
                .map(|g| (g.key, g.title.to_string()))
                .collect::<Vec<_>>()
        })
    });

    view! {
        <style>{CSS}</style>
        <div class="gf-panel" class:gf-open=move || open.get()>
            <button
                class="gf-tab"
                data-testid="stats-tab"
                tabindex="-1"
                aria-label="Connection statistics"
                // Cancelling mousedown stops the button taking keyboard focus,
                // so keys keep going to the remote session.
                on:mousedown=move |ev| ev.prevent_default()
                on:click=toggle
            >
                {move || if open.get() { ">" } else { "<" }}
            </button>
            <Show when=move || open.get()>
                <div class="gf-body">
                    <VersionRow view=stats />
                    <For
                        each=move || groups.get()
                        key=|g| g.0
                        children=move |(key, title)| view! { <Group key=key title=title view=stats /> }
                    />
                    <Show when=move || { stats.with(|v| v.samples_rejected > 0) }>
                        <div class="gf-rejected">
                            "Rejected stats frames: " {move || stats.with(|v| v.samples_rejected)}
                        </div>
                    </Show>
                </div>
            </Show>
        </div>
    }
}
