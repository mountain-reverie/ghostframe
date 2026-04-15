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
	"net"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"unsafe"

	"tailscale.com/tsnet"
)

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
	if _, err := h.server.Up(context.Background()); err != nil {
		return -2
	}
	return 0
}

//export gbridge_listen_udp
func gbridge_listen_udp(sd C.int32_t, cAddr *C.char, fdOut *C.int32_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	conn, err := h.server.ListenPacket("udp", C.GoString(cAddr))
	if err != nil {
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
	// tsnet.Server.Dial returns a net.Conn; for UDP it wraps the packet conn.
	netConn, err := h.server.Dial(context.Background(), "udp", C.GoString(cRemote))
	if err != nil {
		return -3
	}
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
		for {
			n, srcAddr, err := conn.ReadFrom(buf)
			if err != nil {
				return
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
		for {
			if _, err := readFull(goFd, header); err != nil {
				return
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
