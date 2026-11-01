package main

import (
	"bytes"
	"encoding/binary"
	"net"
	"net/netip"
	"strconv"
)

// Framing for the packet bridge, kept free of per-packet allocations.
//
// Frame layout: [total_len(4)][payload_len(4)][payload][port(2)][host\0]
// total_len is the full frame size including the first 4 bytes.
//
// Every QUIC datagram of a session crosses this code, and on a slow client
// the Go runtime (allocation, GC, scheduling) is the largest single CPU cost
// in the process. The address part of a frame is the same for every packet of
// a dialed connection and for long runs of packets on a listener, so both
// directions cache it instead of formatting and re-parsing it each time.

// maxDatagram is the largest UDP payload a frame can carry.
const maxDatagram = 65535

// frameAddrSlack is room reserved after the payload for [port(2)][host\0]: the
// longest IP literal is 45 bytes (an IPv4-mapped IPv6 address), plus a zone.
const frameAddrSlack = 2 + 64 + 1

// outFramer builds tsnet -> rust frames in one reused buffer. The caller reads
// a datagram straight into payloadBuf(), then calls frame to append the
// address and fill the header, so the payload is never copied.
type outFramer struct {
	buf []byte

	// The address tail ([port(2)][host\0]) of the last source, and that source.
	lastSrc  netip.AddrPort
	lastTail []byte
}

func newOutFramer() *outFramer {
	return &outFramer{buf: make([]byte, 8+maxDatagram+frameAddrSlack)}
}

// payloadBuf is where the next datagram is read to.
func (f *outFramer) payloadBuf() []byte {
	return f.buf[8 : 8+maxDatagram]
}

// frame finishes the frame for an n-byte datagram from src and returns it. The
// slice is only valid until the next call. ok is false for a source that has no
// IP:port form, which the old string-based code also dropped.
func (f *outFramer) frame(n int, src net.Addr) (frame []byte, ok bool) {
	ap, ok := addrPortOf(src)
	if !ok {
		return nil, false
	}
	if ap != f.lastSrc || f.lastTail == nil {
		f.lastSrc = ap
		f.lastTail = appendAddrTail(f.lastTail[:0], ap)
	}
	total := 8 + n + len(f.lastTail)
	if total > len(f.buf) {
		// Only an address longer than frameAddrSlack gets here.
		grown := make([]byte, total)
		copy(grown, f.buf[:8+n])
		f.buf = grown
	}
	binary.BigEndian.PutUint32(f.buf[0:4], uint32(total))
	binary.BigEndian.PutUint32(f.buf[4:8], uint32(n))
	copy(f.buf[8+n:], f.lastTail)
	return f.buf[:total], true
}

// addrPortOf extracts IP:port from a datagram's source without going through
// its string form when it is a *net.UDPAddr (both tsnet's listener and a
// dialed connection hand those back).
func addrPortOf(a net.Addr) (netip.AddrPort, bool) {
	if ua, ok := a.(*net.UDPAddr); ok {
		ap := ua.AddrPort()
		// net.UDPAddr.String prints a 4-in-6 address as plain IPv4; so did the
		// string-based framing, and the Rust side relies on getting IPv4 back.
		return netip.AddrPortFrom(ap.Addr().Unmap(), ap.Port()), ap.IsValid()
	}
	if a == nil {
		return netip.AddrPort{}, false
	}
	host, port, err := net.SplitHostPort(a.String())
	if err != nil {
		return netip.AddrPort{}, false
	}
	ip, err := netip.ParseAddr(host)
	if err != nil {
		return netip.AddrPort{}, false
	}
	p, err := strconv.ParseUint(port, 10, 16)
	if err != nil {
		return netip.AddrPort{}, false
	}
	return netip.AddrPortFrom(ip, uint16(p)), true
}

func appendAddrTail(b []byte, ap netip.AddrPort) []byte {
	b = binary.BigEndian.AppendUint16(b, ap.Port())
	b = ap.Addr().AppendTo(b)
	return append(b, 0)
}

// inFramer decodes rust -> tsnet frames. It reuses one body buffer and keeps
// the destination of the last frame, so a run of packets to the same peer
// resolves its address once.
type inFramer struct {
	body []byte

	lastTail []byte // [port(2)][host] of lastDst, without the NUL
	lastDst  *net.UDPAddr
}

// bodyBuf returns a buffer for the totalLen-8 bytes that follow a header,
// valid until the next call.
func (f *inFramer) bodyBuf(totalLen uint32) []byte {
	n := int(totalLen) - 8
	if cap(f.body) < n {
		f.body = make([]byte, n)
	}
	return f.body[:n]
}

// parse splits a body into payload and destination. ok is false for a body
// whose address does not resolve, which the bridge skips. The returned
// *net.UDPAddr is shared between calls: callers must not modify it.
func (f *inFramer) parse(body []byte, payloadLen uint32) (payload []byte, dst *net.UDPAddr, ok bool) {
	payload = body[:payloadLen]
	tail := body[payloadLen:]
	// Trim the trailing null terminator (and anything after it).
	if i := bytes.IndexByte(tail[2:], 0); i >= 0 {
		tail = tail[:2+i]
	}
	if f.lastDst != nil && bytes.Equal(tail, f.lastTail) {
		return payload, f.lastDst, true
	}
	port := binary.BigEndian.Uint16(tail[0:2])
	d, err := net.ResolveUDPAddr("udp", net.JoinHostPort(string(tail[2:]), strconv.Itoa(int(port))))
	if err != nil {
		return nil, nil, false
	}
	f.lastTail = append(f.lastTail[:0], tail...)
	f.lastDst = d
	return payload, d, true
}
