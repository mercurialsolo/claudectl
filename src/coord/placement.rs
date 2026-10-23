//! Where a supervisor task runs (#571): this machine or a relay peer, chosen
//! by each machine's headroom against its slot policy.
//!
//! `place` is pure: it gets one view per machine, already measured, and says
//! where a task goes or why it waits. The views come from the sensors (this
//! machine probed, peers from their heartbeats) and the slot policy from
//! `~/.claudectl/coord/policy.toml`:
//!
//! ```toml
//! stale_after_secs = 90        # a peer heard from longer ago than this is skipped
//!
//! [local]                      # this machine
//! max_tasks = 4                # what `max_concurrent` was
//! min_free_mem_mb = 2048       # don't start a task below this much free memory
//! max_load_per_core = 1.5
//! min_free_disk_mb = 5120
//! mem_per_task_mb = 1024       # assumed per task placed before it's measured
//! drain = false                # true: start nothing here (as `supervisor drain`)
//!
//! [peers]                      # defaults for every peer
//! max_tasks = 2
//!
//! [peers.baradas-mac-mini-508f9c0e]   # overrides for one peer
//! max_tasks = 6
//! ```
//!
//! Every floor is optional. A floor that's set and a measurement that's
//! missing is not room: an unknown is never treated as plenty.

use std::collections::{BTreeMap, HashMap};

use claudectl_core::headroom::Headroom;

/// One machine's limits. `None` is no limit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlotPolicy {
    pub max_tasks: Option<u32>,
    pub min_free_mem_mb: Option<u64>,
    pub max_load_per_core: Option<f64>,
    pub min_free_disk_mb: Option<u64>,
    pub mem_per_task_mb: Option<u64>,
    pub drain: bool,
}

impl SlotPolicy {
    /// `self`, with each field it leaves unset taken from `defaults`.
    #[cfg_attr(not(feature = "relay"), allow(dead_code))]
    fn over(&self, defaults: &SlotPolicy) -> SlotPolicy {
        SlotPolicy {
            max_tasks: self.max_tasks.or(defaults.max_tasks),
            min_free_mem_mb: self.min_free_mem_mb.or(defaults.min_free_mem_mb),
            max_load_per_core: self.max_load_per_core.or(defaults.max_load_per_core),
            min_free_disk_mb: self.min_free_disk_mb.or(defaults.min_free_disk_mb),
            mem_per_task_mb: self.mem_per_task_mb.or(defaults.mem_per_task_mb),
            drain: self.drain || defaults.drain,
        }
    }
}

/// What `policy.toml` says.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacementPolicy {
    pub stale_after_secs: u64,
    /// Route a task with no `role` to the bound role that owns its paths
    /// (`coord::route`), when that role is live. Off by default: `init`
    /// binds a role at every project root, and turning this on hands those
    /// sessions work that used to start headless.
    pub auto_route: bool,
    pub local: SlotPolicy,
    pub peer_defaults: SlotPolicy,
    pub peers: HashMap<String, SlotPolicy>,
}

/// Tasks this machine runs at once when `policy.toml` doesn't say: the old
/// `max_concurrent` default.
pub const DEFAULT_LOCAL_MAX_TASKS: u32 = 4;
/// Three heartbeats at the default 30s interval. Shared with `relay peers`
/// and the TUI, so all three agree on which machines are live.
pub const DEFAULT_STALE_AFTER_SECS: u64 = claudectl_core::runtime::PEER_STALE_AFTER_SECS;

impl Default for PlacementPolicy {
    fn default() -> Self {
        Self {
            stale_after_secs: DEFAULT_STALE_AFTER_SECS,
            auto_route: false,
            local: SlotPolicy {
                max_tasks: Some(DEFAULT_LOCAL_MAX_TASKS),
                ..SlotPolicy::default()
            },
            peer_defaults: SlotPolicy::default(),
            peers: HashMap::new(),
        }
    }
}

impl PlacementPolicy {
    /// One peer's limits: its own section over the `[peers]` defaults.
    #[cfg_attr(not(feature = "relay"), allow(dead_code))]
    pub fn for_peer(&self, peer: &str) -> SlotPolicy {
        match self.peers.get(peer) {
            Some(own) => own.over(&self.peer_defaults),
            None => self.peer_defaults.clone(),
        }
    }
}

pub fn policy_path() -> std::path::PathBuf {
    super::store::coord_dir().join("policy.toml")
}

