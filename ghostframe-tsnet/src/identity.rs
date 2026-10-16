//! Tailnet identity lookups: who a peer is, who this node is, and the hook
//! the embedded web server uses to ask whether a peer may be served.
//!
//! These are lookups only. The access rule itself is the embedder's (for the
//! server, `ghostframe-lib`'s `peer_auth`); nothing here decides anything.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::{GhostbridgeError, GhostbridgeHandle};

/// The callback ghostbridge's web middleware calls for each request.
/// Returns 1 to serve the request; anything else refuses it.
type RawPeerAuthorizer = unsafe extern "C" fn(
    ctx: *mut c_void,
    peer_addr: *const c_char,
    lookup_status: i32,
    user_id: i64,
    login_name: *const c_char,
    node_name: *const c_char,
    tags_csv: *const c_char,
) -> i32;

extern "C" {
    fn gbridge_whois(sd: c_int, addr: *const c_char, buf: *mut c_char, buf_len: usize) -> c_int;
    fn gbridge_self_identity(sd: c_int, buf: *mut c_char, buf_len: usize) -> c_int;
    fn gbridge_set_peer_authorizer(sd: c_int, f: RawPeerAuthorizer, ctx: *mut c_void) -> c_int;
}

// Status codes, matching `ghostbridge/identity.go`. Only OK and NOT_FOUND
// are matched on; every other code is a failed lookup. The rest are here to
// document the ABI, and the test pins them all.
pub(crate) const IDENTITY_STATUS_OK: c_int = 0;
#[allow(dead_code)]
pub(crate) const IDENTITY_STATUS_INVALID_HANDLE: c_int = -1;
#[allow(dead_code)]
pub(crate) const IDENTITY_STATUS_BUFFER_TOO_SMALL: c_int = -5;
pub(crate) const WHOIS_STATUS_NOT_FOUND: c_int = -30;
#[allow(dead_code)]
pub(crate) const WHOIS_STATUS_FAILED: c_int = -31;

/// Room for a record: a login name and a node FQDN are each bounded well
/// under 512 bytes, and a node carries a handful of tags.
const IDENTITY_BUF_LEN: usize = 8192;

/// A tailnet node's identity, as the control plane reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// The owning user's stable numeric ID. For a tagged node this is the
    /// control plane's placeholder user and means nothing: tags replace
    /// the user as the node's identity.
    pub user_id: i64,
    /// The owning user's login name (e.g. `alice@example.com`). Display
    /// only; compare users by [`Self::user_id`].
    pub login_name: String,
    /// The node's MagicDNS name.
    pub node_name: String,
    /// ACL tags (`tag:...`). Non-empty means the node is tagged.
    pub tags: Vec<String>,
}

impl PeerIdentity {
    pub fn is_tagged(&self) -> bool {
        !self.tags.is_empty()
    }
}

/// Why a WhoIs produced no identity.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WhoIsError {
    /// The address belongs to no node in this node's netmap.
    #[error("peer not found in the tailnet")]
    NotFound,
    /// The lookup itself failed (LocalAPI error, timeout, malformed answer).
    #[error("identity lookup failed: {0}")]
    Failed(String),
}

/// Parse the `user_id \n login \n node \n tag,tag` record ghostbridge writes.
pub(crate) fn parse_identity_record(rec: &str) -> Result<PeerIdentity, WhoIsError> {
    let mut it = rec.split('\n');
    let (Some(uid), Some(login), Some(node), Some(tags), None) =
        (it.next(), it.next(), it.next(), it.next(), it.next())
    else {
        return Err(WhoIsError::Failed("malformed identity record".into()));
    };
    let user_id = uid
        .parse::<i64>()
        .map_err(|_| WhoIsError::Failed("malformed user id".into()))?;
    let tags = if tags.is_empty() {
        Vec::new()
    } else {
        tags.split(',').map(str::to_owned).collect()
    };
    Ok(PeerIdentity {
        user_id,
        login_name: login.to_owned(),
        node_name: node.to_owned(),
        tags,
    })
}

