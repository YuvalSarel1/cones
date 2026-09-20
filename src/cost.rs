//! Report provenance and optional prices for reported usage. No harness execution or billing.
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const CATALOG_URL: &str = "https://models.dev/api.json";
const MAX_BYTES: u64 = 32 * 1024 * 1024;
const REFRESH_AFTER: i64 = 24 * 60 * 60;
const MAX_AGE: i64 = 7 * REFRESH_AFTER;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Harness,
    ModelsDev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogStamp {
    pub fetched_at: DateTime<Utc>,
    /// SHA-256 of the normalized price table, so an estimate identifies its exact rates.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub source: Source,
    pub coverage: Coverage,
    pub priced_records: u64,
    pub unpriced_records: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogStamp>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unpriced_reasons: BTreeMap<String, u64>,
}

impl Info {
    pub fn reported() -> Self {
        Self {
            source: Source::Harness,
            coverage: Coverage::Complete,
            priced_records: 1,
            unpriced_records: 0,
            catalog: None,
            unpriced_reasons: BTreeMap::new(),
        }
    }
}

pub fn valid(usd: f64) -> bool {
    usd.is_finite() && usd >= 0.0
}

pub fn display(usd: Option<f64>, info: Option<&Info>) -> String {
    let Some(usd) = usd.filter(|n| valid(*n)) else {
        return "-".into();
    };
    let prefix = if info.is_some_and(|i| i.source == Source::ModelsDev) {
        "~"
    } else {
        ""
    };
    // Gaps stay in `Info` for the details pane and JSON; the cell shows the amount alone.
    format!("{prefix}{}", crate::fleet::cost(usd))
}

pub fn describe(usd: Option<f64>, info: Option<&Info>) -> Option<String> {
    let info = info?;
    let source = match info.source {
        Source::Harness => "harness report".to_owned(),
        Source::ModelsDev => info.catalog.as_ref().map_or_else(
            || "price catalog unavailable".into(),
            |c| {
                format!(
                    "models.dev prices fetched {}",
                    c.fetched_at.format("%Y-%m-%d")
                )
            },
        ),
    };
    let reasons = info
        .unpriced_reasons
        .iter()
        .map(|(reason, count)| format!("{} ({count})", reason.replace('_', " ")))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Cost {} · {source} · {} priced records{}",
        display(usd, Some(info)),
        info.priced_records,
        if reasons.is_empty() {
            String::new()
        } else {
            format!(" · {reasons}")
        },
    ))
}

/// Native adapters translate reports; only this module decides how they are priced.
pub(crate) trait Adapter: std::fmt::Debug + Clone + Default + PartialEq {
    fn read<'a>(&'a mut self, event: &'a Value) -> Reading<'a>;
}

pub(crate) enum Reading<'a> {
    Ignore,
    Gap(&'static str),
    Response(Response<'a>),
}

pub(crate) struct Response<'a> {
    pub id: Option<&'a str>,
    pub reported_usd: Option<f64>,
    /// True only when the native counters explicitly describe an empty response.
    pub empty: bool,
    pub usage: std::result::Result<Usage<'a>, &'static str>,
    /// A native cumulative jump can reveal missing requests before this response.
    pub gap: Option<&'static str>,
}

/// Every registered harness constructs this interface with its native adapter.
pub(crate) trait Reader: std::fmt::Debug {
    fn observe(&mut self, event: &Value, catalog: Option<&Catalog>);
    fn report(&self, native_total: Option<f64>) -> (Option<f64>, Option<Info>);
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Accounting<A: Adapter> {
    adapter: A,
    total: Total,
    seen: HashSet<String>,
}

impl<A: Adapter> Reader for Accounting<A> {
    fn observe(&mut self, event: &Value, catalog: Option<&Catalog>) {
        let response = match self.adapter.read(event) {
            Reading::Ignore => return,
            Reading::Gap(reason) => {
                self.total.unknown(reason);
                return;
            }
            Reading::Response(response) => response,
        };
        if let Some(id) = response.id
            && !self.seen.insert(id.to_owned())
        {
            return;
        }
        if let Some(reason) = response.gap {
            self.total.unknown(reason);
        }
        if let Some(usd) = response
            .reported_usd
            .filter(|n| valid(*n) && (*n > 0.0 || response.empty))
        {
            self.total.reported(Some(usd), false);
            return;
        }
        match response.usage {
            Ok(usage) => self.total.estimate(&usage, catalog),
            Err(reason) => self.total.unknown(reason),
        }
    }

