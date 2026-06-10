package main

import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
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
}
