//! Who may connect: the tailnet peer access rule, in one place.
//!
//! Being on the tailnet is not enough. By default only peers owned by the
//! user who owns this node are admitted -- the user who logged it in, or who
//! owns the auth key it joined with -- and `GHOSTFRAME_ALLOWED_USERS` (see
//! [`crate::config::AccessConfig`]) adds more users and tags to that. The
//! QUIC listener and the embedded web server both ask [`AccessPolicy::decide`];
//! there is no second copy of the rule, in Go or anywhere else.
//!
//! # The rule
//!
//! - A peer the tailnet cannot identify, or whose lookup failed, is refused.
//! - A **tagged** peer is identified by its tags, not by a user, which is
//!   Tailscale's own semantics: tags replace the user. It is admitted only
//!   if one of its tags is listed. Its owning user is never consulted, so a
//!   tagged device cannot pass as the owner.
//! - An **untagged** peer is admitted if its owning user is this node's
//!   owner -- compared by stable numeric user ID, never by display or login
//!   name -- or if its login name is listed.
//! - A tagged *node* has no owning user. It then admits only listed peers,
//!   and with an empty list, nobody.
//!
//! # Identity sources
//!
//! [`PeerIdentitySource`] answers "who is the peer at this IP?". Production
//! uses tsnet's WhoIs. Identifying a peer by its tailnet IP is sound because
//! WireGuard only accepts an inner source address from the peer whose
//! AllowedIPs contain it; an address behind a subnet router belongs to no
//! node, so WhoIs finds nothing and the peer is refused.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::AccessConfig;
pub use crate::transport::ghostbridge::{PeerIdentity, WhoIsError};

/// The outcome of asking who a peer is.
pub type PeerLookup = Result<PeerIdentity, WhoIsError>;

/// How long an admission is remembered for a peer IP.
pub const ALLOW_TTL: Duration = Duration::from_secs(30);
/// How long a refusal is remembered. Short, so a peer that has just been
/// added to the list is not kept out for long; non-zero, so a refused peer
/// retrying in a loop does not cost a lookup per packet.
pub const DENY_TTL: Duration = Duration::from_secs(5);
/// Minimum gap between two denial log lines for the same peer IP.
pub const DENIAL_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// Peers the denial log remembers before it forgets the oldest.
const DENIAL_LOG_CAPACITY: usize = 256;

/// This node's owning user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub user_id: i64,
    pub login_name: String,
}

/// The resolved access policy: the owner plus whatever the configuration
/// adds. Built once at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPolicy {
    owner: Option<Owner>,
    logins: BTreeSet<String>,
    tags: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowReason {
    /// The peer belongs to the user who owns this node.
    Owner,
    /// The peer's user is in `GHOSTFRAME_ALLOWED_USERS`.
    ListedUser,
    /// The peer carries this listed tag.
    ListedTag(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// No node in the tailnet has this address.
    UnknownPeer,
    /// The identity lookup itself failed.
    LookupFailed(String),
    /// An untagged peer of a user who is neither the owner nor listed.
    UserNotAllowed,
    /// A tagged peer none of whose tags are listed.
    TagsNotAllowed,
    /// No identity source is installed, so nobody can be identified.
    NoIdentitySource,
}

impl std::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPeer => f.write_str("peer not found in the tailnet"),
            Self::LookupFailed(e) => write!(f, "identity lookup failed ({e})"),
            Self::UserNotAllowed => {
                f.write_str("user is not this node's owner and not in GHOSTFRAME_ALLOWED_USERS")
            }
            Self::TagsNotAllowed => {
                f.write_str("tagged peer, and none of its tags is in GHOSTFRAME_ALLOWED_USERS")
            }
            Self::NoIdentitySource => f.write_str("no identity source installed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow(AllowReason),
    Deny(DenyReason),
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow(_))
    }

    /// How long this decision may be reused for the same IP. `None` for a
    /// failed lookup: that says nothing about the peer, so the next
    /// connection attempt asks again.
    pub fn ttl(&self) -> Option<Duration> {
        match self {
            Self::Allow(_) => Some(ALLOW_TTL),
            Self::Deny(DenyReason::LookupFailed(_) | DenyReason::NoIdentitySource) => None,
            Self::Deny(_) => Some(DENY_TTL),
        }
    }
}

