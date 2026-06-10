package main

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

func TestServeIndex(t *testing.T) {
	mux := newWebMux("deadbeef")
	srv := httptest.NewServer(mux)
	defer srv.Close()

	resp, err := http.Get(srv.URL + "/")
	if err != nil {
		t.Fatalf("GET /: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("GET /: status %d, want 200", resp.StatusCode)
	}
	body, _ := io.ReadAll(resp.Body)
	if !strings.Contains(string(body), "<html") {
		preview := body
		if len(preview) > 80 {
			preview = preview[:80]
		}
		t.Fatalf("GET /: body %q does not look like HTML", string(preview))
	}
}

func TestServeConfigJSON(t *testing.T) {
	mux := newWebMux("deadbeefcafebabe1234567890abcdef0011223344556677889900aabbccddeeff")
	srv := httptest.NewServer(mux)
	defer srv.Close()

	resp, err := http.Get(srv.URL + "/config.json")
	if err != nil {
		t.Fatalf("GET /config.json: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("GET /config.json: status %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); !strings.HasPrefix(ct, "application/json") {
		t.Fatalf("GET /config.json: content-type %q, want application/json", ct)
	}
	body, _ := io.ReadAll(resp.Body)
	want := `{"certHash":"deadbeefcafebabe1234567890abcdef0011223344556677889900aabbccddeeff"}`
	if strings.TrimSpace(string(body)) != want {
		t.Fatalf("GET /config.json: body %q, want %q", string(body), want)
	}
	if hsts := resp.Header.Get("Strict-Transport-Security"); !strings.Contains(hsts, "max-age=") {
		t.Fatalf("GET /config.json: HSTS header %q missing max-age=", hsts)
	}
}

func TestRedirectHandler(t *testing.T) {
	h := newRedirectHandler()
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
