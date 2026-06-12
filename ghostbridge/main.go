package main

/*
#include <stdint.h>
#include <string.h>
typedef int32_t gbridge_status;
*/
import "C"

import (
	"context"
	"encoding/binary"
	"io"
	"log"
	"net"
	"os"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"unsafe"

	"tailscale.com/envknob"
	"tailscale.com/net/netns"
	"tailscale.com/tsnet"
)

func init() {
	// TS_DEBUG_USE_DERP_HTTP=1 makes the tailscale DERP client connect to the
	// DERP relay server using HTTP instead of HTTPS. This is required when
	// using headscale's embedded DERP without TLS (http:// server_url) — i.e.
	// the e2e harness — but it breaks the public Tailscale network, whose
	// DERP relays only accept HTTPS. Forcing it on unconditionally caused
	// every production node to silently time out on all DERP regions,
	// leaving WireGuard with no relay path and browser HTTPS connections
	// stalling out as NS_ERROR_NET_INTERRUPT.
	//
	// Heuristic: a non-empty TS_CONTROL_URL means the operator is pointing
	// us at a custom control plane (headscale). An empty TS_CONTROL_URL
	// means we're joining the public Tailscale network and must use HTTPS
	// DERP.
	//
	// envknob.Setenv is required (not plain os.Setenv) because tailscale
	// packages register their env knobs at package-init time; the
	// already-registered pointer needs to be updated.
	if explicit := os.Getenv("TS_DEBUG_USE_DERP_HTTP"); explicit != "" {
		log.Printf("ghostbridge: init: TS_DEBUG_USE_DERP_HTTP already set to %q", explicit)
	} else if os.Getenv("TS_CONTROL_URL") != "" {
		log.Printf("ghostbridge: init: TS_CONTROL_URL is set, opting into TS_DEBUG_USE_DERP_HTTP=1 (headscale path)")
		envknob.Setenv("TS_DEBUG_USE_DERP_HTTP", "1")
	} else {
		log.Printf("ghostbridge: init: production path (public Tailscale) — leaving TS_DEBUG_USE_DERP_HTTP unset for HTTPS DERP")
	}

	// Disable netns socket marking (SO_MARK / SO_BINDTODEVICE).
	//
	// In a normal tailscaled installation, netns marks outbound sockets so
	// that DERP and control-plane traffic bypasses the TUN and goes out the
	// real network interface. In a userspace-only tsnet node (our case),
	// there is no TUN, so the routing-loop protection is unnecessary — and
	// it actively breaks things when DERP is behind Docker port-mapping
	// (the bind-to-device selects the wrong interface). tsnet's own test
	// suite disables netns for exactly this reason.
	netns.SetEnabled(false)
	log.Printf("ghostbridge: init: netns socket marking disabled")
}

var (
	servers    = make(map[int32]*serverHandle)
	serversMu  sync.Mutex
	nextHandle int32 = 1
)

type serverHandle struct {
	server *tsnet.Server
}

// setNonblock sets O_NONBLOCK on a raw fd. The Rust side wraps this fd in
// tokio::net::UnixStream, which requires non-blocking mode.
func setNonblock(fd int) error {
	return syscall.SetNonblock(fd, true)
}

//export gbridge_new
func gbridge_new(cHostname, cAuthkey, cStateDir, cControlURL *C.char, sdOut *C.int32_t) C.gbridge_status {
	hostname := C.GoString(cHostname)
	authkey := C.GoString(cAuthkey)
	stateDir := C.GoString(cStateDir)
	controlURL := C.GoString(cControlURL)

	s := &tsnet.Server{
		Hostname: hostname,
		AuthKey:  authkey,
		Dir:      stateDir,
		// Route tsnet's internal logger to log.Printf so cert-provisioning
		// failures (ACME DNS-01 errors, LocalAPI cert-fetch timeouts, etc.)
		// surface in the daemon's journal instead of being swallowed.
		// Without this, an SSL handshake failure on :443 is silent from the
		// operator's perspective and looks like a daemon bug rather than a
		// tailnet/cert provisioning issue.
		Logf: log.Printf,
	}
	if controlURL != "" {
		s.ControlURL = controlURL
	}

	serversMu.Lock()
	id := nextHandle
	nextHandle++
	servers[id] = &serverHandle{server: s}
	serversMu.Unlock()

	*sdOut = C.int32_t(id)
	return 0
}