impl AccessPolicy {
    /// The policy for a node whose own identity is `self_identity`.
    ///
    /// A tagged node, or one reporting no valid user ID, has no owner.
    pub fn new(self_identity: &PeerIdentity, access: &AccessConfig) -> Self {
        let owner = (!self_identity.is_tagged() && self_identity.user_id > 0).then(|| Owner {
            user_id: self_identity.user_id,
            login_name: self_identity.login_name.clone(),
        });
        Self::from_parts(
            owner,
            access.allowed_logins.iter().cloned(),
            access.allowed_tags.iter().cloned(),
        )
    }

    /// Build directly from parts. Empty strings are dropped: they could only
    /// match a peer whose lookup reported nothing.
    pub fn from_parts(
        owner: Option<Owner>,
        logins: impl IntoIterator<Item = String>,
        tags: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            owner: owner.filter(|o| o.user_id > 0),
            logins: logins.into_iter().filter(|l| !l.is_empty()).collect(),
            tags: tags
                .into_iter()
                .filter(|t| t.len() > "tag:".len() && t.starts_with("tag:"))
                .collect(),
        }
    }

    pub fn owner(&self) -> Option<&Owner> {
        self.owner.as_ref()
    }

    pub fn allowed_logins(&self) -> impl Iterator<Item = &str> {
        self.logins.iter().map(String::as_str)
    }

    pub fn allowed_tags(&self) -> impl Iterator<Item = &str> {
        self.tags.iter().map(String::as_str)
    }

    /// True when no peer can ever be admitted (a tagged node with nothing
    /// listed).
    pub fn admits_no_one(&self) -> bool {
        self.owner.is_none() && self.logins.is_empty() && self.tags.is_empty()
    }

    /// The access rule. See the module documentation.
    pub fn decide(&self, lookup: &PeerLookup) -> Decision {
        let peer = match lookup {
            Ok(peer) => peer,
            Err(WhoIsError::NotFound) => return Decision::Deny(DenyReason::UnknownPeer),
            Err(WhoIsError::Failed(e)) => {
                return Decision::Deny(DenyReason::LookupFailed(e.clone()))
            }
        };
        if peer.is_tagged() {
            return match peer.tags.iter().find(|t| self.tags.contains(*t)) {
                Some(tag) => Decision::Allow(AllowReason::ListedTag(tag.clone())),
                None => Decision::Deny(DenyReason::TagsNotAllowed),
            };
        }
        if self
            .owner
            .as_ref()
            .is_some_and(|o| peer.user_id > 0 && o.user_id == peer.user_id)
        {
            return Decision::Allow(AllowReason::Owner);
        }
        if !peer.login_name.is_empty() && self.logins.contains(&peer.login_name) {
            return Decision::Allow(AllowReason::ListedUser);
        }
        Decision::Deny(DenyReason::UserNotAllowed)
    }

    /// Log the policy once at startup. A tagged node with nothing listed
    /// gets a WARN that says how to fix it, because it will refuse everyone.
    pub fn log_startup(&self, self_identity: &PeerIdentity, access: &AccessConfig) {
        for bad in &access.rejected {
            tracing::warn!(
                entry = %bad,
                "GHOSTFRAME_ALLOWED_USERS: ignoring malformed entry (expected a login \
                 name like alice@example.com or a tag like tag:ops)"
            );
        }
        let logins: Vec<&str> = self.allowed_logins().collect();
        let tags: Vec<&str> = self.allowed_tags().collect();
        match &self.owner {
            Some(o) => tracing::info!(
                owner_login = %o.login_name,
                owner_user_id = o.user_id,
                ?logins,
                ?tags,
                "tailnet access: admitting peers of this node's owner, plus the listed users and tags"
            ),
            None if self.admits_no_one() => tracing::warn!(
                node_tags = ?self_identity.tags,
                "tailnet access: this node is tagged (or reports no owning user), so it has \
                 no owner to admit, and GHOSTFRAME_ALLOWED_USERS is empty: NO peer can \
                 connect. Set GHOSTFRAME_ALLOWED_USERS to the users and tags to admit, \
                 e.g. GHOSTFRAME_ALLOWED_USERS=alice@example.com,tag:ops"
            ),
            None => tracing::warn!(
                node_tags = ?self_identity.tags,
                ?logins,
                ?tags,
                "tailnet access: this node is tagged, so it has no owner; only the users \
                 and tags in GHOSTFRAME_ALLOWED_USERS are admitted"
            ),
        }
    }
}

