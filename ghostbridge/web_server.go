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
	"time"

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

// loggingListener wraps a net.Listener so that every Accept (TCP-level —
// before any TLS handshake) is visible in the journal. Without this, a
// browser that reaches the :443 socket but fails inside Go's TLS handshake
// (e.g. cert provisioning returns an error and Go sends 'internal_error')
// is indistinguishable from a browser that never connected at all.
type loggingListener struct {
	net.Listener
	label string
}

func (l *loggingListener) Accept() (net.Conn, error) {
	c, err := l.Listener.Accept()
	if err != nil {
		return nil, err
	}
	log.Printf("ghostbridge: %s accept from %s", l.label, c.RemoteAddr())
	return c, nil
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

	// Explicit *http.Server with timeouts. http.Serve's bare default leaves
	// every timeout at zero, exposing the daemon to Slowloris-style stalls
	// from a misbehaving tailnet peer. The values are conservative for a
	// small static-asset + JSON server.
	//
	// ErrorLog: route http.Server's diagnostic output (including the
	// "TLS handshake error from X: Y" line that's the most direct signal
	// of a cert-provisioning failure) into our journal with a prefix so
	// it's easy to spot when triaging connection failures.
	tlsErrLog := log.New(log.Writer(), "ghostbridge: :443 http: ", log.LstdFlags)
	tlsServer := &http.Server{
		Handler:           newWebMux(certHashHex),
		ErrorLog:          tlsErrLog,
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       30 * time.Second,
		WriteTimeout:      30 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	redirServer := &http.Server{
		Handler:           newRedirectHandler(),
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       10 * time.Second,
		WriteTimeout:      10 * time.Second,
		IdleTimeout:       10 * time.Second,
	}
	// Wrap the production TLS listener so TCP accepts are visible. (Skip the
	// wrap on staticCert mode — the e2e harness already has its own logging
	// and the extra Accept log would just add noise.)
	if staticCert == nil {
		tlsLn = &loggingListener{Listener: tlsLn, label: ":443"}
		redirLn = &loggingListener{Listener: redirLn, label: ":80"}
	}
	go func() {
		err := tlsServer.Serve(tlsLn)
		if !errors.Is(err, net.ErrClosed) && !errors.Is(err, http.ErrServerClosed) {
			log.Printf("ghostbridge: :443 serve exited: %v", err)
		}
	}()
	go func() {
		err := redirServer.Serve(redirLn)
		if !errors.Is(err, net.ErrClosed) && !errors.Is(err, http.ErrServerClosed) {
			log.Printf("ghostbridge: :80 serve exited: %v", err)
		}
	}()

	_ = ctx // reserved for graceful-shutdown wiring
	return nil
}