fn identity_from_rc(name: &str, rc: c_int, buf: &[u8]) -> Result<PeerIdentity, WhoIsError> {
    match rc {
        IDENTITY_STATUS_OK => {
            let rec = CStr::from_bytes_until_nul(buf)
                .map_err(|_| WhoIsError::Failed(format!("{name}: missing NUL")))?
                .to_str()
                .map_err(|_| WhoIsError::Failed(format!("{name}: non-utf8")))?;
            parse_identity_record(rec)
        }
        WHOIS_STATUS_NOT_FOUND => Err(WhoIsError::NotFound),
        other => Err(WhoIsError::Failed(format!("{name} rc={other}"))),
    }
}

fn whois_sd(sd: c_int, addr: &str) -> Result<PeerIdentity, WhoIsError> {
    let c_addr = CString::new(addr).map_err(|_| WhoIsError::Failed("address has NUL".into()))?;
    let mut buf = vec![0u8; IDENTITY_BUF_LEN];
    // SAFETY: c_addr is NUL-terminated; buf is valid for buf.len() bytes.
    let rc = unsafe {
        gbridge_whois(
            sd,
            c_addr.as_ptr(),
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        )
    };
    identity_from_rc("gbridge_whois", rc, &buf)
}

/// A detached WhoIs client: holds only the Go-side handle id, so a blocking
/// lookup can run on another thread while the [`GhostbridgeHandle`] is owned
/// elsewhere. A closed handle no longer resolves and every lookup fails.
#[derive(Debug, Clone, Copy)]
pub struct WhoIsClient {
    sd: c_int,
}

impl WhoIsClient {
    /// Who owns the node at `addr` (an IP, or `IP:port`).
    ///
    /// Blocks on a LocalAPI call (bounded at 2 s on the Go side); call it off
    /// any event loop.
    pub fn whois(&self, addr: &str) -> Result<PeerIdentity, WhoIsError> {
        whois_sd(self.sd, addr)
    }
}

/// The web server's per-request access check. Receives the peer's
/// `IP:port` and the outcome of looking it up; returns whether to serve it.
pub type PeerAuthorizerFn =
    dyn Fn(&str, Result<PeerIdentity, WhoIsError>) -> bool + Send + Sync + 'static;

/// A C string field, or `None` if it is missing or not UTF-8. A field that
/// cannot be read is never treated as empty: an unreadable tag list read as
/// "no tags" would turn a tagged peer into an untagged one.
unsafe fn c_field<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok()
    }
}

/// The C trampoline ghostbridge calls. Never unwinds: a panic, or a peer
/// address that is not UTF-8, refuses the request.
unsafe extern "C" fn peer_authorizer_trampoline(
    ctx: *mut c_void,
    peer_addr: *const c_char,
    lookup_status: i32,
    user_id: i64,
    login_name: *const c_char,
    node_name: *const c_char,
    tags_csv: *const c_char,
) -> i32 {
    let verdict = catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() || peer_addr.is_null() {
            return false;
        }
        // SAFETY: ctx is the `Box<Box<PeerAuthorizerFn>>` leaked by
        // `set_peer_authorizer`, never freed.
        let f = &*(ctx as *const Box<PeerAuthorizerFn>);
        let Ok(peer) = CStr::from_ptr(peer_addr).to_str() else {
            return false;
        };
        let lookup = match lookup_status {
            IDENTITY_STATUS_OK => {
                match (c_field(login_name), c_field(node_name), c_field(tags_csv)) {
                    (Some(login), Some(node), Some(tags)) => Ok(PeerIdentity {
                        user_id,
                        login_name: login.to_owned(),
                        node_name: node.to_owned(),
                        tags: if tags.is_empty() {
                            Vec::new()
                        } else {
                            tags.split(',').map(str::to_owned).collect()
                        },
                    }),
                    _ => Err(WhoIsError::Failed("unreadable identity field".into())),
                }
            }
            WHOIS_STATUS_NOT_FOUND => Err(WhoIsError::NotFound),
            other => Err(WhoIsError::Failed(format!("rc={other}"))),
        };
        f(peer, lookup)
    }));
    i32::from(matches!(verdict, Ok(true)))
}