/// Rate-limited log of refused peers, shared by the QUIC and web paths.
///
/// INFO, at most one line per peer IP per [`DENIAL_LOG_INTERVAL`], carrying
/// how many refusals it stood for. A refused client retries; one line per
/// attempt would flood the journal and, past journald's burst limit, take
/// unrelated lines down with it.
#[derive(Debug, Default)]
pub struct DenialLog {
    seen: Mutex<HashMap<IpAddr, (Instant, u64)>>,
}

impl DenialLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one refusal. Returns whether it was logged (for tests).
    pub fn record(
        &self,
        now: Instant,
        ip: IpAddr,
        via: &'static str,
        lookup: Option<&PeerLookup>,
        reason: &DenyReason,
    ) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let suppressed = match seen.get_mut(&ip) {
            Some((last, n)) if now.saturating_duration_since(*last) < DENIAL_LOG_INTERVAL => {
                *n += 1;
                return false;
            }
            Some((_, n)) => std::mem::take(n),
            None => 0,
        };
        if seen.len() >= DENIAL_LOG_CAPACITY && !seen.contains_key(&ip) {
            seen.retain(|_, (last, _)| now.saturating_duration_since(*last) < DENIAL_LOG_INTERVAL);
            if seen.len() >= DENIAL_LOG_CAPACITY {
                seen.clear();
            }
        }
        seen.insert(ip, (now, 0));
        drop(seen);
        let (login, node, tags) = match lookup {
            Some(Ok(p)) => (
                p.login_name.as_str(),
                p.node_name.as_str(),
                p.tags.join(","),
            ),
            _ => ("", "", String::new()),
        };
        tracing::info!(
            peer = %ip,
            login,
            node,
            tags,
            %reason,
            via,
            suppressed_since_last = suppressed,
            "refused tailnet peer (rate-limited per peer)"
        );
        true
    }
}

/// Answers "who is the peer at this IP?".
pub trait PeerIdentitySource: Send + Sync + 'static {
    /// Blocking lookup.
    fn lookup(&self, ip: IpAddr) -> PeerLookup;

    /// True if [`Self::lookup`] never blocks (an in-memory table), so the
    /// gate may answer on the event loop instead of in the background.
    fn is_immediate(&self) -> bool {
        false
    }
}

/// Production identity source: tsnet's WhoIs.
pub struct TsnetWhoIs(pub crate::transport::ghostbridge::WhoIsClient);

impl PeerIdentitySource for TsnetWhoIs {
    fn lookup(&self, ip: IpAddr) -> PeerLookup {
        self.0.whois(&ip.to_string())
    }
}

/// An in-memory identity table, for tests and harnesses that have no
/// tailnet. Every IP not in the table is [`WhoIsError::NotFound`] unless a
/// fallback identity is set.
#[derive(Debug, Default, Clone)]
pub struct StaticIdentities {
    pub by_ip: HashMap<IpAddr, PeerLookup>,
    pub fallback: Option<PeerIdentity>,
}

impl PeerIdentitySource for StaticIdentities {
    fn lookup(&self, ip: IpAddr) -> PeerLookup {
        match self.by_ip.get(&ip) {
            Some(r) => r.clone(),
            None => self.fallback.clone().ok_or(WhoIsError::NotFound),
        }
    }

    fn is_immediate(&self) -> bool {
        true
    }
}