/// The policy from `policy.toml`; the defaults when there is none. A file
/// that doesn't parse is an error, not the defaults: a typo must not quietly
/// lift every limit the file set.
pub fn load_policy() -> Result<PlacementPolicy, String> {
    match std::fs::read_to_string(policy_path()) {
        Ok(text) => parse_policy(&text).map_err(|e| format!("{}: {e}", policy_path().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PlacementPolicy::default()),
        Err(e) => Err(format!("{}: {e}", policy_path().display())),
    }
}

/// The `policy.toml` subset above: three kinds of table, numbers and
/// booleans. Unknown keys and tables are errors, so a misspelt floor is
/// reported rather than ignored.
pub fn parse_policy(text: &str) -> Result<PlacementPolicy, String> {
    let mut policy = PlacementPolicy::default();
    // Which table the next key belongs to.
    enum Table {
        Top,
        Local,
        PeerDefaults,
        Peer(String),
    }
    let mut table = Table::Top;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let at = |msg: String| format!("line {}: {msg}", n + 1);
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim();
            table = match name {
                "local" => Table::Local,
                "peers" => Table::PeerDefaults,
                _ => match name.strip_prefix("peers.") {
                    Some(id) => {
                        let id = id.trim().trim_matches('"');
                        if !valid_peer_name(id) {
                            return Err(at(format!("[peers.{id}]: not a peer id")));
                        }
                        policy.peers.entry(id.to_string()).or_default();
                        Table::Peer(id.to_string())
                    }
                    None => return Err(at(format!("unknown table [{name}]"))),
                },
            };
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .map(|(k, v)| (k.trim(), v.trim()))
            .ok_or_else(|| at(format!("expected key = value, got `{line}`")))?;
        let slots = match &table {
            Table::Top => {
                match key {
                    "stale_after_secs" => {
                        policy.stale_after_secs = value
                            .parse()
                            .map_err(|_| at(format!("{key}: not a number: {value}")))?
                    }
                    "auto_route" => {
                        policy.auto_route = match value {
                            "true" => true,
                            "false" => false,
                            _ => return Err(at(format!("{key}: not true or false: {value}"))),
                        }
                    }
                    _ => return Err(at(format!("unknown key `{key}` outside a table"))),
                }
                continue;
            }
            Table::Local => &mut policy.local,
            Table::PeerDefaults => &mut policy.peer_defaults,
            Table::Peer(id) => policy.peers.get_mut(id).expect("inserted with its header"),
        };
        set_slot(slots, key, value).map_err(at)?;
    }
    Ok(policy)
}

fn set_slot(s: &mut SlotPolicy, key: &str, value: &str) -> Result<(), String> {
    let int = || {
        value
            .parse::<u64>()
            .map_err(|_| format!("{key}: not a whole number: {value}"))
    };
    match key {
        "max_tasks" => s.max_tasks = Some(int()? as u32),
        "min_free_mem_mb" => s.min_free_mem_mb = Some(int()?),
        "min_free_disk_mb" => s.min_free_disk_mb = Some(int()?),
        "mem_per_task_mb" => s.mem_per_task_mb = Some(int()?),
        "max_load_per_core" => {
            s.max_load_per_core = Some(
                value
                    .parse()
                    .map_err(|_| format!("{key}: not a number: {value}"))?,
            )
        }
        "drain" => {
            s.drain = match value {
                "true" => true,
                "false" => false,
                _ => return Err(format!("{key}: not true or false: {value}")),
            }
        }
        _ => return Err(format!("unknown key `{key}`")),
    }
    Ok(())
}

/// A relay peer id's shape, checked here too so coord doesn't need relay to
/// read a policy file.
pub fn valid_peer_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// A tag or tool name in a task's `requires`: a word, optionally with
/// `>=<version>` and/or `:auth` (tools only).
pub fn validate_requirement(r: &str) -> Result<(), String> {
    let head = r.strip_suffix(":auth").unwrap_or(r);
    let (name, version) = match head.split_once(">=") {
        Some((n, v)) => (n, Some(v)),
        None => (head, None),
    };
    if !valid_peer_name(name) || name.len() > 40 {
        return Err(format!(
            "requires `{r}`: `{name}` is not a tool or tag name"
        ));
    }
    if version.is_some_and(|v| v.is_empty() || !v.chars().all(|c| c.is_ascii_digit() || c == '.')) {
        return Err(format!("requires `{r}`: the version must be like 1.88"));
    }
    Ok(())
}