//export gbridge_up
func gbridge_up(sd C.int32_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	log.Printf("ghostbridge: gbridge_up: calling server.Up()")
	st, err := h.server.Up(context.Background())
	if err != nil {
		log.Printf("ghostbridge: gbridge_up: server.Up() error: %v", err)
		return -2
	}
	ips := st.TailscaleIPs
	log.Printf("ghostbridge: gbridge_up: server.Up() done, state=%s ips=%v", st.BackendState, ips)
	return 0
}

//export gbridge_listen_udp
func gbridge_listen_udp(sd C.int32_t, cAddr *C.char, fdOut *C.int32_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	addr := C.GoString(cAddr)
	conn, err := h.server.ListenPacket("udp", addr)
	if err != nil {
		log.Printf("ghostbridge: ListenPacket(udp, %q) failed: %v", addr, err)
		return -3
	}
	goFd, cFd, err := makeSocketpair()
	if err != nil {
		conn.Close()
		return -4
	}
	spawnPacketBridge(conn, goFd)
	*fdOut = C.int32_t(cFd)
	return 0
}

//export gbridge_dial_udp
func gbridge_dial_udp(sd C.int32_t, cRemote *C.char, fdOut *C.int32_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	remote := C.GoString(cRemote)
	log.Printf("ghostbridge: dialing udp %q", remote)
	// tsnet.Server.Dial returns a net.Conn; for UDP it wraps the packet conn.
	netConn, err := h.server.Dial(context.Background(), "udp", remote)
	if err != nil {
		log.Printf("ghostbridge: dial udp %q failed: %v", remote, err)
		return -3
	}
	log.Printf("ghostbridge: dial udp %q succeeded, remote=%s local=%s", remote, netConn.RemoteAddr(), netConn.LocalAddr())
	// Wrap as PacketConn-like: reads come from this one peer, writes go to it.
	pc := &dialedPacketConn{Conn: netConn}
	goFd, cFd, err := makeSocketpair()
	if err != nil {
		netConn.Close()
		return -4
	}
	spawnPacketBridge(pc, goFd)
	*fdOut = C.int32_t(cFd)
	return 0
}

//export gbridge_dial_tcp
func gbridge_dial_tcp(sd C.int32_t, cRemote *C.char, fdOut *C.int32_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	remote := C.GoString(cRemote)
	log.Printf("ghostbridge: dialing tcp %q", remote)
	netConn, err := h.server.Dial(context.Background(), "tcp", remote)
	if err != nil {
		log.Printf("ghostbridge: dial tcp %q failed: %v", remote, err)
		return -3
	}
	goFd, cFd, err := makeSocketpair()
	if err != nil {
		netConn.Close()
		return -4
	}
	spawnTcpBridge(netConn, goFd)
	*fdOut = C.int32_t(cFd)
	return 0
}

// spawnTcpBridge copies bytes bidirectionally between a tsnet TCP
// connection and the Go-side socketpair fd. Mirrors spawnPacketBridge
// but without the per-packet framing — TCP is a byte stream.
//
// Both directions defer closing BOTH endpoints. Closing one half of a
// socketpair (or the tsnet net.Conn) unblocks the other goroutine's
// blocking io.Copy with EOF / EPIPE, so the surviving goroutine exits
// promptly and runs its own defers (idempotent Close()). Without
// symmetric closes, a remote-initiated close on conn would leave goFile
// open and the Rust-side forwarder would hang on read forever.
func spawnTcpBridge(conn net.Conn, goFd int) {
	goFile := os.NewFile(uintptr(goFd), "gbridge-tcp-goside")
	go func() {
		defer goFile.Close()
		defer conn.Close()
		_, _ = io.Copy(conn, goFile)
	}()
	go func() {
		defer goFile.Close()
		defer conn.Close()
		_, _ = io.Copy(goFile, conn)
	}()
}

//export gbridge_close
func gbridge_close(sd C.int32_t) C.gbridge_status {
	serversMu.Lock()
	h, ok := servers[int32(sd)]
	if ok {
		delete(servers, int32(sd))
	}
	serversMu.Unlock()
	if !ok {
		return -1
	}
	h.server.Close()
	return 0
}

