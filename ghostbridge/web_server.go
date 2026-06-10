package main

import (
	"context"
	"crypto/tls"
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log"
	"net"
	"net/http"
	"os"

	"tailscale.com/tsnet"
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

// loadStaticCertFromEnv reads GHOSTFRAME_WEB_TLS_CERT_PEM and
// GHOSTFRAME_WEB_TLS_KEY_PEM and returns a parsed certificate when both
// are set. Used by the e2e harness to substitute a self-signed cert
// for tsnet's LE provisioning, which headscale-backed e2e cannot reach.
//
// Returns (nil, nil) when neither env var is set (production path).
// Returns an error if exactly one is set (clear misconfiguration) or
// if parsing fails.
func loadStaticCertFromEnv() (*tls.Certificate, error) {
	certPEM := os.Getenv("GHOSTFRAME_WEB_TLS_CERT_PEM")
	keyPEM := os.Getenv("GHOSTFRAME_WEB_TLS_KEY_PEM")
	if certPEM == "" && keyPEM == "" {
		return nil, nil
	}
	if certPEM == "" || keyPEM == "" {
		return nil, fmt.Errorf("GHOSTFRAME_WEB_TLS_{CERT,KEY}_PEM must be set together")
	}
	cert, err := tls.X509KeyPair([]byte(certPEM), []byte(keyPEM))
	if err != nil {
		return nil, fmt.Errorf("X509KeyPair: %w", err)
	}
	return &cert, nil
}

// startWebListeners brings up the :80 (redirect) and :443 (TLS) listeners
// on the given tsnet.Server. In production mode (staticCert == nil) it
// uses tsnet's LE-backed ListenTLS; in e2e mode it uses a plain Listen
// wrapped with the supplied static cert.
//
// Returns immediately after the listeners are bound; the http.Serve
// goroutines run for the lifetime of the process.
func startWebListeners(
	ctx context.Context,
	srv *tsnet.Server,
	certHashHex string,
	staticCert *tls.Certificate,
) error {
	// :443 — TLS
	var tlsLn net.Listener
	if staticCert != nil {
		raw, err := srv.Listen("tcp", ":443")
		if err != nil {
			return err
		}
		tlsLn = tls.NewListener(raw, &tls.Config{
			Certificates: []tls.Certificate{*staticCert},
		})
	} else {
		// Production: tsnet's automatic LE provisioning for *.ts.net.
		var err error
		tlsLn, err = srv.ListenTLS("tcp", ":443")
		if err != nil {
			return err
		}
	}

	// :80 — plain HTTP redirect
	redirLn, err := srv.Listen("tcp", ":80")
	if err != nil {
		tlsLn.Close()
		return err
	}

	mux := newWebMux(certHashHex)
	go func() {
		err := http.Serve(tlsLn, mux)
		if !errors.Is(err, net.ErrClosed) {
			log.Printf("ghostbridge: :443 serve exited: %v", err)
		}
	}()
	go func() {
		err := http.Serve(redirLn, newRedirectHandler())
		if !errors.Is(err, net.ErrClosed) {
			log.Printf("ghostbridge: :80 serve exited: %v", err)
		}
	}()

	_ = ctx // reserved for graceful-shutdown wiring
	return nil
}
