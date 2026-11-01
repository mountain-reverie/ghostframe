package main

import (
	"bytes"
	"encoding/binary"
	"net"
	"strconv"
	"syscall"
	"testing"
	"time"
)

// legacyFrame is the frame the bridge used to build for a datagram: the
// source's string form split back into host and port. The reusing framer must
// produce exactly these bytes, since the Rust side parses them.
func legacyFrame(t *testing.T, payload []byte, src net.Addr) []byte {
	t.Helper()
	host, port, err := net.SplitHostPort(src.String())
	if err != nil {
		t.Fatal(err)
	}
	p, _ := strconv.Atoi(port)
	hostBytes := []byte(host)
	total := 8 + len(payload) + 2 + len(hostBytes) + 1
	frame := make([]byte, total)
	binary.BigEndian.PutUint32(frame[0:4], uint32(total))
	binary.BigEndian.PutUint32(frame[4:8], uint32(len(payload)))
	copy(frame[8:], payload)
	binary.BigEndian.PutUint16(frame[8+len(payload):], uint16(p))
	copy(frame[8+len(payload)+2:], hostBytes)
	return frame
}

func udp(s string) *net.UDPAddr {
	a, err := net.ResolveUDPAddr("udp", s)
	if err != nil {
		panic(err)
	}
	return a
}

func TestOutFramerMatchesTheLegacyFrameForEveryAddressShape(t *testing.T) {
	srcs := []net.Addr{
		udp("100.90.101.40:443"),
		udp("[fd7a:115c:a1e0::1]:443"),
		udp("[fe80::1%lo]:9"),
		// A 4-in-6 address must still come out as IPv4, as String() prints it.
		&net.UDPAddr{IP: net.ParseIP("100.64.0.7").To16(), Port: 1},
		// Not a *net.UDPAddr: takes the string path.
		&net.TCPAddr{IP: net.ParseIP("100.64.0.9"), Port: 2},
		// Back to the first, so a stale cached tail would show.
		udp("100.90.101.40:443"),
	}
	f := newOutFramer()
	for i, src := range srcs {
		payload := bytes.Repeat([]byte{byte(i + 1)}, 100*(len(srcs)-i))
		n := copy(f.payloadBuf(), payload)
		got, ok := f.frame(n, src)
		if !ok {
			t.Fatalf("%v: dropped", src)
		}
		if want := legacyFrame(t, payload, src); !bytes.Equal(got, want) {
			t.Fatalf("%v:\n got %x\nwant %x", src, got, want)
		}
	}
}

func TestOutFramerDropsASourceWithNoIPPort(t *testing.T) {
	f := newOutFramer()
	if _, ok := f.frame(1, &net.UnixAddr{Name: "/x", Net: "unix"}); ok {
		t.Fatal("a unix address has no IP:port and must be dropped")
	}
	if _, ok := f.frame(1, nil); ok {
		t.Fatal("a nil address must be dropped")
	}
}

func TestOutFramerGrowsForAnAddressLongerThanTheSlack(t *testing.T) {
	src := &net.UDPAddr{IP: net.ParseIP("fe80::1"), Port: 7, Zone: string(bytes.Repeat([]byte{'z'}, 100))}
	f := newOutFramer()
	payload := bytes.Repeat([]byte{9}, maxDatagram)
	n := copy(f.payloadBuf(), payload)
	got, ok := f.frame(n, src)
	if !ok || !bytes.Equal(got, legacyFrame(t, payload, src)) {
		t.Fatal("a maximum datagram with a long zone was not framed intact")
	}
}

