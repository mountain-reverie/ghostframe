package main

// NOTE: these tests exercise the plain-Go helpers behind gbridge_login_url
// and gbridge_logout (loginURLForHandle / logoutForHandle), not the cgo
// exports themselves. `go test` on this package fails with "use of cgo in
// test main_test.go not supported" the moment a _test.go file adds its own
// `import "C"` — cgo-exported packages (as main.go is here, via
// -buildmode=c-archive) cannot carry cgo in test files at all. The cgo
// wrappers are kept intentionally thin (marshal C types, call the Go
// helper, marshal the result back into the caller's buffer) so that thin
// layer is the only thing not covered by these tests.

import (
	"testing"
	"time"
)

// A login URL cannot be produced for a session that was never created.
func TestLoginURLRejectsUnknownHandle(t *testing.T) {
	if _, err := loginURLForHandle(9999, time.Second); err == nil {
		t.Fatal("expected error for unknown handle, got nil")
	}
}

func TestLogoutRejectsUnknownHandle(t *testing.T) {
	if err := logoutForHandle(9999, time.Second); err == nil {
		t.Fatal("expected error for unknown handle, got nil")
	}
}
