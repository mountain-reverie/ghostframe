package main

// Peer identity lookups, and the hook the web server uses to ask Rust
// whether a peer may be served.
//
// The access rule itself lives in Rust (ghostframe-lib's peer_auth module),
// in one place. Go only answers "who is this?": WhoIs for a peer, Status for
// this node. The web server's middleware hands each WhoIs result to a
// callback Rust registered, so there is no second copy of the rule here.

/*
#include <stdint.h>
#include <stdlib.h>

// Returns 1 to serve the request, anything else to refuse it.
// lookup_status is 0 when the peer was found, otherwise one of the
// gbridgeWhois* codes below; the identity fields are then empty.
typedef int32_t (*gbridge_peer_authorizer)(
	void *ctx,
	const char *peer_addr,
	int32_t lookup_status,
	int64_t user_id,
	const char *login_name,
	const char *node_name,
	const char *tags_csv);

static inline int32_t gbridge_call_peer_authorizer(
	gbridge_peer_authorizer f, void *ctx, const char *peer_addr,
	int32_t lookup_status, int64_t user_id, const char *login_name,
	const char *node_name, const char *tags_csv)
{
	return f(ctx, peer_addr, lookup_status, user_id, login_name, node_name, tags_csv);
}
*/
import "C"

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net/http"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
	"unsafe"

	"tailscale.com/client/local"
	"tailscale.com/client/tailscale/apitype"
	"tailscale.com/ipn/ipnstate"
)

// Status codes for the identity exports. Pinned on the Rust side by
// ghostframe-tsnet's `identity_status_codes_match_ghostbridge`.
const (
	gbridgeIdentityStatusOK             = 0
	gbridgeIdentityStatusInvalidHandle  = -1
	gbridgeIdentityStatusBufferTooSmall = -5
	gbridgeWhoisStatusNotFound          = -30
	gbridgeWhoisStatusFailed            = -31
)

// identityLookupTimeout bounds one LocalAPI WhoIs / Status call. These are
// in-process calls and normally take well under a millisecond; the bound is
// there so a wedged backend turns into a refusal rather than a hang.
const identityLookupTimeout = 2 * time.Second

// peerIdentity is what the access rule needs to know about a node.
type peerIdentity struct {
	userID    int64
	loginName string
	nodeName  string
	tags      []string
}

// errIdentityField is returned when a value from the control plane cannot be
// carried in the newline-separated record. Treated as a failed lookup, so
// the peer is refused rather than mis-parsed.
var errIdentityField = errors.New("ghostbridge: identity field contains a separator")

// encodeIdentity renders the record Rust parses:
//
//	user_id \n login_name \n node_name \n tag,tag
//
// A field carrying a separator is rejected rather than escaped: none can
// legitimately contain one, and a refusal is the safe reading of a value
// that does.
func encodeIdentity(id peerIdentity) (string, error) {
	for _, f := range []string{id.loginName, id.nodeName} {
		if strings.ContainsAny(f, "\n\x00") {
			return "", errIdentityField
		}
	}
	for _, t := range id.tags {
		if t == "" || strings.ContainsAny(t, "\n\x00,") {
			return "", errIdentityField
		}
	}
	return strconv.FormatInt(id.userID, 10) + "\n" + id.loginName + "\n" +
		id.nodeName + "\n" + strings.Join(id.tags, ","), nil
}

// identityFromWhoIs extracts the identity from a WhoIs response. The user is
// the node's owning user (Node.User), which for a tagged node is the
// control plane's placeholder: the rule ignores it whenever tags are present.
func identityFromWhoIs(resp *apitype.WhoIsResponse) (peerIdentity, error) {
	if resp == nil || resp.Node == nil {
		return peerIdentity{}, errors.New("ghostbridge: WhoIs response has no node")
	}
	id := peerIdentity{
		userID:   int64(resp.Node.User),
		nodeName: resp.Node.Name,
		tags:     append([]string(nil), resp.Node.Tags...),
	}
	if resp.UserProfile != nil {
		id.loginName = resp.UserProfile.LoginName
	}
	return id, nil
}