/// A probed tool, as placement needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub present: bool,
    pub version: Option<String>,
    pub credential: Option<bool>,
}

/// One machine as placement sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerView {
    /// `None` is this machine.
    pub peer: Option<String>,
    pub headroom: Headroom,
    pub tags: Vec<String>,
    /// What its relay probed. Empty when nothing has been heard.
    pub tools: BTreeMap<String, Tool>,
    /// Heard from too long ago to act on.
    pub stale: bool,
    pub slots: SlotPolicy,
    /// Tasks placed on it earlier in this pass, not yet in its headroom.
    pub planned: u32,
}

impl PeerView {
    /// This machine with no measurements and only a task ceiling: what the
    /// reconciler did before placement.
    pub fn local_with_ceiling(max_tasks: u32) -> Self {
        PeerView {
            peer: None,
            headroom: Headroom::default(),
            tags: Vec::new(),
            tools: BTreeMap::new(),
            stale: false,
            slots: SlotPolicy {
                max_tasks: Some(max_tasks),
                ..SlotPolicy::default()
            },
            planned: 0,
        }
    }

    fn name(&self) -> &str {
        self.peer.as_deref().unwrap_or("this machine")
    }

    /// Free memory once the tasks planned on it this pass are counted.
    fn mem_after_planned(&self) -> Option<u64> {
        let per = self.slots.mem_per_task_mb.unwrap_or(0);
        self.headroom
            .mem_free_mb
            .map(|m| m.saturating_sub(per * self.planned as u64))
    }

    /// Why it can't take a task with these requirements now, or `None` when
    /// it can.
    fn refusal(&self, requires: &[String]) -> Option<String> {
        if self.stale {
            return Some(format!("{} has not been heard from recently", self.name()));
        }
        if self.slots.drain {
            return Some(format!("{} is draining", self.name()));
        }
        if let Some(r) = requires.iter().find(|r| !self.meets(r)) {
            return Some(format!("{} lacks `{r}`", self.name()));
        }
        let running = self.headroom.running_tasks + self.planned;
        if let Some(max) = self.slots.max_tasks {
            if running >= max {
                return Some(format!(
                    "{} is running {running} of {max} tasks",
                    self.name()
                ));
            }
        }
        if let Some(floor) = self.slots.min_free_mem_mb {
            let after = self
                .mem_after_planned()
                .map(|m| m.saturating_sub(self.slots.mem_per_task_mb.unwrap_or(0)));
            if after.is_none_or(|m| m < floor) {
                return Some(format!(
                    "{} would go below {floor} MB free memory",
                    self.name()
                ));
            }
        }
        if let Some(max) = self.slots.max_load_per_core {
            if self.headroom.load_per_core.is_none_or(|l| l > max) {
                return Some(format!("{} is above {max} load per core", self.name()));
            }
        }
        if let Some(floor) = self.slots.min_free_disk_mb {
            if self.headroom.disk_free_mb.is_none_or(|d| d < floor) {
                return Some(format!("{} has under {floor} MB free disk", self.name()));
            }
        }
        None
    }

    /// Whether it has what one `requires` entry names: a tag it advertises,
    /// or a probed tool at that version with a credential when `:auth`. A
    /// name that's neither is not met: "never heard of it" is not "has it".
    fn meets(&self, req: &str) -> bool {
        if self.tags.iter().any(|t| t == req) {
            return true;
        }
        let (head, auth) = match req.strip_suffix(":auth") {
            Some(h) => (h, true),
            None => (req, false),
        };
        let (name, min) = match head.split_once(">=") {
            Some((n, v)) => (n, Some(v)),
            None => (head, None),
        };
        let Some(tool) = self.tools.get(name).filter(|t| t.present) else {
            return false;
        };
        if auth && tool.credential != Some(true) {
            return false;
        }
        match (min, &tool.version) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(min), Some(have)) => !version_lt(have, min),
        }
    }
}

/// Dotted-number comparison; a part that isn't a number compares as 0.
fn version_lt(have: &str, want: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    let (h, w) = (parts(have), parts(want));
    for i in 0..h.len().max(w.len()) {
        let (a, b) = (
            h.get(i).copied().unwrap_or(0),
            w.get(i).copied().unwrap_or(0),
        );
        if a != b {
            return a < b;
        }
    }
    false
}

