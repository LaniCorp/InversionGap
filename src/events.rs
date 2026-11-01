//! Synthetic "high-frequency event triage" payload: deterministic security/system events.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// SplitMix64 — tiny, fast, deterministic. Used for event generation and gateway jitter.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// Inclusive range.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo { lo } else { lo + self.below(hi - lo + 1) }
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    BruteForce,
    PortScan,
    PrivilegeEscalation,
    MalwareHash,
    DnsExfiltration,
    ImpossibleTravel,
    SuspiciousPowershell,
    LateralMovement,
    DiskPressure,
    ServiceCrashLoop,
    CertExpiry,
    BenignLogin,
}

const KINDS: [EventKind; 12] = [
    EventKind::BruteForce,
    EventKind::PortScan,
    EventKind::PrivilegeEscalation,
    EventKind::MalwareHash,
    EventKind::DnsExfiltration,
    EventKind::ImpossibleTravel,
    EventKind::SuspiciousPowershell,
    EventKind::LateralMovement,
    EventKind::DiskPressure,
    EventKind::ServiceCrashLoop,
    EventKind::CertExpiry,
    EventKind::BenignLogin,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityEvent {
    pub id: u32,
    pub ts: DateTime<Utc>,
    pub kind: EventKind,
    pub source: String,
    pub host: String,
    pub src_ip: String,
    pub user: String,
    pub detail: String,
}

impl SecurityEvent {
    /// What the triage model sees. The `kind` label is deliberately withheld.
    pub fn summary(&self) -> String {
        format!(
            "[{}] source={} host={} src_ip={} user={} :: {}",
            self.ts.to_rfc3339(),
            self.source,
            self.host,
            self.src_ip,
            self.user,
            self.detail
        )
    }
}

pub fn generate(count: usize, seed: u64) -> Vec<SecurityEvent> {
    const HOSTS: [&str; 8] = [
        "db-prod-02", "web-edge-11", "k8s-node-07", "dc01.corp.local",
        "build-agent-3", "laptop-nl-114", "vault-01", "gw-fw-02",
    ];
    const USERS: [&str; 8] = ["root", "svc_backup", "nils", "admin", "jenkins", "guest", "SYSTEM", "a.morgan"];

    let mut rng = Rng::new(seed);
    let base = Utc::now();
    (0..count)
        .map(|i| {
            let kind = *rng.pick(&KINDS);
            let host = rng.pick(&HOSTS).to_string();
            let user = rng.pick(&USERS).to_string();
            let src_ip = if rng.below(3) == 0 {
                format!("10.{}.{}.{}", rng.below(255), rng.below(255), rng.range(1, 254))
            } else {
                format!("{}.{}.{}.{}", rng.range(23, 223), rng.below(255), rng.below(255), rng.range(1, 254))
            };
            let (source, detail) = describe(kind, &mut rng, &host);
            SecurityEvent {
                id: i as u32 + 1,
                ts: base + Duration::milliseconds(i as i64 * 17),
                kind,
                source: source.to_string(),
                host,
                src_ip,
                user,
                detail,
            }
        })
        .collect()
}

fn describe(kind: EventKind, rng: &mut Rng, host: &str) -> (&'static str, String) {
    match kind {
        EventKind::BruteForce => (
            "sshd",
            format!("{} failed password attempts in {}s, then 1 success", rng.range(20, 400), rng.range(30, 120)),
        ),
        EventKind::PortScan => (
            "fw",
            format!("SYN sweep across {} ports on {} in {}ms", rng.range(200, 65000), host, rng.range(300, 4000)),
        ),
        EventKind::PrivilegeEscalation => (
            "auditd",
            format!("uid change 1001->0 via unexpected setuid binary /tmp/.x{}", rng.below(999)),
        ),
        EventKind::MalwareHash => (
            "edr",
            format!("process hash matches known ransomware family (sha256 prefix {:08x})", rng.next_u64() as u32),
        ),
        EventKind::DnsExfiltration => (
            "dns",
            format!("{} TXT queries with 60+ char base32 labels to a single new domain", rng.range(300, 5000)),
        ),
        EventKind::ImpossibleTravel => (
            "idp",
            format!("sign-in from two countries {} minutes apart", rng.range(3, 40)),
        ),
        EventKind::SuspiciousPowershell => (
            "sysmon",
            "powershell -nop -w hidden -enc <base64 blob> spawned by winword.exe".to_string(),
        ),
        EventKind::LateralMovement => (
            "edr",
            format!("remote service creation over SMB to {} hosts within 2 minutes", rng.range(3, 30)),
        ),
        EventKind::DiskPressure => (
            "node-exporter",
            format!("/var at {}% used, growing {}MB/min", rng.range(88, 99), rng.range(50, 900)),
        ),
        EventKind::ServiceCrashLoop => (
            "systemd",
            format!("payments-api restarted {} times in 5 minutes (exit code 137)", rng.range(5, 40)),
        ),
        EventKind::CertExpiry => (
            "cert-monitor",
            format!("TLS certificate for {}.corp.local expires in {} days", host, rng.range(1, 14)),
        ),
        EventKind::BenignLogin => (
            "idp",
            "interactive sign-in from managed device, MFA satisfied, usual location".to_string(),
        ),
    }
}