/// Decisions remembered per peer IP.
#[derive(Debug, Default)]
pub struct DecisionCache {
    entries: HashMap<IpAddr, (Decision, Instant)>,
}

impl DecisionCache {
    /// The cached decision for `ip`, if it has not expired at `now`.
    pub fn get(&mut self, ip: IpAddr, now: Instant) -> Option<&Decision> {
        if self.entries.get(&ip).is_some_and(|(_, exp)| now >= *exp) {
            self.entries.remove(&ip);
        }
        self.entries.get(&ip).map(|(d, _)| d)
    }

    /// Remember `decision` for its TTL. Uncacheable decisions are dropped,
    /// and replace nothing.
    pub fn insert(&mut self, ip: IpAddr, decision: Decision, now: Instant) {
        if let Some(ttl) = decision.ttl() {
            // Expired entries are only dropped on lookup; sweep them here so
            // a stream of one-off peers cannot grow the map without bound.
            if self.entries.len() >= 1024 {
                self.entries.retain(|_, (_, exp)| now < *exp);
            }
            self.entries.insert(ip, (decision, now + ttl));
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What the gate says about a new connection from an IP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateCheck {
    Allow,
    Deny,
    /// A lookup is outstanding; the caller holds the connection until
    /// [`PeerGate::resolve`] is called for this IP.
    Pending,
}

/// Admission state for the QUIC listener: the policy, an identity source,
/// the per-IP decision cache and the set of lookups in flight.
pub struct PeerGate {
    policy: Arc<AccessPolicy>,
    source: Option<Arc<dyn PeerIdentitySource>>,
    denials: Arc<DenialLog>,
    cache: DecisionCache,
    in_flight: HashSet<IpAddr>,
    requested: Vec<IpAddr>,
}

impl std::fmt::Debug for PeerGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerGate")
            .field("policy", &self.policy)
            .field("has_source", &self.source.is_some())
            .field("cached", &self.cache.len())
            .field("in_flight", &self.in_flight.len())
            .finish()
    }
}

impl PeerGate {
    /// A gate that refuses everyone: nothing can be identified. What a
    /// `QuicServer` starts with until an identity source is installed.
    pub fn deny_all() -> Self {
        Self {
            policy: Arc::new(AccessPolicy::from_parts(None, [], [])),
            source: None,
            denials: Arc::new(DenialLog::new()),
            cache: DecisionCache::default(),
            in_flight: HashSet::new(),
            requested: Vec::new(),
        }
    }

    pub fn new(
        policy: Arc<AccessPolicy>,
        source: Arc<dyn PeerIdentitySource>,
        denials: Arc<DenialLog>,
    ) -> Self {
        Self {
            policy,
            source: Some(source),
            denials,
            cache: DecisionCache::default(),
            in_flight: HashSet::new(),
            requested: Vec::new(),
        }
    }

    /// For tests and harnesses with no tailnet: every peer is a user, and
    /// that user owns this node. It still runs the real rule.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn single_owner_for_tests() -> Self {
        let owner = PeerIdentity {
            user_id: 1,
            login_name: "owner@test".into(),
            node_name: "test-peer".into(),
            tags: vec![],
        };
        Self::new(
            Arc::new(AccessPolicy::new(&owner, &AccessConfig::default())),
            Arc::new(StaticIdentities {
                by_ip: HashMap::new(),
                fallback: Some(owner),
            }),
            Arc::new(DenialLog::new()),
        )
    }

    pub fn policy(&self) -> &Arc<AccessPolicy> {
        &self.policy
    }

    pub fn source(&self) -> Option<Arc<dyn PeerIdentitySource>> {
        self.source.clone()
    }

