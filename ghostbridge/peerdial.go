// SPDX-License-Identifier: MIT OR Apache-2.0

package main

import (
	"context"
	"fmt"
	"net"
	"net/netip"
	"strings"
	"time"

	"tailscale.com/ipn/ipnstate"
)

// peerWaitTimeout bounds how long a dial waits for its destination to appear
// in this node's netmap. The peer list normally lands within a second or two
// of Up(); a destination still missing after this is not on the tailnet (or
// not visible to this node), which is worth a clear error rather than a hang.
const peerWaitTimeout = 15 * time.Second

// peerPollInterval is how often the netmap is re-read while waiting.
const peerPollInterval = 50 * time.Millisecond

// awaitPeerAddr resolves remote ("host:port") to a tailnet peer's IP:port,
// waiting until that peer is in this node's netmap.
//
// Why this exists: tsnet.Server.Up returns as soon as the node is Running with
// an address of its own, which can follow a "lite" map update that carries no
// peers. A Dial issued before the streaming netmap lands finds neither the
// name nor the IP in the netmap, and tsdial.UserDial then silently falls back
// to the OS resolver and an OS socket -- traffic that leaves through the
// host's network instead of this node. On a host that also runs tailscaled
// that happens to reach the peer anyway, from the host's tailnet address; on
// any other host it fails. Which one a session got depended on whether the
// netmap or the dial came first, roughly half the time each.
//
// Dialing the peer's IP once it is known makes the path deterministic: an IP
// that is a known peer always takes tsnet's own netstack.
func awaitPeerAddr(ctx context.Context, status func(context.Context) (*ipnstate.Status, error), remote string, timeout, poll time.Duration) (string, error) {
	host, port, err := net.SplitHostPort(remote)
	if err != nil {
		return "", err
	}
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	var lastErr error
	for {
		st, err := status(ctx)
		if err == nil {
			if ip, ok := peerAddrFor(st, host); ok {
				return net.JoinHostPort(ip.String(), port), nil
			}
		} else {
			lastErr = err
		}
		select {
		case <-ctx.Done():
			if lastErr != nil {
				return "", fmt.Errorf("tailnet peer %q not found within %v (last status error: %v)", host, timeout, lastErr)
			}
			return "", fmt.Errorf("tailnet peer %q not found in this node's netmap within %v", host, timeout)
		case <-time.After(poll):
		}
	}
}

// peerAddrFor finds host among st's peers and returns its tailnet address,
// IPv4 preferred. host may be a peer IP, its MagicDNS name (with or without
// the tailnet suffix or trailing dot), or its OS hostname. A MagicDNS match
// wins over a hostname match: MagicDNS names are unique on a tailnet,
// hostnames are not (they get a -1, -2... suffix in MagicDNS when they clash).
func peerAddrFor(st *ipnstate.Status, host string) (netip.Addr, bool) {
	if st == nil {
		return netip.Addr{}, false
	}
	want := strings.ToLower(strings.TrimSuffix(host, "."))
	if ip, err := netip.ParseAddr(want); err == nil {
		for _, p := range st.Peer {
			for _, a := range p.TailscaleIPs {
				if a == ip {
					return a, true
				}
			}
		}
		return netip.Addr{}, false
	}
	var byHostName *ipnstate.PeerStatus
	for _, p := range st.Peer {
		dns := strings.ToLower(strings.TrimSuffix(p.DNSName, "."))
		if dns != "" && (dns == want || strings.SplitN(dns, ".", 2)[0] == want) {
			if ip, ok := preferV4(p.TailscaleIPs); ok {
				return ip, true
			}
		}
		if byHostName == nil && strings.EqualFold(p.HostName, want) {
			byHostName = p
		}
	}
	if byHostName != nil {
		return preferV4(byHostName.TailscaleIPs)
	}
	return netip.Addr{}, false
}

func preferV4(ips []netip.Addr) (netip.Addr, bool) {
	for _, a := range ips {
		if a.Is4() {
			return a, true
		}
	}
	if len(ips) > 0 {
		return ips[0], true
	}
	return netip.Addr{}, false
}

// isLocalTailnetAddr reports whether addr (a net.Conn's LocalAddr) is one of
// this node's own tailnet IPs -- i.e. the dial really went through tsnet.
func isLocalTailnetAddr(addr net.Addr, mine []netip.Addr) bool {
	ap, err := netip.ParseAddrPort(addr.String())
	if err != nil {
		return false
	}
	for _, m := range mine {
		if ap.Addr().Unmap() == m {
			return true
		}
	}
	return false
}
