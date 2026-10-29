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
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"tailscale.com/client/local"
	"tailscale.com/client/tailscale/apitype"
	"tailscale.com/ipn/ipnstate"
	"tailscale.com/tailcfg"
	"tailscale.com/types/views"
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

// ---- Peer identity lookups (identity.go) ----
//
// The access rule lives in Rust; what is tested here is that Go reports
// identities faithfully and that the web middleware fails closed.

type fakeWhoiser struct {
	resp  *apitype.WhoIsResponse
	err   error
	calls []string
}

func (f *fakeWhoiser) WhoIs(ctx context.Context, addr string) (*apitype.WhoIsResponse, error) {
	f.calls = append(f.calls, addr)
	return f.resp, f.err
}

func TestEncodeIdentityRecord(t *testing.T) {
	got, err := encodeIdentity(peerIdentity{
		userID: 42, loginName: "alice@example.com", nodeName: "laptop.tail.ts.net.",
		tags: []string{"tag:a", "tag:b"},
	})
	if err != nil {
		t.Fatalf("encodeIdentity: %v", err)
	}
	if want := "42\nalice@example.com\nlaptop.tail.ts.net.\ntag:a,tag:b"; got != want {
		t.Fatalf("record = %q, want %q", got, want)
	}
}

// A value that would shift the record's fields must refuse, not be carried:
// Rust would otherwise read a login name out of a node name.
func TestEncodeIdentityRejectsSeparators(t *testing.T) {
	for _, id := range []peerIdentity{
		{loginName: "a\nb"},
		{nodeName: "n\x00"},
		{tags: []string{"tag:a,tag:b"}},
		{tags: []string{""}},
	} {
		if _, err := encodeIdentity(id); err == nil {
			t.Errorf("encodeIdentity(%+v) accepted a separator", id)
		}
	}
}

func TestIdentityFromWhoIsCarriesUserAndTags(t *testing.T) {
	resp := &apitype.WhoIsResponse{
		Node:        &tailcfg.Node{User: 7, Name: "box.ts.net.", Tags: []string{"tag:ops"}},
		UserProfile: &tailcfg.UserProfile{ID: 7, LoginName: "bob@example.com"},
	}
	id, err := identityFromWhoIs(resp)
	if err != nil {
		t.Fatalf("identityFromWhoIs: %v", err)
	}
	if id.userID != 7 || id.loginName != "bob@example.com" || id.nodeName != "box.ts.net." ||
		len(id.tags) != 1 || id.tags[0] != "tag:ops" {
		t.Fatalf("identity = %+v", id)
	}
	if _, err := identityFromWhoIs(&apitype.WhoIsResponse{}); err == nil {
		t.Fatal("a response with no node must be an error")
	}
}

func TestIdentityFromStatusReportsSelfTags(t *testing.T) {
	tags := views.SliceOf([]string{"tag:server"})
	st := &ipnstate.Status{
		Self: &ipnstate.PeerStatus{UserID: 9, DNSName: "gf.ts.net.", Tags: &tags},
		User: map[tailcfg.UserID]tailcfg.UserProfile{9: {ID: 9, LoginName: "owner@example.com"}},
	}
	id, err := identityFromStatus(st)
	if err != nil {
		t.Fatalf("identityFromStatus: %v", err)
	}
	if id.userID != 9 || id.loginName != "owner@example.com" || len(id.tags) != 1 {
		t.Fatalf("identity = %+v", id)
	}
	if _, err := identityFromStatus(&ipnstate.Status{}); err == nil {
		t.Fatal("a status with no self must be an error")
	}
}

func TestLookupPeerMapsOutcomesToStatus(t *testing.T) {
	ok := &fakeWhoiser{resp: &apitype.WhoIsResponse{
		Node: &tailcfg.Node{User: 1}, UserProfile: &tailcfg.UserProfile{LoginName: "a@b"},
	}}
	if id, st := lookupPeer(ok, "100.64.0.2"); st != gbridgeIdentityStatusOK || id.loginName != "a@b" {
		t.Fatalf("found peer: status %d id %+v", st, id)
	}
	if _, st := lookupPeer(&fakeWhoiser{err: local.ErrPeerNotFound}, "x"); st != gbridgeWhoisStatusNotFound {
		t.Fatalf("not-found peer: status %d", st)
	}
	if _, st := lookupPeer(&fakeWhoiser{err: errors.New("boom")}, "x"); st != gbridgeWhoisStatusFailed {
		t.Fatalf("failed lookup: status %d", st)
	}
	if _, st := lookupPeer(&fakeWhoiser{resp: &apitype.WhoIsResponse{}}, "x"); st != gbridgeWhoisStatusFailed {
		t.Fatalf("node-less response: status %d", st)
	}
}

type recordingAuthorizer struct {
	verdict bool
	peers   []string
	status  []int32
}

func (r *recordingAuthorizer) allow(peer string, status int32, _ peerIdentity) bool {
	r.peers = append(r.peers, peer)
	r.status = append(r.status, status)
	return r.verdict
}

const testHash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