// identityFromStatus extracts this node's own identity.
func identityFromStatus(st *ipnstate.Status) (peerIdentity, error) {
	if st == nil || st.Self == nil {
		return peerIdentity{}, errors.New("ghostbridge: status has no self node")
	}
	id := peerIdentity{
		userID:   int64(st.Self.UserID),
		nodeName: st.Self.DNSName,
	}
	if st.Self.Tags != nil {
		id.tags = st.Self.Tags.AsSlice()
	}
	if up, ok := st.User[st.Self.UserID]; ok {
		id.loginName = up.LoginName
	}
	return id, nil
}

// whoiser is the one method the lookups need from *local.Client, narrowed so
// the outcomes are testable without a tailnet.
type whoiser interface {
	WhoIs(ctx context.Context, remoteAddr string) (*apitype.WhoIsResponse, error)
}

// lookupPeer maps a WhoIs call onto (identity, status code).
func lookupPeer(lc whoiser, addr string) (peerIdentity, int32) {
	ctx, cancel := context.WithTimeout(context.Background(), identityLookupTimeout)
	defer cancel()
	resp, err := lc.WhoIs(ctx, addr)
	if err != nil {
		if errors.Is(err, local.ErrPeerNotFound) {
			return peerIdentity{}, gbridgeWhoisStatusNotFound
		}
		log.Printf("ghostbridge: WhoIs(%q): %v", addr, err)
		return peerIdentity{}, gbridgeWhoisStatusFailed
	}
	id, err := identityFromWhoIs(resp)
	if err != nil {
		log.Printf("ghostbridge: WhoIs(%q): %v", addr, err)
		return peerIdentity{}, gbridgeWhoisStatusFailed
	}
	return id, gbridgeIdentityStatusOK
}

// copyToCBuf writes s plus a NUL into the caller's buffer.
func copyToCBuf(s string, cBuf *C.char, cBufLen C.size_t) C.int32_t {
	if C.size_t(len(s)+1) > cBufLen {
		return gbridgeIdentityStatusBufferTooSmall
	}
	dst := unsafe.Slice((*byte)(unsafe.Pointer(cBuf)), int(cBufLen))
	copy(dst, s)
	dst[len(s)] = 0
	return gbridgeIdentityStatusOK
}

// Look up the tailnet identity of the peer at `addr` (an IP, or IP:port).
// Writes the record described at encodeIdentity on success.
//
//export gbridge_whois
func gbridge_whois(sd C.int32_t, cAddr *C.char, cBuf *C.char, cBufLen C.size_t) C.int32_t {
	h := lookup(int32(sd))
	if h == nil {
		return gbridgeIdentityStatusInvalidHandle
	}
	lc, err := h.server.LocalClient()
	if err != nil {
		log.Printf("ghostbridge: gbridge_whois: LocalClient: %v", err)
		return gbridgeWhoisStatusFailed
	}
	id, status := lookupPeer(lc, C.GoString(cAddr))
	if status != gbridgeIdentityStatusOK {
		return C.int32_t(status)
	}
	rec, err := encodeIdentity(id)
	if err != nil {
		return gbridgeWhoisStatusFailed
	}
	return copyToCBuf(rec, cBuf, cBufLen)
}