/// Every machine as the supervisor's sensor sees it now: this one probed,
/// each relay peer from its last heartbeat. Limits come from `policy.toml`
/// and the `supervisor drain` marker. A policy file that doesn't parse
/// drains this machine with the error as the reason: an unreadable limit is
/// not no limit.
pub fn live_views() -> Vec<PeerView> {
    let (policy, policy_error) = match load_policy() {
        Ok(p) => (p, None),
        Err(e) => (PlacementPolicy::default(), Some(e)),
    };
    let tags = crate::config::Config::load().relay.unwrap_or_default().tags;
    let mut local = PeerView {
        peer: None,
        headroom: claudectl_core::headroom::probe(&super::store::coord_dir(), 0),
        tags,
        tools: local_tools(),
        stale: false,
        slots: policy.local.clone(),
        planned: 0,
    };
    if super::supervisor_cli::is_draining() || policy_error.is_some() {
        local.slots.drain = true;
    }
    // Said once per distinct error, not on every pass of a daemon.
    static LAST_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    if let Ok(mut last) = LAST_ERROR.lock() {
        if *last != policy_error {
            if let Some(e) = &policy_error {
                eprintln!("supervisor: {e}; starting nothing here until it's fixed");
            }
            *last = policy_error;
        }
    }
    let mut views = vec![local];
    views.extend(peer_views(&policy));
    views
}

#[cfg(feature = "relay")]
fn tools_from(caps: &crate::relay::capabilities::Capabilities) -> BTreeMap<String, Tool> {
    use crate::relay::capabilities::ToolState;
    caps.tools
        .iter()
        .map(|(name, state)| {
            let tool = match state {
                ToolState::Present {
                    version,
                    credential,
                } => Tool {
                    present: true,
                    version: version.clone(),
                    credential: *credential,
                },
                ToolState::Missing => Tool {
                    present: false,
                    version: None,
                    credential: None,
                },
            };
            (name.clone(), tool)
        })
        .collect()
}

#[cfg(feature = "relay")]
fn local_tools() -> BTreeMap<String, Tool> {
    tools_from(&crate::relay::capabilities::local())
}

/// Without the relay there's no tool probe: a task can require tags only.
#[cfg(not(feature = "relay"))]
fn local_tools() -> BTreeMap<String, Tool> {
    BTreeMap::new()
}

#[cfg(feature = "relay")]
fn peer_views(policy: &PlacementPolicy) -> Vec<PeerView> {
    let now = crate::relay::epoch_ms();
    crate::relay::headroom::load_all()
        .into_iter()
        // A forgotten peer still connected keeps heartbeating, and the
        // running relay rewrites its `.headroom`; without its key a
        // delegation to it can only fail.
        .filter(|r| crate::relay::load_peer_psk(&r.peer).is_some())
        .map(|r| PeerView {
            stale: !claudectl_core::runtime::PeerLiveness::classify(
                Some(r.received_ms),
                now,
                policy.stale_after_secs,
            )
            .is_live(),
            // The lower of what this machine allows it and what it allows
            // itself: it refuses work past its own.
            slots: {
                let mut s = policy.for_peer(&r.peer);
                s.max_tasks = match (s.max_tasks, r.advertised.headroom.max_tasks) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                s
            },
            tools: crate::relay::capabilities::load_peer_capabilities(&r.peer)
                .map(|c| tools_from(&c))
                .unwrap_or_default(),
            headroom: r.advertised.headroom,
            tags: r.advertised.tags,
            peer: Some(r.peer),
            planned: 0,
        })
        .collect()
}

#[cfg(not(feature = "relay"))]
fn peer_views(_policy: &PlacementPolicy) -> Vec<PeerView> {
    Vec::new()
}

/// Where a task goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Local,
    Remote(String),
    /// Nowhere has room for it now; the reason names why for each machine.
    Wait(String),
}

/// Place one task on the eligible machine with the most free memory (after
/// what's been planned on it this pass), and count it there. Ties go to the
/// machine running fewer tasks, then this machine, then by peer id. A task
/// with `peer` set is considered for that peer only.
pub fn place(views: &mut [PeerView], requires: &[String], pin: Option<&str>) -> Decision {
    let mut refusals = Vec::new();
    let mut best: Option<usize> = None;
    for (i, v) in views.iter().enumerate() {
        if let Some(p) = pin {
            if v.peer.as_deref() != Some(p) {
                continue;
            }
        }
        if let Some(why) = v.refusal(requires) {
            refusals.push(why);
            continue;
        }
        best = match best {
            None => Some(i),
            Some(b) => Some(if better(v, &views[b]) { i } else { b }),
        };
    }
    match best {
        Some(i) => {
            views[i].planned += 1;
            match &views[i].peer {
                None => Decision::Local,
                Some(p) => Decision::Remote(p.clone()),
            }
        }
        None if refusals.is_empty() => Decision::Wait(match pin {
            Some(p) => format!("peer {p} is not connected"),
            None => "no machine to run it on".into(),
        }),
        None => Decision::Wait(refusals.join("; ")),
    }
}