// Status codes for the web server start path. Negative-numbered to match
// the existing convention used by gbridge_new / gbridge_up / etc.
const (
	gbridgeWebStatusOK                 = 0
	gbridgeWebStatusInvalidHandle      = -1
	gbridgeWebStatusInvalidArg         = -20
	gbridgeWebStatusHTTPSCertsDisabled = -21
	gbridgeWebStatusListenFailed       = -22
)

//export gbridge_start_web_server
func gbridge_start_web_server(sd C.int32_t, cCertHashHex *C.char) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return gbridgeWebStatusInvalidHandle
	}
	certHash := C.GoString(cCertHashHex)
	if len(certHash) != 64 {
		log.Printf("ghostbridge: gbridge_start_web_server: certHash length %d, want 64", len(certHash))
		return gbridgeWebStatusInvalidArg
	}

	staticCert, err := loadStaticCertFromEnv()
	if err != nil {
		log.Printf("ghostbridge: gbridge_start_web_server: static cert env vars set but invalid: %v", err)
		return gbridgeWebStatusInvalidArg
	}

	if staticCert == nil {
		// Production: HTTPS Certs must be enabled in the tailnet.
		domains := h.server.CertDomains()
		if len(domains) == 0 {
			log.Printf("ghostbridge: gbridge_start_web_server: tailnet has no HTTPS-eligible domains")
			log.Printf("ghostbridge:   enable HTTPS at https://login.tailscale.com/admin/dns")
			return gbridgeWebStatusHTTPSCertsDisabled
		}
		// Log the exact FQDN(s) tsnet thinks it can serve via LE so the operator
		// can compare with whatever URL the browser is hitting. A mismatch (e.g.
		// browser uses a stale tailnet domain) presents as 'internal_error' from
		// the TLS server because GetCertificate has nothing to return for that SNI.
		log.Printf("ghostbridge: HTTPS-eligible cert domains: %v", domains)
	}

	if err := startWebListeners(context.Background(), h.server, certHash, staticCert); err != nil {
		log.Printf("ghostbridge: gbridge_start_web_server: listener bind failed: %v", err)
		return gbridgeWebStatusListenFailed
	}
	return gbridgeWebStatusOK
}

//export gbridge_getips
func gbridge_getips(sd C.int32_t, cBuf *C.char, cBufLen C.size_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	// tsnet.Server.TailscaleIPs() returns (ip4, ip6 netip.Addr), not a slice,
	// and does not return an error. Either may be zero/invalid.
	ip4, ip6 := h.server.TailscaleIPs()
	parts := make([]string, 0, 2)
	if ip4.IsValid() {
		parts = append(parts, ip4.String())
	}
	if ip6.IsValid() {
		parts = append(parts, ip6.String())
	}
	s := strings.Join(parts, ",")
	if C.size_t(len(s)+1) > cBufLen {
		return -5
	}
	// Copy bytes directly into the caller's buffer (no intermediate C.CString).
	dst := unsafe.Slice((*byte)(unsafe.Pointer(cBuf)), int(cBufLen))
	copy(dst, s)
	dst[len(s)] = 0
	return 0
}

func lookup(sd int32) *serverHandle {
	serversMu.Lock()
	defer serversMu.Unlock()
	return servers[sd]
}

func makeSocketpair() (goFd, cFd int, err error) {
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		return 0, 0, err
	}
	// Only the C side needs O_NONBLOCK (tokio AsyncFd expects it).
	// The Go side stays blocking for simple goroutine loops.
	if err := setNonblock(fds[1]); err != nil {
		syscall.Close(fds[0])
		syscall.Close(fds[1])
		return 0, 0, err
	}
	return fds[0], fds[1], nil
}

// dialedPacketConn adapts a connected net.Conn to a PacketConn-ish interface
// used by the packet bridge. Source address on reads is always the dialed peer.
type dialedPacketConn struct {
	net.Conn
}

func (d *dialedPacketConn) ReadFrom(p []byte) (int, net.Addr, error) {
	n, err := d.Conn.Read(p)
	return n, d.Conn.RemoteAddr(), err
}

func (d *dialedPacketConn) WriteTo(p []byte, _ net.Addr) (int, error) {
	return d.Conn.Write(p)
}

type readFromWriter interface {
	ReadFrom(p []byte) (int, net.Addr, error)
	WriteTo(p []byte, addr net.Addr) (int, error)
	Close() error
}

