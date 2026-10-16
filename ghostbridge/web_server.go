package main

import (
	"context"
	"crypto/tls"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"tailscale.com/tsnet"
)

// The embedded SPA and the mux that serves it live in the build-tagged
// pair web_dist.go / web_dist_noweb.go.

// certHashStore holds the WebTransport certificate hashes published at
// /config.json. The daemon rotates its certificate in-process, so the list
// changes while the process runs; handlers read it on every request, which is
// what makes a per-session fetch return the hashes of the day rather than the
// ones from startup.
type certHashStore struct {
	mu     sync.RWMutex
	hashes []string
}

func newCertHashStore(hashes ...string) *certHashStore {
	s := &certHashStore{}
	s.set(hashes)
	return s
}

func (s *certHashStore) set(hashes []string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.hashes = append([]string(nil), hashes...)
}

func (s *certHashStore) get() []string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return append([]string(nil), s.hashes...)
}

// parseCertHashList splits a comma-separated list of 64-hex-char SHA-256
// hashes, the form Rust passes across the FFI. It rejects an empty list or
// any element of the wrong length.
func parseCertHashList(csv string) ([]string, error) {
	parts := strings.Split(csv, ",")
	for _, p := range parts {
		if len(p) != 64 {
			return nil, fmt.Errorf("certHash length %d, want 64", len(p))
		}
		// Lowercase hex only: a non-hex element would make every browser's
		// hexToBytes throw, so reject it here where the cause is nameable.
		for _, c := range p {
			if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
				return nil, fmt.Errorf("certHash has non-hex character %q", c)
			}
		}
	}
	return parts, nil
}

// writeConfigJSON writes the /config.json body. `certHash` (the hash being
// served now) is kept for older clients; `certHashes` is the full list a
// client should pin, current first. Never cacheable: a cached copy is
// exactly the stale pin rotation exists to avoid.
func writeConfigJSON(w http.ResponseWriter, hashes *certHashStore) {
	list := hashes.get()
	first := ""
	if len(list) > 0 {
		first = list[0]
	}
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	_ = json.NewEncoder(w).Encode(struct {
		CertHash   string   `json:"certHash"`
		CertHashes []string `json:"certHashes"`
	}{CertHash: first, CertHashes: list})
}

// newRedirectHandler returns an HTTP handler that 301-redirects every
// request to the same host:path on https://, EXCEPT /config.json, which it
// serves directly.
//
// Used for the tsnet :80 listener so users can paste the bare-hostname URL.
//
// Why /config.json is exempt: a non-browser client needs the WebTransport
// cert hash before it can open a session, and it has no reason to speak TLS
// to get it. The :443 listener exists because browsers refuse WebTransport
// over anything else -- not for confidentiality on the wire. Both listeners
// are tsnet listeners, so every byte reaching them has already been
// encrypted and authenticated by WireGuard; TLS here is a second envelope
// inside a tunnel that already has one.
//
// The hash itself is a public fingerprint, not a secret: it identifies a
// certificate, and holding it grants nothing without the private key. The
// browser fetches the same value over the same tailnet today.
//
// Without this, a native client's plaintext GET to :443 blocks forever --
// the server is waiting for a ClientHello and the client is waiting for a
// response. That failure is silent and reads like a network fault.
func newRedirectHandler(hashes *certHashStore) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/config.json" {
			writeConfigJSON(w, hashes)
			return
		}
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
//
// Every request on both listeners goes through requireAuthorizedPeer first:
// a peer the access policy refuses gets a bare 403 -- no SPA, no
// /config.json, no redirect.
func startWebListeners(
	ctx context.Context,
	srv *tsnet.Server,
	hashes *certHashStore,
	staticCert *tls.Certificate,
	who whoiser,
	authz *authorizerSlot,
) error {
	// A noweb archive has no SPA to serve. Fail the bind instead of coming
	// up and answering 404 for every asset: the second failure mode is
	// indistinguishable, from the browser, from a client-side bug, and this
	// one names the cause. Reaching here means a *server* was built with
	// the native client's `-tags noweb`.
	if !webDistEmbedded {
		return errors.New(
			"ghostbridge built with -tags noweb: no embedded web client " +
				"(that build is for the native client; a server needs " +
				"ghostframe-tsnet's `web-embed` feature)")
	}

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
		Handler:           requireAuthorizedPeer(newWebMux(hashes), who, authz),
		ErrorLog:          tlsErrLog,
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       30 * time.Second,
		WriteTimeout:      30 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	redirServer := &http.Server{
		Handler:           requireAuthorizedPeer(newRedirectHandler(hashes), who, authz),
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