fn better(a: &PeerView, b: &PeerView) -> bool {
    let key = |v: &PeerView| {
        (
            v.mem_after_planned().unwrap_or(0),
            std::cmp::Reverse(v.headroom.running_tasks + v.planned),
            v.peer.is_none(),
            std::cmp::Reverse(v.peer.clone()),
        )
    };
    key(a) > key(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(peer: Option<&str>, mem: u64, running: u32, slots: SlotPolicy) -> PeerView {
        PeerView {
            peer: peer.map(String::from),
            headroom: Headroom {
                mem_free_mb: Some(mem),
                load_per_core: Some(0.2),
                disk_free_mb: Some(50_000),
                running_tasks: running,
                max_tasks: None,
            },
            tags: Vec::new(),
            tools: BTreeMap::new(),
            stale: false,
            slots,
            planned: 0,
        }
    }

    fn floor(min_free: u64, per_task: u64) -> SlotPolicy {
        SlotPolicy {
            min_free_mem_mb: Some(min_free),
            mem_per_task_mb: Some(per_task),
            ..SlotPolicy::default()
        }
    }

    /// #571's acceptance: two machines with different floors; ten tasks are
    /// spread so neither goes below its floor, and the rest wait.
    #[test]
    fn ten_tasks_spread_without_crossing_either_memory_floor() {
        let mut views = vec![
            view(None, 8_000, 0, floor(4_000, 1_000)),
            view(Some("mini"), 6_000, 0, floor(1_000, 1_000)),
        ];
        let decisions: Vec<Decision> = (0..10).map(|_| place(&mut views, &[], None)).collect();
        let local = decisions.iter().filter(|d| **d == Decision::Local).count();
        let mini = decisions
            .iter()
            .filter(|d| **d == Decision::Remote("mini".into()))
            .count();
        // 8000 - 4 * 1000 = 4000: local takes four. 6000 - 5 * 1000 = 1000: mini takes five.
        assert_eq!((local, mini), (4, 5), "{decisions:?}");
        assert!(matches!(decisions.last(), Some(Decision::Wait(w)) if w.contains("below")));
        for v in &views {
            let left = v.headroom.mem_free_mb.unwrap() - v.planned as u64 * 1_000;
            assert!(left >= v.slots.min_free_mem_mb.unwrap(), "{v:?}");
        }
    }

    #[test]
    fn the_machine_with_more_free_memory_goes_first() {
        let mut views = vec![
            view(None, 2_000, 0, SlotPolicy::default()),
            view(Some("mini"), 9_000, 0, SlotPolicy::default()),
        ];
        assert_eq!(
            place(&mut views, &[], None),
            Decision::Remote("mini".into())
        );
    }

    /// #571's acceptance: a task needing `docker` lands only where it's
    /// advertised, and waits when that machine has no room.
    #[test]
    fn a_tag_requirement_goes_only_where_the_tag_is_and_waits_when_full() {
        let mut docker = view(
            Some("box"),
            1_000,
            0,
            SlotPolicy {
                max_tasks: Some(1),
                ..SlotPolicy::default()
            },
        );
        docker.tags = vec!["docker".into()];
        let mut views = vec![view(None, 64_000, 0, SlotPolicy::default()), docker];
        let req = vec!["docker".to_string()];
        assert_eq!(
            place(&mut views, &req, None),
            Decision::Remote("box".into())
        );
        match place(&mut views, &req, None) {
            Decision::Wait(why) => {
                assert!(why.contains("this machine lacks `docker`"), "{why}");
                assert!(why.contains("box is running 1 of 1"), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_requirements_check_version_and_credential_and_unknown_is_unmet() {
        let mut v = view(None, 1_000, 0, SlotPolicy::default());
        v.tools.insert(
            "node".into(),
            Tool {
                present: true,
                version: Some("22.3.0".into()),
                credential: None,
            },
        );
        v.tools.insert(
            "claude".into(),
            Tool {
                present: true,
                version: Some("2.1.0".into()),
                credential: Some(false),
            },
        );
        assert!(v.meets("node") && v.meets("node>=22") && !v.meets("node>=23"));
        assert!(v.meets("claude") && !v.meets("claude:auth"));
        assert!(!v.meets("cargo"), "never probed is not present");
        assert!(!v.meets("gpu"), "no such tag");
    }

    #[test]
    fn a_pinned_task_waits_for_its_peer_even_with_room_elsewhere() {
        let mut views = vec![
            view(None, 64_000, 0, SlotPolicy::default()),
            view(
                Some("mini"),
                64_000,
                0,
                SlotPolicy {
                    drain: true,
                    ..SlotPolicy::default()
                },
            ),
        ];
        assert_eq!(
            place(&mut views, &[], Some("mini")),
            Decision::Wait("mini is draining".into())
        );
        assert_eq!(
            place(&mut views, &[], Some("gone")),
            Decision::Wait("peer gone is not connected".into())
        );
    }

    #[test]
    fn stale_or_unmeasured_machines_are_not_room() {
        let mut stale = view(Some("old"), 64_000, 0, SlotPolicy::default());
        stale.stale = true;
        let mut unmeasured = view(None, 0, 0, floor(1, 0));
        unmeasured.headroom.mem_free_mb = None;
        let mut views = vec![unmeasured, stale];
        match place(&mut views, &[], None) {
            Decision::Wait(why) => {
                assert!(why.contains("below 1 MB"), "{why}");
                assert!(why.contains("old has not been heard from"), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn with_no_policy_this_machine_runs_four_at_a_time_as_before() {
        let mut views = vec![PeerView::local_with_ceiling(DEFAULT_LOCAL_MAX_TASKS)];
        let placed = (0..6)
            .map(|_| place(&mut views, &[], None))
            .filter(|d| *d == Decision::Local)
            .count();
        assert_eq!(placed, 4);
    }

    #[test]
    fn the_policy_file_reads_tables_and_peer_overrides() {
        let p = parse_policy(
            "stale_after_secs = 60\n[local]\nmax_tasks = 3\nmin_free_mem_mb = 2048 # floor\n\
             drain = true\n[peers]\nmax_tasks = 2\nmax_load_per_core = 1.5\n\
             [peers.mini-508f]\nmax_tasks = 6\n",
        )
        .unwrap();
        assert_eq!(p.stale_after_secs, 60);
        assert_eq!(p.local.max_tasks, Some(3));
        assert_eq!(p.local.min_free_mem_mb, Some(2048));
        assert!(p.local.drain);
        let mini = p.for_peer("mini-508f");
        assert_eq!(
            (mini.max_tasks, mini.max_load_per_core),
            (Some(6), Some(1.5))
        );
        assert_eq!(p.for_peer("other").max_tasks, Some(2));
        assert_eq!(parse_policy("").unwrap(), PlacementPolicy::default());
    }

    /// #596 follow-up: routing a roleless task to its owner is opt-in.
    #[test]
    fn auto_route_is_off_unless_the_policy_file_turns_it_on() {
        assert!(!PlacementPolicy::default().auto_route);
        assert!(parse_policy("auto_route = true\n").unwrap().auto_route);
        assert!(
            !parse_policy("auto_route = false\n[local]\nmax_tasks = 1\n")
                .unwrap()
                .auto_route
        );
        assert!(parse_policy("auto_route = yes\n").is_err());
        assert!(parse_policy("auto_rout = true\n").is_err());
        // It is a top-level key, not a slot limit.
        assert!(parse_policy("[local]\nauto_route = true\n").is_err());
    }

    #[test]
    fn a_policy_file_with_a_typo_is_an_error_not_the_defaults() {
        for bad in [
            "[local]\nmin_free_mem = 1",
            "[locals]\n",
            "[local]\nmax_tasks = many",
            "max_tasks = 3",
            "[local]\ndrain = yes",
            "[peers.a b]\n",
        ] {
            assert!(parse_policy(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn requirement_names_are_validated() {
        for ok in ["docker", "node>=22", "claude:auth", "cargo>=1.88:auth"] {
            assert!(validate_requirement(ok).is_ok(), "{ok}");
        }
        for bad in ["", "node>=", "node>=x", "a b", "--force"] {
            assert!(validate_requirement(bad).is_err(), "{bad}");
        }
    }
}
