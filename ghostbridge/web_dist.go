//go:build !noweb

package main

// The embedded browser SPA, and the mux that serves it.
//
// Split out of web_server.go so the whole embed can be dropped with
// `-tags noweb`. The native client links ghostbridge for tsnet only and
// never serves a browser anything, so making it build the Vite bundle
// first was a cost it had no way to pay back. See web_dist_noweb.go for
// the other half of the pair, and ghostframe-tsnet's `web-embed` feature
// for how the tag gets chosen.

import (
	"embed"
	"encoding/json"
	"io/fs"
	"net/http"
)

// webDistEmbedded records which half of the pair got compiled. Read by
// startWebListeners, which refuses to bind when the SPA is absent rather
// than serving 404s that look like a client bug.
const webDistEmbedded = true

//go:embed all:dist
var webDist embed.FS

// distFS returns the dist tree rooted at "dist/" so handlers can serve
// "/index.html" instead of "/dist/index.html". Errors at startup are a
// build-config bug (missing //go:embed sources), not a runtime concern.
func distFS() fs.FS {
	sub, err := fs.Sub(webDist, "dist")
	if err != nil {
		panic("ghostbridge: dist/ subtree missing from embed: " + err.Error())
	}
	return sub
}

// hstsHeader is the value of Strict-Transport-Security set on every
// HTTPS response. One-year max-age locks browsers onto HTTPS for the
// daemon's tailnet hostname; includeSubDomains is harmless because the
// daemon is the only thing serving on this name.
const hstsHeader = "max-age=31536000; includeSubDomains"

// newWebMux builds the HTTP handler mux for the embedded SPA + config.
// certHashHex is the lowercase-hex SHA-256 of the WebTransport server cert.
func newWebMux(certHashHex string) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/config.json", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Cache-Control", "no-store")
		_ = json.NewEncoder(w).Encode(struct {
			CertHash string `json:"certHash"`
		}{CertHash: certHashHex})
	})
	mux.Handle("/", http.FileServer(http.FS(distFS())))
	// Wrap so every HTTPS response carries HSTS.
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Strict-Transport-Security", hstsHeader)
		mux.ServeHTTP(w, r)
	})
}