// spawnPacketBridge runs two goroutines that copy framed packets between a
// PacketConn (tsnet side) and a blocking socketpair fd (Rust side).
//
// Frame layout: [total_len(4)][payload_len(4)][payload][port(2)][host\0]
// total_len is the full frame size including the first 4 bytes.
func spawnPacketBridge(conn readFromWriter, goFd int) {
	// tsnet -> socketpair
	go func() {
		defer syscall.Close(goFd)
		buf := make([]byte, 65535)
		pktCount := 0
		for {
			n, srcAddr, err := conn.ReadFrom(buf)
			if err != nil {
				log.Printf("ghostbridge: tsnet->rust ReadFrom error after %d pkts: %v", pktCount, err)
				return
			}
			pktCount++
			if pktCount <= 5 {
				log.Printf("ghostbridge: tsnet->rust pkt #%d %d bytes from %s", pktCount, n, srcAddr)
			}
			host, port, err := net.SplitHostPort(srcAddr.String())
			if err != nil {
				continue
			}
			portNum, _ := strconv.Atoi(port)
			frame := encodeFrame(buf[:n], host, uint16(portNum))
			if _, err := writeAll(goFd, frame); err != nil {
				return
			}
		}
	}()

	// socketpair -> tsnet
	go func() {
		defer conn.Close()
		header := make([]byte, 8)
		pktCount := 0
		for {
			if _, err := readFull(goFd, header); err != nil {
				log.Printf("ghostbridge: rust->tsnet readFull error after %d pkts: %v", pktCount, err)
				return
			}
			pktCount++
			if pktCount <= 5 {
				log.Printf("ghostbridge: rust->tsnet pkt #%d", pktCount)
			}
			totalLen := binary.BigEndian.Uint32(header[0:4])
			payloadLen := binary.BigEndian.Uint32(header[4:8])
			// Promote to uint64 so the minimum-frame-size check cannot wrap
			// if a malformed peer sends a huge payloadLen.
			if uint64(totalLen) < uint64(payloadLen)+11 {
				return // malformed
			}
			rest := make([]byte, totalLen-8)
			if _, err := readFull(goFd, rest); err != nil {
				return
			}
			payload := rest[:payloadLen]
			port := binary.BigEndian.Uint16(rest[payloadLen : payloadLen+2])
			hostBytes := rest[payloadLen+2:]
			// Trim trailing null terminator.
			if i := indexByte(hostBytes, 0); i >= 0 {
				hostBytes = hostBytes[:i]
			}
			dst, err := net.ResolveUDPAddr("udp", net.JoinHostPort(string(hostBytes), strconv.Itoa(int(port))))
			if err != nil {
				continue
			}
			conn.WriteTo(payload, dst)
		}
	}()
}

func encodeFrame(payload []byte, host string, port uint16) []byte {
	hostBytes := []byte(host)
	totalLen := 4 + 4 + len(payload) + 2 + len(hostBytes) + 1
	frame := make([]byte, totalLen)
	binary.BigEndian.PutUint32(frame[0:4], uint32(totalLen))
	binary.BigEndian.PutUint32(frame[4:8], uint32(len(payload)))
	copy(frame[8:], payload)
	off := 8 + len(payload)
	binary.BigEndian.PutUint16(frame[off:off+2], port)
	copy(frame[off+2:], hostBytes)
	frame[totalLen-1] = 0
	return frame
}

// readFull reads exactly len(p) bytes from fd, retrying on short reads.
// The Go side of the socketpair is blocking, so io.EOF means the peer closed.
func readFull(fd int, p []byte) (int, error) {
	total := 0
	for total < len(p) {
		n, err := syscall.Read(fd, p[total:])
		if err != nil {
			return total, err
		}
		if n == 0 {
			return total, io.EOF
		}
		total += n
	}
	return total, nil
}

func writeAll(fd int, p []byte) (int, error) {
	total := 0
	for total < len(p) {
		n, err := syscall.Write(fd, p[total:])
		if err != nil {
			return total, err
		}
		total += n
	}
	return total, nil
}

func indexByte(b []byte, c byte) int {
	for i, x := range b {
		if x == c {
			return i
		}
	}
	return -1
}

func main() {} // Required for c-archive build mode