    /// Decide about a new connection from `ip`.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> GateCheck {
        if let Some(d) = self.cache.get(ip, now) {
            return match d {
                Decision::Allow(_) => GateCheck::Allow,
                Decision::Deny(reason) => {
                    let reason = reason.clone();
                    self.denials.record(now, ip, "quic", None, &reason);
                    GateCheck::Deny
                }
            };
        }
        let Some(source) = self.source.clone() else {
            self.denials
                .record(now, ip, "quic", None, &DenyReason::NoIdentitySource);
            return GateCheck::Deny;
        };
        if source.is_immediate() {
            let lookup = source.lookup(ip);
            return if self.resolve(ip, &lookup, now) {
                GateCheck::Allow
            } else {
                GateCheck::Deny
            };
        }
        if self.in_flight.insert(ip) {
            self.requested.push(ip);
        }
        GateCheck::Pending
    }

    /// Lookups to start, oldest first. Each IP appears once until resolved.
    pub fn take_requests(&mut self) -> Vec<IpAddr> {
        std::mem::take(&mut self.requested)
    }

    /// Whether a lookup for `ip` is outstanding.
    pub fn is_pending(&self, ip: IpAddr) -> bool {
        self.in_flight.contains(&ip)
    }

    /// Apply a finished lookup: decide, cache, log a refusal. Returns
    /// whether `ip` is admitted.
    pub fn resolve(&mut self, ip: IpAddr, lookup: &PeerLookup, now: Instant) -> bool {
        self.in_flight.remove(&ip);
        let decision = self.policy.decide(lookup);
        if let Decision::Deny(reason) = &decision {
            self.denials.record(now, ip, "quic", Some(lookup), reason);
        }
        let allowed = decision.is_allowed();
        self.cache.insert(ip, decision, now);
        allowed
    }
}

