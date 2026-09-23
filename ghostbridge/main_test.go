package main

// NOTE: these tests exercise the plain-Go helpers behind gbridge_login_url
// and gbridge_logout, not the cgo exports themselves.
//
// `go test` rejects `import "C"` in ANY _test.go file, in any package --
// it is not conditional on this package being cgo-exported or built
// -buildmode=c-archive, so do not "fix" it by changing either. The error
// reads: "use of cgo in test main_test.go not supported".
//
// The cgo wrappers are therefore kept deliberately thin -- marshal C types,
// call the Go helper, marshal the result back into the caller's buffer --
// so that untestable layer holds no decisions.

import (
	"context"
	"errors"
	"testing"
	"time"

	"tailscale.com/ipn/ipnstate"
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

// fakePoller replays a scripted sequence of statuses, so the three outcomes
// of loginURL can be exercised without a tailnet.
type fakePoller struct {
	statuses []*ipnstate.Status
	calls    int
}

func (f *fakePoller) StatusWithoutPeers(ctx context.Context) (*ipnstate.Status, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if f.calls < len(f.statuses) {
		st := f.statuses[f.calls]
		f.calls++
		return st, nil
	}
	// Exhausted: keep reporting "not yet authorised" so the caller polls on.
	return &ipnstate.Status{BackendState: "NeedsLogin"}, nil
}

// An already-authorised node must return an empty URL and success
// IMMEDIATELY. Rust turns the empty string into Option::None; a regression
// that made this wait out the poll timeout would look like a hung `login`.
func TestLoginURLAlreadyAuthorisedReturnsEmptyImmediately(t *testing.T) {
	f := &fakePoller{statuses: []*ipnstate.Status{{BackendState: "Running"}}}
	start := time.Now()
	url, err := loginURL(f, loginPollTimeout)
	if err != nil {
		t.Fatalf("loginURL: %v", err)
	}
	if url != "" {
		t.Fatalf("url = %q, want empty for an authorised node", url)
	}
	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Fatalf("took %v; an authorised node must not wait out the poll timeout", elapsed)
	}
}

// A published AuthURL wins, and is returned even though the backend is not
// yet Running -- that is the whole point of polling rather than using Up().
func TestLoginURLReturnsAuthURLOncePublished(t *testing.T) {
	const want = "https://login.tailscale.com/a/deadbeef"
	f := &fakePoller{statuses: []*ipnstate.Status{
		{BackendState: "NeedsLogin"},
		{BackendState: "NeedsLogin", AuthURL: want},
	}}
	got, err := loginURL(f, loginPollTimeout)
	if err != nil {
		t.Fatalf("loginURL: %v", err)
	}
	if got != want {
		t.Fatalf("url = %q, want %q", got, want)
	}
}

// Timing out must be distinguishable from every other failure, because the
// C shim maps it to its own status code.
func TestLoginURLTimesOutDistinctly(t *testing.T) {
	f := &fakePoller{} // never authorises, never publishes a URL
	_, err := loginURL(f, 300*time.Millisecond)
	if !errors.Is(err, errLoginTimeout) {
		t.Fatalf("err = %v, want errLoginTimeout", err)
	}
}