func serveConfig(t *testing.T, h http.Handler) (int, string) {
	t.Helper()
	srv := httptest.NewServer(h)
	defer srv.Close()
	resp, err := http.Get(srv.URL + "/config.json")
	if err != nil {
		t.Fatalf("GET: %v", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(body)
}

// No authorizer registered means nobody is served -- not everybody.
func TestMiddlewareRefusesWhenNoAuthorizerRegistered(t *testing.T) {
	who := &fakeWhoiser{resp: &apitype.WhoIsResponse{Node: &tailcfg.Node{User: 1}}}
	h := requireAuthorizedPeer(newRedirectHandler(newCertHashStore(testHash)), who, &authorizerSlot{})
	code, body := serveConfig(t, h)
	if code != http.StatusForbidden {
		t.Fatalf("status %d, want 403", code)
	}
	if strings.Contains(body, testHash) {
		t.Fatal("a refused peer was sent the cert hash")
	}
}

func TestMiddlewareRefusedPeerGetsNoConfig(t *testing.T) {
	who := &fakeWhoiser{err: local.ErrPeerNotFound}
	slot := &authorizerSlot{}
	az := &recordingAuthorizer{verdict: false}
	slot.set(az)
	code, body := serveConfig(t, requireAuthorizedPeer(newRedirectHandler(newCertHashStore(testHash)), who, slot))
	if code != http.StatusForbidden || strings.Contains(body, testHash) {
		t.Fatalf("status %d body %q, want a bare 403", code, body)
	}
	// The authorizer saw the lookup failure, and the lookup was made for the
	// connection's own remote address.
	if len(az.status) != 1 || az.status[0] != gbridgeWhoisStatusNotFound {
		t.Fatalf("authorizer saw statuses %v", az.status)
	}
	if len(who.calls) != 1 || who.calls[0] != az.peers[0] {
		t.Fatalf("WhoIs was asked about %v, authorizer about %v", who.calls, az.peers)
	}
}

func TestMiddlewareAdmittedPeerIsServed(t *testing.T) {
	who := &fakeWhoiser{resp: &apitype.WhoIsResponse{Node: &tailcfg.Node{User: 1}}}
	slot := &authorizerSlot{}
	slot.set(&recordingAuthorizer{verdict: true})
	code, body := serveConfig(t, requireAuthorizedPeer(newRedirectHandler(newCertHashStore(testHash)), who, slot))
	if code != http.StatusOK || !strings.Contains(body, testHash) {
		t.Fatalf("status %d body %q, want 200 with the hash", code, body)
	}
}

// Both listeners -- the SPA and /config.json on :443, the redirect and
// /config.json on :80 -- refuse a peer the authorizer refuses, and every
// peer when none is registered. Holds under both build tags.
func TestBothWebListenersRefuseDisallowedPeers(t *testing.T) {
	who := &fakeWhoiser{resp: &apitype.WhoIsResponse{Node: &tailcfg.Node{User: 1}}}
	denying := &authorizerSlot{}
	denying.set(&recordingAuthorizer{verdict: false})
	for name, slot := range map[string]*authorizerSlot{"none registered": {}, "denied": denying} {
		tlsH, redirH := newWebHandlers(newCertHashStore(testHash), who, slot)
		for label, h := range map[string]http.Handler{":443": tlsH, ":80": redirH} {
			for _, path := range []string{"/config.json", "/", "/index.html"} {
				srv := httptest.NewServer(h)
				resp, err := http.Get(srv.URL + path)
				if err != nil {
					t.Fatalf("GET: %v", err)
				}
				body, _ := io.ReadAll(resp.Body)
				resp.Body.Close()
				srv.Close()
				if resp.StatusCode != http.StatusForbidden || strings.Contains(string(body), testHash) {
					t.Errorf("%s, %s %s: status %d, want a bare 403", name, label, path, resp.StatusCode)
				}
			}
		}
	}
}

// An identity field that cannot cross to Rust intact fails the lookup on
// the web path too, not only in the record gbridge_whois writes.
func TestLookupPeerRejectsFieldsThatCannotCrossIntact(t *testing.T) {
	for _, resp := range []*apitype.WhoIsResponse{
		{Node: &tailcfg.Node{User: 1}, UserProfile: &tailcfg.UserProfile{LoginName: "alice@x\x00evil"}},
		{Node: &tailcfg.Node{User: 1, Tags: []string{"tag:a,tag:b"}}},
		{Node: &tailcfg.Node{User: 1, Tags: []string{"tag:\xff"}}},
	} {
		if _, st := lookupPeer(&fakeWhoiser{resp: resp}, "x"); st != gbridgeWhoisStatusFailed {
			t.Errorf("%+v: status %d, want failed", resp.Node, st)
		}
	}
}

func TestRateLimitedLogSuppressesWithinInterval(t *testing.T) {
	l := &rateLimitedLog{interval: time.Hour}
	if !l.printf("first") {
		t.Fatal("first line must print")
	}
	if l.printf("second") || l.printf("third") {
		t.Fatal("lines within the interval must be suppressed")
	}
	if l.suppressed != 2 {
		t.Fatalf("suppressed = %d, want 2", l.suppressed)
	}
}

// A node whose key expired (or that was removed in the admin console) starts
// in NeedsLogin and tsnet then publishes an auth URL. A non-interactive
// caller (capture without a key) must get an error, not a hang.
func TestUpNonInteractiveFailsOnAuthURL(t *testing.T) {
	f := &fakePoller{statuses: []*ipnstate.Status{{BackendState: "NeedsLogin", AuthURL: "https://login.example/a/1"}}}
	blocked := func(ctx context.Context) error { <-ctx.Done(); return ctx.Err() }
	err := upNonInteractive(blocked, f, time.Millisecond, time.Hour, nil)
	if !errors.Is(err, errNeedsLogin) {
		t.Fatalf("got %v, want errNeedsLogin", err)
	}
}

func TestUpNonInteractiveFailsOnLingeringNeedsLogin(t *testing.T) {
	f := &fakePoller{} // exhausted: NeedsLogin forever, no URL
	blocked := func(ctx context.Context) error { <-ctx.Done(); return ctx.Err() }
	err := upNonInteractive(blocked, f, time.Millisecond, 20*time.Millisecond, nil)
	if !errors.Is(err, errNeedsLogin) {
		t.Fatalf("got %v, want errNeedsLogin", err)
	}
}

// tsnet briefly reports NeedsLogin even with a valid key; a short blip
// followed by Running must succeed.
func TestUpNonInteractiveToleratesABriefNeedsLogin(t *testing.T) {
	f := &fakePoller{statuses: []*ipnstate.Status{{BackendState: "NeedsLogin"}, {BackendState: "Running"}}}
	up := func(ctx context.Context) error {
		time.Sleep(30 * time.Millisecond)
		return nil
	}
	if err := upNonInteractive(up, f, time.Millisecond, time.Hour, nil); err != nil {
		t.Fatalf("got %v, want nil", err)
	}
}

func TestUpNonInteractivePassesUpErrorsThrough(t *testing.T) {
	boom := errors.New("boom")
	f := &fakePoller{statuses: []*ipnstate.Status{{BackendState: "Running"}}}
	err := upNonInteractive(func(context.Context) error { return boom }, f, time.Millisecond, time.Hour, nil)
	if !errors.Is(err, boom) || errors.Is(err, errNeedsLogin) {
		t.Fatalf("got %v", err)
	}
}

// The sign-in URL must never reach the journal, however a tailscale release
// phrases the line: it is redacted by SHAPE, not by the words around it.
func TestRedactingLogfRemovesURLsPassedAsArguments(t *testing.T) {
	var got []string
	next := func(f string, a ...any) { got = append(got, fmt.Sprintf(f, a...)) }
	l := redactingLogf(next)
	for _, url := range []string{
		"https://login.tailscale.com/a/0123456789abc",
		"https://hs.example.net:8443/register/mkey:abcdef",
		"http://127.0.0.1:8080/register/xyz",
	} {
		got = nil
		l("%s", url)
		l("something unrelated: %v (retrying)", url)
		l("no verb at all, url=" + url)
		if len(got) != 3 {
			t.Fatalf("got %q", got)
		}
		for _, line := range got {
			if strings.Contains(line, url) || strings.Contains(line, "/a/0123") || strings.Contains(line, "/register/") {
				t.Fatalf("URL survived in %q", line)
			}
		}
	}
}

func TestRedactingLogfKeepsOrdinaryLines(t *testing.T) {
	var got []string
	next := func(f string, a ...any) { got = append(got, fmt.Sprintf(f, a...)) }
	l := redactingLogf(next)
	l("magicsock: endpoints changed: %s", "1.2.3.4:5")
	l("control server key from %s", "https://controlplane.tailscale.com")
	if len(got) != 2 || got[0] != "magicsock: endpoints changed: 1.2.3.4:5" ||
		got[1] != "control server key from https://controlplane.tailscale.com" {
		t.Fatalf("got %q", got)
	}
}

// Approval pending is distinct from "no URL": say so instead of timing out.
func TestLoginURLReportsMachineAuth(t *testing.T) {
	f := &fakePoller{statuses: []*ipnstate.Status{{BackendState: "NeedsMachineAuth"}}}
	_, err := loginURL(f, time.Second)
	if !errors.Is(err, errMachineAuth) {
		t.Fatalf("got %v, want errMachineAuth", err)
	}
}

func TestUpNonInteractiveHintsOnceWhenWaitingForApproval(t *testing.T) {
	f := &fakePoller{statuses: []*ipnstate.Status{
		{BackendState: "NeedsMachineAuth"}, {BackendState: "NeedsMachineAuth"}, {BackendState: "NeedsMachineAuth"},
	}}
	up := func(ctx context.Context) error { time.Sleep(60 * time.Millisecond); return nil }
	n := 0
	if err := upNonInteractive(up, f, time.Millisecond, time.Hour, func() { n++ }); err != nil {
		t.Fatal(err)
	}
	if n != 1 {
		t.Fatalf("hint called %d times, want 1", n)
	}
}
