package main

import (
	"embed"
	"encoding/json"
	"io/fs"
	"net/http"
)

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

// newWebMux builds the HTTP handler mux for the embedded SPA + config.
// certHashHex is the lowercase-hex SHA-256 of the WebTransport server cert.
func newWebMux(certHashHex string) *http.ServeMux {
	mux := http.NewServeMux()
	mux.HandleFunc("/config.json", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Cache-Control", "no-store")
		_ = json.NewEncoder(w).Encode(struct {
			CertHash string `json:"certHash"`
		}{CertHash: certHashHex})
	})
	mux.Handle("/", http.FileServer(http.FS(distFS())))
	return mux
}

// newRedirectHandler returns an HTTP handler that 301-redirects every
// request to the same host:path on https://. Used for the tsnet :80
// listener so users can paste the bare-hostname URL.
func newRedirectHandler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		target := "https://" + r.Host + r.URL.RequestURI()
		http.Redirect(w, r, target, http.StatusMovedPermanently)
	})
}