// This node's own identity: its owning user and its tags, from Status.
// Same record as gbridge_whois.
//
//export gbridge_self_identity
func gbridge_self_identity(sd C.int32_t, cBuf *C.char, cBufLen C.size_t) C.int32_t {
	h := lookup(int32(sd))
	if h == nil {
		return gbridgeIdentityStatusInvalidHandle
	}
	lc, err := h.server.LocalClient()
	if err != nil {
		log.Printf("ghostbridge: gbridge_self_identity: LocalClient: %v", err)
		return gbridgeWhoisStatusFailed
	}
	ctx, cancel := context.WithTimeout(context.Background(), identityLookupTimeout)
	defer cancel()
	st, err := lc.StatusWithoutPeers(ctx)
	if err != nil {
		log.Printf("ghostbridge: gbridge_self_identity: %v", err)
		return gbridgeWhoisStatusFailed
	}
	id, err := identityFromStatus(st)
	if err != nil {
		log.Printf("ghostbridge: gbridge_self_identity: %v", err)
		return gbridgeWhoisStatusFailed
	}
	rec, err := encodeIdentity(id)
	if err != nil {
		return gbridgeWhoisStatusFailed
	}
	return copyToCBuf(rec, cBuf, cBufLen)
}

// peerAuthorizer decides whether a looked-up peer may be served. The
// production implementation calls back into Rust; tests supply their own.
type peerAuthorizer interface {
	allow(peerAddr string, status int32, id peerIdentity) bool
}

// authorizerSlot holds the authorizer Rust registered, if any. Read on every
// request; an empty slot refuses everyone.
type authorizerSlot struct {
	p atomic.Pointer[peerAuthorizer]
}

func (s *authorizerSlot) set(a peerAuthorizer) { s.p.Store(&a) }

func (s *authorizerSlot) get() peerAuthorizer {
	if p := s.p.Load(); p != nil {
		return *p
	}
	return nil
}

// cAuthorizer calls the Rust callback.
type cAuthorizer struct {
	fn  C.gbridge_peer_authorizer
	ctx unsafe.Pointer
}

func (a cAuthorizer) allow(peerAddr string, status int32, id peerIdentity) bool {
	cPeer := C.CString(peerAddr)
	defer C.free(unsafe.Pointer(cPeer))
	cLogin := C.CString(id.loginName)
	defer C.free(unsafe.Pointer(cLogin))
	cNode := C.CString(id.nodeName)
	defer C.free(unsafe.Pointer(cNode))
	cTags := C.CString(strings.Join(id.tags, ","))
	defer C.free(unsafe.Pointer(cTags))
	rc := C.gbridge_call_peer_authorizer(a.fn, a.ctx, cPeer, C.int32_t(status),
		C.int64_t(id.userID), cLogin, cNode, cTags)
	return rc == 1
}

// Register the callback the web server consults for every request. `ctx` is
// passed back verbatim and must stay valid for the life of the process: a
// request already in flight may still call it after the handle is closed.
//
//export gbridge_set_peer_authorizer
func gbridge_set_peer_authorizer(sd C.int32_t, fn C.gbridge_peer_authorizer, ctx unsafe.Pointer) C.int32_t {
	h := lookup(int32(sd))
	if h == nil {
		return gbridgeIdentityStatusInvalidHandle
	}
	if fn == nil {
		return gbridgeWebStatusInvalidArg
	}
	h.authz.set(cAuthorizer{fn: fn, ctx: ctx})
	return gbridgeIdentityStatusOK
}

// requireAuthorizedPeer wraps a handler so that only peers the registered
// authorizer admits reach it. Everyone else -- including every peer when no
// authorizer has been registered -- gets a bare 403: no SPA, no cert hash.
func requireAuthorizedPeer(next http.Handler, lc whoiser, slot *authorizerSlot) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		a := slot.get()
		if a == nil {
			forbid(w)
			return
		}
		id, status := lookupPeer(lc, r.RemoteAddr)
		if !a.allow(r.RemoteAddr, status, id) {
			forbid(w)
			return
		}
		next.ServeHTTP(w, r)
	})
}

func forbid(w http.ResponseWriter) {
	w.Header().Set("Cache-Control", "no-store")
	http.Error(w, fmt.Sprintf("%d %s", http.StatusForbidden, http.StatusText(http.StatusForbidden)), http.StatusForbidden)
}