/// The web server's per-request check: same rule, same denial log, no
/// cache (Go does a fresh WhoIs per request).
pub fn authorize_web(
    policy: &AccessPolicy,
    denials: &DenialLog,
    now: Instant,
    peer_addr: &str,
    lookup: PeerLookup,
) -> bool {
    // Go hands over `r.RemoteAddr`, an `IP:port`. Anything else is refused.
    let Some(ip) = peer_addr
        .parse::<std::net::SocketAddr>()
        .map(|a| a.ip())
        .ok()
        .or_else(|| peer_addr.parse::<IpAddr>().ok())
    else {
        return false;
    };
    match policy.decide(&lookup) {
        Decision::Allow(_) => true,
        Decision::Deny(reason) => {
            denials.record(now, ip, "web", Some(&lookup), &reason);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: i64, login: &str) -> PeerIdentity {
        PeerIdentity {
            user_id: id,
            login_name: login.into(),
            node_name: format!("{login}-laptop"),
            tags: vec![],
        }
    }

    fn tagged(id: i64, tags: &[&str]) -> PeerIdentity {
        PeerIdentity {
            user_id: id,
            login_name: "tagged-devices".into(),
            node_name: "ci-box".into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn access(logins: &[&str], tags: &[&str]) -> AccessConfig {
        AccessConfig {
            allowed_logins: logins.iter().map(|s| s.to_string()).collect(),
            allowed_tags: tags.iter().map(|s| s.to_string()).collect(),
            rejected: vec![],
        }
    }

    const OWNER_ID: i64 = 100;

    fn owned_policy(logins: &[&str], tags: &[&str]) -> AccessPolicy {
        AccessPolicy::new(&user(OWNER_ID, "owner@example.com"), &access(logins, tags))
    }

    #[test]
    fn owner_is_admitted_by_default() {
        let p = owned_policy(&[], &[]);
        assert_eq!(
            p.decide(&Ok(user(OWNER_ID, "owner@example.com"))),
            Decision::Allow(AllowReason::Owner)
        );
    }

    #[test]
    fn another_user_is_refused_by_default() {
        let p = owned_policy(&[], &[]);
        assert_eq!(
            p.decide(&Ok(user(7, "mallory@example.com"))),
            Decision::Deny(DenyReason::UserNotAllowed)
        );
    }

    // The owner is matched by stable ID. A peer whose login name merely
    // reads like the owner's is someone else.
    #[test]
    fn owner_is_matched_by_user_id_not_login_name() {
        let p = owned_policy(&[], &[]);
        assert!(!p.decide(&Ok(user(7, "owner@example.com"))).is_allowed());
        assert!(p
            .decide(&Ok(user(OWNER_ID, "renamed@example.com")))
            .is_allowed());
    }

    #[test]
    fn listed_user_is_admitted_and_the_owner_still_is() {
        let p = owned_policy(&["guest@example.com"], &[]);
        assert_eq!(
            p.decide(&Ok(user(8, "guest@example.com"))),
            Decision::Allow(AllowReason::ListedUser)
        );
        assert!(p
            .decide(&Ok(user(OWNER_ID, "owner@example.com")))
            .is_allowed());
        assert!(!p.decide(&Ok(user(9, "other@example.com"))).is_allowed());
    }

    #[test]
    fn login_match_is_exact() {
        let p = owned_policy(&["guest@example.com"], &[]);
        assert!(!p.decide(&Ok(user(8, "Guest@example.com"))).is_allowed());
        assert!(!p.decide(&Ok(user(8, "guest@example.co"))).is_allowed());
    }

    #[test]
    fn listed_tag_admits_a_tagged_peer() {
        let p = owned_policy(&[], &["tag:ops"]);
        assert_eq!(
            p.decide(&Ok(tagged(5, &["tag:web", "tag:ops"]))),
            Decision::Allow(AllowReason::ListedTag("tag:ops".into()))
        );
        assert_eq!(
            p.decide(&Ok(tagged(5, &["tag:web"]))),
            Decision::Deny(DenyReason::TagsNotAllowed)
        );
    }

    // Tags replace the user. A tagged device whose (placeholder) user ID is
    // the owner's must not ride in as the owner, nor as a listed login.
    #[test]
    fn tagged_peer_never_passes_as_its_user() {
        let p = owned_policy(&["tagged-devices"], &[]);
        assert_eq!(
            p.decide(&Ok(tagged(OWNER_ID, &["tag:x"]))),
            Decision::Deny(DenyReason::TagsNotAllowed)
        );
    }

    #[test]
    fn tagged_node_with_no_list_admits_no_one() {
        let p = AccessPolicy::new(&tagged(1, &["tag:server"]), &AccessConfig::default());
        assert!(p.owner().is_none());
        assert!(p.admits_no_one());
        // Not even a peer of the user whose auth key tagged this node.
        assert!(!p.decide(&Ok(user(1, "keyowner@example.com"))).is_allowed());
        assert!(!p.decide(&Ok(tagged(1, &["tag:server"]))).is_allowed());
    }

    #[test]
    fn tagged_node_admits_only_what_is_listed() {
        let p = AccessPolicy::new(
            &tagged(1, &["tag:server"]),
            &access(&["alice@example.com"], &["tag:ci"]),
        );
        assert!(!p.admits_no_one());
        assert!(p.decide(&Ok(user(2, "alice@example.com"))).is_allowed());
        assert!(p.decide(&Ok(tagged(3, &["tag:ci"]))).is_allowed());
        assert!(!p.decide(&Ok(user(1, "keyowner@example.com"))).is_allowed());
    }

    #[test]
    fn lookup_errors_are_refused() {
        let p = owned_policy(&["guest@example.com"], &["tag:ops"]);
        assert_eq!(
            p.decide(&Err(WhoIsError::NotFound)),
            Decision::Deny(DenyReason::UnknownPeer)
        );
        assert!(matches!(
            p.decide(&Err(WhoIsError::Failed("timeout".into()))),
            Decision::Deny(DenyReason::LookupFailed(_))
        ));
    }

    // A lookup that came back with no user must not match an owner with no
    // user either: user_id 0 is "unknown", never an identity.
    #[test]
    fn zero_user_id_is_never_the_owner() {
        let p = AccessPolicy::new(&user(0, ""), &AccessConfig::default());
        assert!(p.owner().is_none());
        assert!(!p.decide(&Ok(user(0, ""))).is_allowed());
        let p = owned_policy(&[], &[]);
        assert!(!p.decide(&Ok(user(0, ""))).is_allowed());
    }

    #[test]
    fn empty_entries_match_nothing() {
        let p = AccessPolicy::from_parts(None, ["".to_string()], ["tag:".to_string()]);
        assert!(p.admits_no_one());
        assert!(!p.decide(&Ok(user(3, ""))).is_allowed());
    }

    // ---- cache and TTL ----

    #[test]
    fn cache_expires_allow_and_deny_on_their_own_ttls() {
        let t0 = Instant::now();
        let ip: IpAddr = "100.64.0.2".parse().unwrap();
        let ip2: IpAddr = "100.64.0.3".parse().unwrap();
        let mut c = DecisionCache::default();
        c.insert(ip, Decision::Allow(AllowReason::Owner), t0);
        c.insert(ip2, Decision::Deny(DenyReason::UserNotAllowed), t0);
        assert!(c
            .get(ip, t0 + ALLOW_TTL - Duration::from_millis(1))
            .is_some());
        assert!(c
            .get(ip2, t0 + DENY_TTL - Duration::from_millis(1))
            .is_some());
        assert!(c.get(ip2, t0 + DENY_TTL).is_none());
        assert!(c.get(ip, t0 + ALLOW_TTL).is_none());
        assert!(c.is_empty(), "expired entries are removed on lookup");
    }

    #[test]
    fn failed_lookups_are_not_cached() {
        let t0 = Instant::now();
        let ip: IpAddr = "100.64.0.2".parse().unwrap();
        let mut c = DecisionCache::default();
        c.insert(ip, Decision::Deny(DenyReason::LookupFailed("x".into())), t0);
        assert!(c.get(ip, t0).is_none());
    }

    /// A source that counts its lookups, blocking-style (not immediate).
    struct Counting {
        answer: PeerLookup,
        calls: Mutex<u32>,
        immediate: bool,
    }

    impl PeerIdentitySource for Counting {
        fn lookup(&self, _: IpAddr) -> PeerLookup {
            *self.calls.lock().unwrap() += 1;
            self.answer.clone()
        }
        fn is_immediate(&self) -> bool {
            self.immediate
        }
    }

    fn gate(answer: PeerLookup, immediate: bool) -> (PeerGate, Arc<Counting>) {
        let src = Arc::new(Counting {
            answer,
            calls: Mutex::new(0),
            immediate,
        });
        let g = PeerGate::new(
            Arc::new(owned_policy(&[], &[])),
            src.clone(),
            Arc::new(DenialLog::new()),
        );
        (g, src)
    }

    #[test]
    fn background_gate_holds_then_resolves_once_per_ip() {
        let t0 = Instant::now();
        let ip: IpAddr = "100.64.0.2".parse().unwrap();
        let (mut g, _src) = gate(Ok(user(OWNER_ID, "o")), false);
        assert_eq!(g.check(ip, t0), GateCheck::Pending);
        assert_eq!(g.check(ip, t0), GateCheck::Pending);
        assert_eq!(g.take_requests(), vec![ip], "one lookup per IP in flight");
        assert!(g.take_requests().is_empty());
        assert!(g.is_pending(ip));
        assert!(g.resolve(ip, &Ok(user(OWNER_ID, "o")), t0));
        assert!(!g.is_pending(ip));
        assert_eq!(g.check(ip, t0 + Duration::from_secs(1)), GateCheck::Allow);
        // After the TTL it asks again.
        assert_eq!(g.check(ip, t0 + ALLOW_TTL), GateCheck::Pending);
    }

    #[test]
    fn background_gate_caches_a_refusal() {
        let t0 = Instant::now();
        let ip: IpAddr = "100.64.0.9".parse().unwrap();
        let (mut g, _src) = gate(Ok(user(7, "m")), false);
        assert_eq!(g.check(ip, t0), GateCheck::Pending);
        assert!(!g.resolve(ip, &Ok(user(7, "m")), t0));
        assert_eq!(g.check(ip, t0 + Duration::from_secs(1)), GateCheck::Deny);
        assert_eq!(g.check(ip, t0 + DENY_TTL), GateCheck::Pending);
    }

    #[test]
    fn a_failed_lookup_is_asked_again_next_time() {
        let t0 = Instant::now();
        let ip: IpAddr = "100.64.0.9".parse().unwrap();
        let (mut g, _src) = gate(Err(WhoIsError::Failed("x".into())), false);
        assert_eq!(g.check(ip, t0), GateCheck::Pending);
        g.take_requests();
        assert!(!g.resolve(ip, &Err(WhoIsError::Failed("x".into())), t0));
        assert_eq!(g.check(ip, t0), GateCheck::Pending);
        assert_eq!(g.take_requests(), vec![ip]);
    }

    #[test]
    fn immediate_gate_decides_inline_and_caches() {
        let t0 = Instant::now();
        let ip: IpAddr = "100.64.0.2".parse().unwrap();
        let (mut g, src) = gate(Ok(user(OWNER_ID, "o")), true);
        assert_eq!(g.check(ip, t0), GateCheck::Allow);
        assert_eq!(g.check(ip, t0), GateCheck::Allow);
        assert_eq!(
            *src.calls.lock().unwrap(),
            1,
            "the second check is a cache hit"
        );
        assert!(g.take_requests().is_empty());

        let (mut g, _) = gate(Err(WhoIsError::NotFound), true);
        assert_eq!(g.check(ip, t0), GateCheck::Deny);
    }

    #[test]
    fn deny_all_gate_refuses_everyone() {
        let mut g = PeerGate::deny_all();
        assert_eq!(
            g.check("100.64.0.2".parse().unwrap(), Instant::now()),
            GateCheck::Deny
        );
        assert!(g.take_requests().is_empty());
    }

    #[test]
    fn test_gate_admits_through_the_real_rule() {
        let mut g = PeerGate::single_owner_for_tests();
        assert_eq!(
            g.check("127.0.0.1".parse().unwrap(), Instant::now()),
            GateCheck::Allow
        );
        assert_eq!(
            g.policy().decide(&Ok(user(2, "someone-else"))),
            Decision::Deny(DenyReason::UserNotAllowed)
        );
    }

    // ---- denial log ----

    #[test]
    fn denial_log_is_rate_limited_per_peer() {
        let t0 = Instant::now();
        let a: IpAddr = "100.64.0.2".parse().unwrap();
        let b: IpAddr = "100.64.0.3".parse().unwrap();
        let log = DenialLog::new();
        let r = DenyReason::UserNotAllowed;
        assert!(log.record(t0, a, "quic", None, &r));
        assert!(!log.record(t0 + Duration::from_secs(1), a, "quic", None, &r));
        assert!(!log.record(t0 + Duration::from_secs(2), a, "web", None, &r));
        assert!(
            log.record(t0, b, "quic", None, &r),
            "another peer is logged"
        );
        assert!(log.record(t0 + DENIAL_LOG_INTERVAL, a, "quic", None, &r));
    }

    #[test]
    fn denial_log_stays_bounded() {
        let t0 = Instant::now();
        let log = DenialLog::new();
        for i in 0..(DENIAL_LOG_CAPACITY as u32 * 3) {
            let ip = IpAddr::from(std::net::Ipv4Addr::from(0x6440_0000 + i));
            log.record(t0, ip, "quic", None, &DenyReason::UnknownPeer);
        }
        assert!(log.seen.lock().unwrap().len() <= DENIAL_LOG_CAPACITY);
    }

    // ---- web ----

    #[test]
    fn web_check_uses_the_same_rule() {
        let p = owned_policy(&[], &[]);
        let log = DenialLog::new();
        let now = Instant::now();
        assert!(authorize_web(
            &p,
            &log,
            now,
            "100.64.0.2:4433",
            Ok(user(OWNER_ID, "o"))
        ));
        assert!(!authorize_web(
            &p,
            &log,
            now,
            "100.64.0.2:4433",
            Ok(user(7, "m"))
        ));
        assert!(!authorize_web(
            &p,
            &log,
            now,
            "[fd7a::2]:1",
            Err(WhoIsError::NotFound)
        ));
        // An address that does not parse is refused even for the owner.
        assert!(!authorize_web(
            &p,
            &log,
            now,
            "garbage",
            Ok(user(OWNER_ID, "o"))
        ));
    }
}