    fn report(&self, native_total: Option<f64>) -> (Option<f64>, Option<Info>) {
        prefer_native(native_total, self.total.report())
    }
}

/// A reported session total supersedes response accounting, including a reported zero.
pub(crate) fn prefer_native(
    native_total: Option<f64>,
    responses: (Option<f64>, Option<Info>),
) -> (Option<f64>, Option<Info>) {
    match native_total.filter(|n| valid(*n)) {
        Some(usd) => (Some(usd), Some(Info::reported())),
        None => responses,
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Total {
    usd: f64,
    priced: u64,
    estimated: bool,
    estimation_attempted: bool,
    reasons: BTreeMap<String, u64>,
    catalog: Option<CatalogStamp>,
}

impl Total {
    fn unknown(&mut self, reason: &str) {
        *self.reasons.entry(reason.into()).or_default() += 1;
    }

    fn reported(&mut self, usd: Option<f64>, zero_is_unpriced: bool) {
        match usd.filter(|n| valid(*n) && (!zero_is_unpriced || *n > 0.0)) {
            Some(n) if valid(self.usd + n) => {
                self.usd += n;
                self.priced += 1;
            }
            _ => self.unknown("unreported_or_unpriced"),
        }
    }

    fn estimate(&mut self, usage: &Usage<'_>, catalog: Option<&Catalog>) {
        self.estimation_attempted = true;
        let Some(catalog) = catalog else {
            self.unknown("catalog_unavailable");
            return;
        };
        self.catalog = Some(catalog.stamp.clone());
        match catalog.price(usage) {
            Ok(n) if valid(self.usd + n) => {
                self.usd += n;
                self.priced += 1;
                self.estimated = true;
            }
            Ok(_) => self.unknown("invalid_cost"),
            Err(reason) => self.unknown(reason),
        }
    }

    fn report(&self) -> (Option<f64>, Option<Info>) {
        let unpriced: u64 = self.reasons.values().sum();
        if self.priced == 0 && unpriced == 0 {
            return (None, None);
        }
        let coverage = if self.priced == 0 {
            Coverage::Unavailable
        } else if unpriced > 0 {
            Coverage::Partial
        } else {
            Coverage::Complete
        };
        (
            (self.priced > 0).then_some(self.usd),
            Some(Info {
                source: if self.estimated || (self.priced == 0 && self.estimation_attempted) {
                    Source::ModelsDev
                } else {
                    Source::Harness
                },
                coverage,
                priced_records: self.priced,
                unpriced_records: unpriced,
                catalog: self.catalog.clone(),
                unpriced_reasons: self.reasons.clone(),
            }),
        )
    }
}

/// Token classes are disjoint. Native readers supply the provider's reported model identity.
pub struct Usage<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Rates {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

impl Rates {
    fn valid(&self) -> bool {
        [self.input, self.output, self.cache_read, self.cache_write]
            .into_iter()
            .flatten()
            .all(valid)
    }

    fn price(&self, usage: &Usage<'_>) -> std::result::Result<f64, &'static str> {
        // Catalogs also use an all-zero table for unpriced/subscription models.
        if ![self.input, self.output, self.cache_read, self.cache_write]
            .into_iter()
            .flatten()
            .any(|n| n > 0.0)
        {
            return Err("unpriced_model");
        }
        let mut total = 0.0;
        for (tokens, rate) in [
            (usage.input, self.input),
            (usage.output, self.output),
            (usage.cache_read, self.cache_read),
            (usage.cache_write, self.cache_write),
        ] {
            if tokens > 0 {
                total += tokens as f64 * rate.ok_or("missing_rate")? / 1_000_000.0;
            }
        }
        valid(total).then_some(total).ok_or("invalid_cost")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Tier {
    above: u64,
    rates: Rates,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelPrice {
    base: Rates,
    tiers: Vec<Tier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    v: u32,
    pub stamp: CatalogStamp,
    models: BTreeMap<String, BTreeMap<String, ModelPrice>>,
}

impl Catalog {
    pub fn parse(bytes: &[u8], fetched_at: DateTime<Utc>) -> Result<Self> {
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "price catalog is too large"
        );
        let raw: Value = serde_json::from_slice(bytes)?;
        let providers = raw
            .as_object()
            .context("catalog providers must be an object")?;
        let mut models = BTreeMap::new();
        for (provider, entry) in providers {
            let Some(table) = entry["models"].as_object() else {
                continue;
            };
            let mut prices = BTreeMap::new();
            for (model, entry) in table {
                if let Ok(price) = Self::model(&entry["cost"]) {
                    prices.insert(model.clone(), price);
                }
            }
            if !prices.is_empty() {
                models.insert(provider.clone(), prices);
            }
        }
        ensure!(!models.is_empty(), "catalog has no valid prices");
        let sha256 = format!("{:x}", Sha256::digest(serde_json::to_vec(&models)?));
        Ok(Self {
            v: 1,
            stamp: CatalogStamp { fetched_at, sha256 },
            models,
        })
    }

    fn model(value: &Value) -> Result<ModelPrice> {
        let base: Rates = serde_json::from_value(value.clone())?;
        ensure!(base.valid(), "invalid base rates");
        let mut tiers = Vec::new();
        if let Some(values) = value.get("tiers") {
            for tier in values.as_array().context("invalid tiers")? {
                ensure!(tier["tier"]["type"] == "context", "unsupported price tier");
                let above = tier["tier"]["size"].as_u64().context("missing tier size")?;
                let rates: Rates = serde_json::from_value(tier.clone())?;
                ensure!(rates.valid(), "invalid tier rates");
                ensure!(
                    !tiers.iter().any(|t: &Tier| t.above == above),
                    "duplicate tier"
                );
                tiers.push(Tier { above, rates });
            }
        } else if let Some(legacy) = value.get("context_over_200k") {
            let rates: Rates = serde_json::from_value(legacy.clone())?;
            ensure!(rates.valid(), "invalid legacy tier");
            tiers.push(Tier {
                above: 200_000,
                rates,
            });
        }
        tiers.sort_by_key(|t| t.above);
        Ok(ModelPrice { base, tiers })
    }

    pub fn price(&self, usage: &Usage<'_>) -> std::result::Result<f64, &'static str> {
        let price = self
            .models
            .get(usage.provider)
            .and_then(|models| models.get(usage.model))
            .ok_or("unknown_provider_or_model")?;
        let context = usage
            .input
            .checked_add(usage.cache_read)
            .and_then(|n| n.checked_add(usage.cache_write))
            .ok_or("invalid_usage")?;
        price
            .tiers
            .iter()
            .rev()
            .find(|tier| context > tier.above)
            .map_or(&price.base, |tier| &tier.rates)
            .price(usage)
    }

    fn usable(&self, now: DateTime<Utc>) -> bool {
        let age = now
            .signed_duration_since(self.stamp.fetched_at)
            .num_seconds();
        (-300..=MAX_AGE).contains(&age)
    }

    fn read(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        crate::output::open_read(path)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "price snapshot is too large"
        );
        let catalog: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            catalog.v == 1 && !catalog.models.is_empty(),
            "invalid price snapshot"
        );
        ensure!(catalog.usable(Utc::now()), "price snapshot expired");
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&catalog.models)?));
        ensure!(
            digest == catalog.stamp.sha256,
            "price snapshot checksum mismatch"
        );
        for models in catalog.models.values() {
            for price in models.values() {
                ensure!(
                    price.base.valid() && price.tiers.iter().all(|t| t.rates.valid()),
                    "invalid cached rates"
                );
            }
        }
        Ok(catalog)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("price snapshot has no directory")?;
        crate::private_dir(parent)?;
        let temp = parent.join(format!(".prices-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temp)?;
            serde_json::to_writer(&mut file, self)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

struct Service {
    path: PathBuf,
    catalog: Option<Arc<Catalog>>,
    refresh: bool,
    next_attempt: Instant,
    busy: bool,
}

static SERVICE: Mutex<Option<Service>> = Mutex::new(None);

/// CLI readers load the cache only. A dashboard also refreshes it in a background thread.
pub fn init(state: &Path, refresh: bool) {
    let path = state.join("prices.json");
    let catalog = Catalog::read(&path).ok().map(Arc::new);
    *SERVICE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Service {
        path,
        catalog,
        refresh,
        next_attempt: Instant::now(),
        busy: false,
    });
    let _ = snapshot();
}

/// Only clones a snapshot and schedules due refreshes; rendering never downloads prices.
pub fn snapshot() -> Option<Arc<Catalog>> {
    let mut service = SERVICE.lock().unwrap_or_else(|e| e.into_inner());
    let active = service.as_mut()?;
    let now = Utc::now();
    let due = active.catalog.as_ref().is_none_or(|c| {
        now.signed_duration_since(c.stamp.fetched_at).num_seconds() >= REFRESH_AFTER
    });
    if active.refresh && due && !active.busy && Instant::now() >= active.next_attempt {
        active.busy = true;
        active.next_attempt = Instant::now() + Duration::from_secs(3600);
        let path = active.path.clone();
        std::thread::spawn(move || {
            let fetched = fetch().and_then(|catalog| {
                catalog.save(&path)?;
                Ok(Arc::new(catalog))
            });
            let mut service = SERVICE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = service.as_mut().filter(|s| s.path == path) {
                if let Ok(catalog) = fetched {
                    active.catalog = Some(catalog);
                }
                active.busy = false;
            }
        });
    }
    active.catalog.as_ref().filter(|c| c.usable(now)).cloned()
}

fn fetch() -> Result<Catalog> {
    let output = Command::new("/usr/bin/curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "5",
            "--max-time",
            "20",
            "--max-filesize",
            "33554432",
            CATALOG_URL,
        ])
        .stdin(Stdio::null())
        .output()
        .context("download model prices")?;
    if !output.status.success() {
        bail!("model price download failed");
    }
    Catalog::parse(&output.stdout, Utc::now())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn fixture() -> Catalog {
        Catalog::parse(&serde_json::to_vec(&json!({
            "provider": {"models": {
                "model": {"cost": {"input": 2, "output": 8, "cache_read": 0.5, "cache_write": 3,
                    "tiers": [{"tier":{"type":"context","size":100}, "input":4,"output":12,"cache_read":1,"cache_write":6}]}},
                "other_model": {"cost": {"input":1,"output":2,"cache_read":0.25,"cache_write":1.5}},
                "free-or-unknown": {"cost": {"input":0,"output":0,"cache_read":0,"cache_write":0}},
                "missing-cache": {"cost": {"input":2,"output":8}},
                "bad": {"cost": {"input":-1,"output":8}}
            }}
        })).unwrap(), Utc::now()).unwrap()
    }

    #[test]
    fn exact_model_rates_disjoint_caches_and_request_wide_tiers() {
        let catalog = fixture();
        let mut usage = Usage {
            provider: "provider",
            model: "model",
            input: 30,
            cache_read: 40,
            cache_write: 30,
            output: 10,
        };
        assert!((catalog.price(&usage).unwrap() - 0.00025).abs() < 1e-12);
        usage.input += 1;
        assert!((catalog.price(&usage).unwrap() - 0.000464).abs() < 1e-12);
        usage.provider = "other";
        assert_eq!(catalog.price(&usage), Err("unknown_provider_or_model"));
        usage.provider = "provider";
        usage.model = "missing-cache";
        assert_eq!(catalog.price(&usage), Err("missing_rate"));
        usage.model = "free-or-unknown";
        assert_eq!(catalog.price(&usage), Err("unpriced_model"));
        usage.model = "bad";
        assert!(catalog.price(&usage).is_err());
    }

    #[test]
    fn partial_and_unavailable_totals_are_distinct_from_zero() {
        let mut total = Total::default();
        total.reported(Some(0.25), true);
        total.reported(Some(0.0), true);
        total.reported(Some(f64::NAN), true);
        let (usd, info) = total.report();
        assert_eq!(usd, Some(0.25));
        assert_eq!(info.as_ref().unwrap().coverage, Coverage::Partial);
        assert_eq!(display(usd, info.as_ref()), "$0.25");
        let mut zero = Total::default();
        zero.reported(Some(0.0), false);
        assert_eq!(zero.report().0, Some(0.0));
        let mut missing = Total::default();
        missing.unknown("missing");
        assert_eq!(missing.report().0, None);
        assert_eq!(display(None, None), "-");
    }

    #[test]
    fn snapshots_validate_integrity_expiry_and_keep_the_previous_file_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prices.json");
        let mut catalog = fixture();
        catalog.save(&path).unwrap();
        assert_eq!(Catalog::read(&path).unwrap().stamp, catalog.stamp);
        assert!(Catalog::parse(b"{\"error\":\"unavailable\"}", Utc::now()).is_err());
        assert!(Catalog::read(&path).is_ok());
        catalog.stamp.fetched_at = Utc::now() - chrono::Duration::days(8);
        catalog.save(&path).unwrap();
        assert!(Catalog::read(&path).is_err());
        catalog.stamp.fetched_at = Utc::now();
        catalog.stamp.sha256 = "tampered".into();
        catalog.save(&path).unwrap();
        assert!(Catalog::read(&path).is_err());
    }
}