func TestInFramerResolvesOnlyWhenTheDestinationChanges(t *testing.T) {
	var in inFramer
	body := func(payload []byte, dst string) ([]byte, uint32) {
		frame := legacyFrame(t, payload, udp(dst))
		b := in.bodyBuf(binary.BigEndian.Uint32(frame[0:4]))
		copy(b, frame[8:])
		return b, uint32(len(payload))
	}
	b, n := body([]byte("first"), "100.64.0.1:443")
	p, d1, ok := in.parse(b, n)
	if !ok || string(p) != "first" || d1.String() != "100.64.0.1:443" {
		t.Fatalf("got %q %v %v", p, d1, ok)
	}
	b, n = body([]byte("second, longer"), "100.64.0.1:443")
	p, d2, _ := in.parse(b, n)
	if string(p) != "second, longer" || d2 != d1 {
		t.Fatalf("same destination must reuse the resolved address: %q %p %p", p, d1, d2)
	}
	b, n = body([]byte("v6"), "[fd7a::2]:9")
	p, d3, _ := in.parse(b, n)
	if string(p) != "v6" || d3.String() != "[fd7a::2]:9" {
		t.Fatalf("a new destination must be resolved: %q %v", p, d3)
	}
	if d1.String() != "100.64.0.1:443" {
		t.Fatal("the shared address of an earlier destination was modified")
	}
}

func TestFramersDoNotAllocatePerPacket(t *testing.T) {
	src := udp("100.90.101.40:443")
	out := newOutFramer()
	if a := testing.AllocsPerRun(1000, func() { out.frame(1200, src) }); a != 0 {
		t.Errorf("outFramer.frame: %v allocations per packet, want 0", a)
	}
	var in inFramer
	frame := legacyFrame(t, make([]byte, 1200), src)
	total := binary.BigEndian.Uint32(frame[0:4])
	if a := testing.AllocsPerRun(1000, func() {
		b := in.bodyBuf(total)
		copy(b, frame[8:])
		in.parse(b, 1200)
	}); a != 0 {
		t.Errorf("inFramer: %v allocations per packet, want 0", a)
	}
}

// fakePacketConn hands out queued datagrams and records what is written.
type fakePacketConn struct {
	reads  chan []byte
	src    net.Addr
	writes chan string
}

func (c *fakePacketConn) ReadFrom(p []byte) (int, net.Addr, error) {
	b, ok := <-c.reads
	if !ok {
		return 0, nil, net.ErrClosed
	}
	return copy(p, b), c.src, nil
}

func (c *fakePacketConn) WriteTo(p []byte, a net.Addr) (int, error) {
	c.writes <- string(p) + " -> " + a.String()
	return len(p), nil
}

func (c *fakePacketConn) Close() error { return nil }

func TestPacketBridgeRelaysBothWaysOverASocketpair(t *testing.T) {
	goFd, cFd, err := makeSocketpair()
	if err != nil {
		t.Fatal(err)
	}
	defer syscall.Close(cFd)
	if err := syscall.SetNonblock(cFd, false); err != nil {
		t.Fatal(err)
	}
	conn := &fakePacketConn{reads: make(chan []byte, 4), src: udp("100.64.0.2:443"), writes: make(chan string, 4)}
	spawnPacketBridge(conn, goFd)

	// tsnet -> rust: two datagrams, the second shorter so a stale tail shows.
	for _, p := range []string{"a longer first datagram", "short"} {
		conn.reads <- []byte(p)
		want := legacyFrame(t, []byte(p), conn.src)
		got := make([]byte, len(want))
		if _, err := readFull(cFd, got); err != nil {
			t.Fatal(err)
		}
		if !bytes.Equal(got, want) {
			t.Fatalf("relayed frame\n got %x\nwant %x", got, want)
		}
	}

	// rust -> tsnet: two packets to two destinations, back to back.
	var frames []byte
	frames = append(frames, legacyFrame(t, []byte("to v4"), udp("100.64.0.3:1"))...)
	frames = append(frames, legacyFrame(t, []byte("to v6"), udp("[fd7a::3]:2"))...)
	if _, err := writeAll(cFd, frames); err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"to v4 -> 100.64.0.3:1", "to v6 -> [fd7a::3]:2"} {
		select {
		case got := <-conn.writes:
			if got != want {
				t.Fatalf("got %q want %q", got, want)
			}
		case <-time.After(5 * time.Second):
			t.Fatalf("timed out waiting for %q", want)
		}
	}
	close(conn.reads)
}
