// SPDX-License-Identifier: MIT OR Apache-2.0

package main

import (
	"context"
	"net"
	"net/netip"
	"strings"
	"testing"
	"time"

	"tailscale.com/ipn/ipnstate"
	"tailscale.com/types/key"
)

func peer(host, dns string, ips ...string) *ipnstate.PeerStatus {
	p := &ipnstate.PeerStatus{HostName: host, DNSName: dns}
	for _, s := range ips {
		p.TailscaleIPs = append(p.TailscaleIPs, netip.MustParseAddr(s))
	}
	return p
}

func statusWith(peers ...*ipnstate.PeerStatus) *ipnstate.Status {
	st := &ipnstate.Status{Peer: map[key.NodePublic]*ipnstate.PeerStatus{}}
	for _, p := range peers {
		st.Peer[key.NewNode().Public()] = p
	}
	return st
}

func TestPeerAddrForMatchesEveryWayAPeerIsNamed(t *testing.T) {
	// The real shape: OS hostname "evangeline-ghostframe", MagicDNS name
	// "evangeline-ghostframe-1" because the hostname clashed.
	server := peer("evangeline-ghostframe", "evangeline-ghostframe-1.tail0943d.ts.net.", "fd7a:115c:a1e0::1", "100.90.101.40")
	other := peer("evangeline", "evangeline.tail0943d.ts.net.", "100.122.126.124")
	st := statusWith(server, other)

	for _, host := range []string{
		"evangeline-ghostframe-1",
		"evangeline-ghostframe-1.tail0943d.ts.net",
		"evangeline-ghostframe-1.tail0943d.ts.net.",
		"EVANGELINE-GHOSTFRAME-1",
		"evangeline-ghostframe", // hostname fallback
		"100.90.101.40",
	} {
		ip, ok := peerAddrFor(st, host)
		if !ok || ip != netip.MustParseAddr("100.90.101.40") {
			t.Errorf("%q: got %v %v, want 100.90.101.40 (IPv4 preferred)", host, ip, ok)
		}
	}
	if ip, ok := peerAddrFor(st, "fd7a:115c:a1e0::1"); !ok || !ip.Is6() {
		t.Errorf("an IPv6 peer address must match itself, got %v %v", ip, ok)
	}
	for _, host := range []string{"nope", "100.64.0.9", "evangeline-ghost"} {
		if ip, ok := peerAddrFor(st, host); ok {
			t.Errorf("%q must not match, got %v", host, ip)
		}
	}
}

func TestPeerAddrForPrefersMagicDNSOverAClashingHostname(t *testing.T) {
	// Two machines both called "box": MagicDNS names them box and box-1. Asking
	// for "box" must reach the one whose MagicDNS name is box, whichever peer
	// the map happens to iterate first.
	a := peer("box", "box-1.tailnet.ts.net.", "100.64.0.1")
	b := peer("other", "box.tailnet.ts.net.", "100.64.0.2")
	for i := 0; i < 50; i++ {
		ip, ok := peerAddrFor(statusWith(a, b), "box")
		if !ok || ip != netip.MustParseAddr("100.64.0.2") {
			t.Fatalf("got %v %v, want the MagicDNS match 100.64.0.2", ip, ok)
		}
	}
}

func TestAwaitPeerAddrWaitsForTheNetmapToListThePeer(t *testing.T) {
	// The race this exists for: Up returned, but the first statuses carry no
	// peers yet.
	calls := 0
	status := func(context.Context) (*ipnstate.Status, error) {
		calls++
		if calls < 4 {
			return statusWith(), nil
		}
		return statusWith(peer("srv", "srv.t.ts.net.", "100.64.0.7")), nil
	}
	addr, err := awaitPeerAddr(context.Background(), status, "srv:443", time.Second, time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}
	if addr != "100.64.0.7:443" {
		t.Fatalf("got %q, want the peer's IP with the original port", addr)
	}
	if calls < 4 {
		t.Fatalf("returned after %d status reads, before the peer was listed", calls)
	}
}

func TestAwaitPeerAddrGivesUpWithAClearError(t *testing.T) {
	status := func(context.Context) (*ipnstate.Status, error) { return statusWith(), nil }
	start := time.Now()
	_, err := awaitPeerAddr(context.Background(), status, "ghost:443", 30*time.Millisecond, time.Millisecond)
	if err == nil || !strings.Contains(err.Error(), `"ghost"`) {
		t.Fatalf("want an error naming the missing peer, got %v", err)
	}
	if time.Since(start) > 2*time.Second {
		t.Fatalf("did not honour the timeout")
	}
}

func TestIsLocalTailnetAddr(t *testing.T) {
	mine := []netip.Addr{netip.MustParseAddr("100.113.138.40"), netip.MustParseAddr("fd7a:115c:a1e0::7437:8a29")}
	cases := map[string]bool{
		"100.113.138.40:34906":            true,
		"[fd7a:115c:a1e0::7437:8a29]:443": true,
		"100.116.173.116:59884":           false, // the host's tailscaled: the fallback
	}
	for s, want := range cases {
		addr, err := net.ResolveUDPAddr("udp", s)
		if err != nil {
			t.Fatal(err)
		}
		if got := isLocalTailnetAddr(addr, mine); got != want {
			t.Errorf("%s: got %v want %v", s, got, want)
		}
	}
}
