//go:build noweb

package main

// The `-tags noweb` half of the web_dist.go pair: no //go:embed, so no
// ghostframe-web-client/dist has to exist for this archive to compile.
//
// This is the shape the native client wants. It links ghostbridge to reach
// tsnet and nothing else, so requiring `npm install && vite build` before
// `cargo build -p ghostframe-cli` bought it exactly nothing.

import "net/http"

// webDistEmbedded is false here, and startWebListeners refuses to bind on
// that. A noweb archive reaching the web-server path means a *server* was
// built with the client's flags -- worth a hard error, because the
// alternative symptom is a daemon that binds :443 and serves 404 for the
// SPA, which reads like a browser problem.
const webDistEmbedded = false

// newWebMux is unreachable in a noweb build (startWebListeners returns
// before constructing it). Defined so main.go compiles under both tags,
// and it answers 501 rather than nil-panicking if that guard ever moves.
func newWebMux(certHashHex string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Error(w,
			"ghostbridge was built with -tags noweb: no embedded web client",
			http.StatusNotImplemented)
	})
}
