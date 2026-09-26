package main

// The two tests that need the embedded SPA live in web_dist_test.go,
// behind the same `!noweb` tag as the embed itself. Everything here holds
// under both tags.

import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"math/big"
	"time"
)

func TestRedirectHandler(t *testing.T) {
	h := newRedirectHandler("deadbeef")
	srv := httptest.NewServer(h)
	defer srv.Close()

	client := &http.Client{
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	resp, err := client.Get(srv.URL + "/some/path?q=1")
	if err != nil {
		t.Fatalf("GET: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 301 {
		t.Fatalf("status %d, want 301", resp.StatusCode)
	}
	loc := resp.Header.Get("Location")
	if !strings.HasPrefix(loc, "https://") {
		t.Fatalf("Location %q does not start with https://", loc)
	}
	if !strings.HasSuffix(loc, "/some/path?q=1") {
		t.Fatalf("Location %q does not preserve path+query", loc)
	}
}

// selfSignedPEM generates a throwaway ECDSA self-signed cert/key pair for tests.
func selfSignedPEM(t *testing.T) (certPEM, keyPEM string) {
	t.Helper()
	priv, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatalf("ecdsa.GenerateKey: %v", err)
	}
	tmpl := &x509.Certificate{
		SerialNumber: big.NewInt(1),
		Subject:      pkix.Name{CommonName: "test"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(time.Hour),
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &priv.PublicKey, priv)
	if err != nil {
		t.Fatalf("x509.CreateCertificate: %v", err)
	}
	certPEM = string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}))
	keyDER, err := x509.MarshalECPrivateKey(priv)
	if err != nil {
		t.Fatalf("MarshalECPrivateKey: %v", err)
	}
	keyPEM = string(pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER}))
	return certPEM, keyPEM
}

func TestLoadStaticCertFromEnv_NeitherSet(t *testing.T) {
	t.Setenv("GHOSTFRAME_WEB_TLS_CERT_PEM", "")
	t.Setenv("GHOSTFRAME_WEB_TLS_KEY_PEM", "")
	cert, err := loadStaticCertFromEnv()
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if cert != nil {
		t.Fatal("expected nil cert when env vars are unset")
	}
}

func TestLoadStaticCertFromEnv_ExactlyOneSet(t *testing.T) {
	certPEM, _ := selfSignedPEM(t)
	t.Setenv("GHOSTFRAME_WEB_TLS_CERT_PEM", certPEM)
	t.Setenv("GHOSTFRAME_WEB_TLS_KEY_PEM", "")
	_, err := loadStaticCertFromEnv()
	if err == nil {
		t.Fatal("expected error when only CERT_PEM is set")
	}
}

func TestLoadStaticCertFromEnv_BothSet(t *testing.T) {
	certPEM, keyPEM := selfSignedPEM(t)
	t.Setenv("GHOSTFRAME_WEB_TLS_CERT_PEM", certPEM)
	t.Setenv("GHOSTFRAME_WEB_TLS_KEY_PEM", keyPEM)
	cert, err := loadStaticCertFromEnv()
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if cert == nil {
		t.Fatal("expected non-nil cert when both env vars are set")
	}
}

// The :80 listener must serve /config.json directly rather than redirecting
// it. A native client needs the cert hash before it can open a WebTransport
// session and has no reason to speak TLS to fetch it; a redirect would send
// it to :443, where a plaintext GET blocks forever waiting for a response
// while the server waits for a ClientHello.
func TestRedirectHandlerServesConfigJSONPlain(t *testing.T) {
	const hash = "deadbeefcafebabe1234567890abcdef0011223344556677889900aabbccddeeff"
	srv := httptest.NewServer(newRedirectHandler(hash))
	defer srv.Close()

	client := &http.Client{
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	resp, err := client.Get(srv.URL + "/config.json")
	if err != nil {
		t.Fatalf("GET /config.json: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("status %d, want 200 (a redirect here hangs a plaintext client)", resp.StatusCode)
	}
	body, _ := io.ReadAll(resp.Body)
	want := `{"certHash":"` + hash + `"}`
	if strings.TrimSpace(string(body)) != want {
		t.Fatalf("body %q, want %q", string(body), want)
	}
}