impl GhostbridgeHandle {
    /// Who owns the node at `addr` (an IP, or `IP:port`). Blocking; see
    /// [`WhoIsClient::whois`].
    pub fn whois(&self, addr: &str) -> Result<PeerIdentity, WhoIsError> {
        whois_sd(self.sd, addr)
    }

    /// A detached [`WhoIsClient`] for lookups off the owning thread.
    pub fn whois_client(&self) -> WhoIsClient {
        WhoIsClient { sd: self.sd }
    }

    /// This node's own identity: the user who logged it in (or owns the
    /// auth key it joined with), or its tags if it is a tagged node.
    /// Only meaningful once [`Self::up`] has returned.
    pub fn self_identity(&self) -> Result<PeerIdentity, WhoIsError> {
        let mut buf = vec![0u8; IDENTITY_BUF_LEN];
        // SAFETY: buf is valid for buf.len() bytes.
        let rc =
            unsafe { gbridge_self_identity(self.sd, buf.as_mut_ptr() as *mut c_char, buf.len()) };
        identity_from_rc("gbridge_self_identity", rc, &buf)
    }

    /// Install the check the embedded web server applies to every request.
    /// Until one is installed, the web server refuses every peer.
    ///
    /// The closure is leaked on purpose: a request already in flight in Go
    /// may still call it after this handle is closed, so it must live for
    /// the rest of the process. Calling this again leaks the previous one.
    pub fn set_peer_authorizer(&self, f: Box<PeerAuthorizerFn>) -> Result<(), GhostbridgeError> {
        let ctx = Box::into_raw(Box::new(f)) as *mut c_void;
        // SAFETY: ctx stays valid forever (leaked); the trampoline matches
        // the C typedef in identity.go.
        let rc = unsafe { gbridge_set_peer_authorizer(self.sd, peer_authorizer_trampoline, ctx) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_set_peer_authorizer", rc));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_untagged_record() {
        let id = parse_identity_record("42\nalice@example.com\nlaptop.ts.net.\n").unwrap();
        assert_eq!(
            id,
            PeerIdentity {
                user_id: 42,
                login_name: "alice@example.com".into(),
                node_name: "laptop.ts.net.".into(),
                tags: vec![],
            }
        );
        assert!(!id.is_tagged());
    }

    #[test]
    fn parses_tags() {
        let id = parse_identity_record("7\ntagged-devices\nci.ts.net.\ntag:ci,tag:ops").unwrap();
        assert_eq!(id.tags, vec!["tag:ci".to_string(), "tag:ops".to_string()]);
        assert!(id.is_tagged());
    }

    #[test]
    fn rejects_malformed_records() {
        for rec in [
            "",
            "42",
            "42\na\nb",
            "x\na\nb\n",
            "42\na\nb\nc\nextra",
            "42\na\nb\nc\n",
        ] {
            assert!(
                matches!(parse_identity_record(rec), Err(WhoIsError::Failed(_))),
                "{rec:?} parsed"
            );
        }
    }

    #[test]
    fn not_found_is_distinct_from_failure() {
        assert_eq!(
            identity_from_rc("t", WHOIS_STATUS_NOT_FOUND, &[0]),
            Err(WhoIsError::NotFound)
        );
        assert!(matches!(
            identity_from_rc("t", WHOIS_STATUS_FAILED, &[0]),
            Err(WhoIsError::Failed(_))
        ));
        assert!(matches!(
            identity_from_rc("t", IDENTITY_STATUS_BUFFER_TOO_SMALL, &[0]),
            Err(WhoIsError::Failed(_))
        ));
    }

    #[test]
    fn identity_status_codes_match_ghostbridge() {
        // Must match the constants in ghostbridge/identity.go.
        assert_eq!(IDENTITY_STATUS_OK, 0);
        assert_eq!(IDENTITY_STATUS_INVALID_HANDLE, -1);
        assert_eq!(IDENTITY_STATUS_BUFFER_TOO_SMALL, -5);
        assert_eq!(WHOIS_STATUS_NOT_FOUND, -30);
        assert_eq!(WHOIS_STATUS_FAILED, -31);
    }

    fn call(
        f: Box<PeerAuthorizerFn>,
        status: i32,
        login: &str,
        tags: &str,
    ) -> (i32, *mut Box<PeerAuthorizerFn>) {
        let ctx = Box::into_raw(Box::new(f));
        let peer = CString::new("100.64.0.9:5000").unwrap();
        let login = CString::new(login).unwrap();
        let node = CString::new("n.ts.net.").unwrap();
        let tags = CString::new(tags).unwrap();
        let rc = unsafe {
            peer_authorizer_trampoline(
                ctx as *mut c_void,
                peer.as_ptr(),
                status,
                5,
                login.as_ptr(),
                node.as_ptr(),
                tags.as_ptr(),
            )
        };
        (rc, ctx)
    }

    #[test]
    fn trampoline_passes_the_identity_and_the_verdict() {
        let (rc, ctx) = call(
            Box::new(|peer, lookup| {
                let id = lookup.expect("found");
                peer == "100.64.0.9:5000"
                    && id.user_id == 5
                    && id.login_name == "bob@x"
                    && id.tags == vec!["tag:a".to_string()]
            }),
            IDENTITY_STATUS_OK,
            "bob@x",
            "tag:a",
        );
        drop(unsafe { Box::from_raw(ctx) });
        assert_eq!(rc, 1);
    }

    #[test]
    fn trampoline_reports_lookup_failures_and_denials() {
        let (rc, ctx) = call(
            Box::new(|_, lookup| lookup == Err(WhoIsError::NotFound)),
            WHOIS_STATUS_NOT_FOUND,
            "",
            "",
        );
        drop(unsafe { Box::from_raw(ctx) });
        assert_eq!(rc, 1, "NotFound must reach the closure as NotFound");

        let (rc, ctx) = call(Box::new(|_, _| false), IDENTITY_STATUS_OK, "a", "");
        drop(unsafe { Box::from_raw(ctx) });
        assert_eq!(rc, 0);
    }

    #[test]
    fn trampoline_refuses_when_the_closure_panics() {
        let (rc, ctx) = call(Box::new(|_, _| panic!("boom")), IDENTITY_STATUS_OK, "a", "");
        drop(unsafe { Box::from_raw(ctx) });
        assert_eq!(rc, 0);
    }

    // An unreadable tag list must reach the rule as a failed lookup, not as
    // "untagged": that would let a tagged device match its user.
    #[test]
    fn trampoline_reports_an_unreadable_field_as_a_failed_lookup() {
        let ctx = Box::into_raw(Box::new(Box::new(
            |_: &str, lookup: Result<PeerIdentity, WhoIsError>| {
                matches!(lookup, Err(WhoIsError::Failed(_)))
            },
        ) as Box<PeerAuthorizerFn>));
        let peer = CString::new("100.64.0.9:5000").unwrap();
        let login = CString::new("a").unwrap();
        let bad_tags = CString::new(vec![b't', 0xff]).unwrap();
        let rc = unsafe {
            peer_authorizer_trampoline(
                ctx as *mut c_void,
                peer.as_ptr(),
                IDENTITY_STATUS_OK,
                1,
                login.as_ptr(),
                login.as_ptr(),
                bad_tags.as_ptr(),
            )
        };
        drop(unsafe { Box::from_raw(ctx) });
        assert_eq!(rc, 1, "the closure must have seen Failed");
    }

    #[test]
    fn trampoline_refuses_a_null_context() {
        let peer = CString::new("1.2.3.4:1").unwrap();
        let rc = unsafe {
            peer_authorizer_trampoline(
                std::ptr::null_mut(),
                peer.as_ptr(),
                IDENTITY_STATUS_OK,
                1,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0);
    }
}
