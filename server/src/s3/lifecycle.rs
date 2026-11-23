//! S3 bucket lifecycle rules: object expiry (TTL), noncurrent-version expiry
//! and aborting incomplete multipart uploads.
//!
//! A bucket's rules are kept as their XML with the bucket. This module parses
//! and validates them, writes them back for GET, decides which rule applies to
//! an object, and computes the `x-amz-expiration` header. The expirer
//! (`service::lifecycle_worker`) applies them.
//!
//! Transitions (storage classes) are accepted and stored but never applied:
//! WarpDrive has one storage class.

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};

use super::handlers::common::{extract_all_xml_tags, extract_xml_tag};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Filter {
    pub prefix: String,
    pub tags: Vec<(String, String)>,
    pub size_gt: Option<u64>,
    pub size_lt: Option<u64>,
    /// How the rule was written, so GET returns it the same way: the legacy
    /// rule-level `<Prefix>`, or a `<Filter>` (possibly empty).
    pub form: FilterForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum FilterForm {
    #[default]
    Legacy,
    Filter,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Expiration {
    pub days: Option<u32>,
    /// ISO 8601 midnight UTC, as given.
    pub date: Option<String>,
    pub expired_object_delete_marker: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub id: String,
    pub enabled: bool,
    pub filter: Filter,
    pub expiration: Option<Expiration>,
    pub noncurrent_days: Option<u32>,
    pub newer_noncurrent: Option<u32>,
    pub abort_mpu_days: Option<u32>,
    /// `<Transition>`/`<NoncurrentVersionTransition>` blocks, kept verbatim.
    pub transitions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub rules: Vec<Rule>,
}

/// A rejected configuration: the S3 error code and message.
#[derive(Debug, Clone, PartialEq)]
pub struct Invalid(pub &'static str, pub String);

/// `src` with every `<tag>...</tag>` block removed, so a rule-level `<Prefix>`
/// isn't confused with one inside `<Filter>`.
fn without_block(src: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = src.to_string();
    while let Some(s) = out.find(&open) {
        match out[s..].find(&close) {
            Some(e) => out.replace_range(s..s + e + close.len(), ""),
            None => break,
        }
    }
    // `<Filter/>` and `<Filter></Filter>` are an empty filter.
    out.replace(&format!("<{tag}/>"), "")
}

fn parse_u32(field: &str, v: &str) -> Result<u32, Invalid> {
    v.trim().parse().map_err(|_| Invalid("InvalidArgument", format!("{field} must be a whole number")))
}

fn parse_tags(block: &str) -> Vec<(String, String)> {
    extract_all_xml_tags(block, "Tag")
        .into_iter()
        .map(|t| (extract_xml_tag(&t, "Key").unwrap_or_default(), extract_xml_tag(&t, "Value").unwrap_or_default()))
        .collect()
}

/// Parses and validates a `LifecycleConfiguration` document.
pub fn parse(xml: &str) -> Result<Config, Invalid> {
    let blocks = extract_all_xml_tags(xml, "Rule");
    if blocks.is_empty() {
        return Err(Invalid("MalformedXML", "a lifecycle configuration needs at least one Rule".into()));
    }
    let mut rules = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        let id = extract_xml_tag(b, "ID").unwrap_or_else(|| format!("rule-{}", i + 1));
        if id.len() > 255 {
            return Err(Invalid("InvalidArgument", "a rule ID can be at most 255 characters".into()));
        }
        let enabled = match extract_xml_tag(b, "Status").as_deref().map(str::trim) {
            Some("Enabled") => true,
            Some("Disabled") => false,
            _ => return Err(Invalid("MalformedXML", "Status must be Enabled or Disabled".into())),
        };
        let has_filter = b.contains("<Filter>") || b.contains("<Filter/>");
        let filter = if has_filter {
            let f = extract_xml_tag(b, "Filter").unwrap_or_default();
            let and = extract_xml_tag(&f, "And");
            let scope = and.clone().unwrap_or_else(|| f.clone());
            Filter {
                prefix: extract_xml_tag(&scope, "Prefix").unwrap_or_default(),
                tags: parse_tags(&scope),
                size_gt: extract_xml_tag(&scope, "ObjectSizeGreaterThan").and_then(|v| v.trim().parse().ok()),
                size_lt: extract_xml_tag(&scope, "ObjectSizeLessThan").and_then(|v| v.trim().parse().ok()),
                form: FilterForm::Filter,
            }
        } else {
            let rest = without_block(b, "Filter");
            Filter { prefix: extract_xml_tag(&rest, "Prefix").unwrap_or_default(), form: FilterForm::Legacy, ..Default::default() }
        };
        let expiration = match extract_xml_tag(b, "Expiration") {
            None => None,
            Some(e) => {
                let days = extract_xml_tag(&e, "Days").map(|d| parse_u32("Days", &d)).transpose()?;
                if days == Some(0) {
                    return Err(Invalid("InvalidArgument", "Days in Expiration must be a positive integer".into()));
                }
                let date = extract_xml_tag(&e, "Date").map(|d| d.trim().to_string());
                if let Some(d) = &date {
                    let midnight = DateTime::parse_from_rfc3339(d).ok().map(|t| t.with_timezone(&Utc)).filter(|t| t.time() == chrono::NaiveTime::MIN);
                    if midnight.is_none() {
                        return Err(Invalid("InvalidArgument", "Date must be ISO 8601 at midnight UTC".into()));
                    }
                }
                let marker = extract_xml_tag(&e, "ExpiredObjectDeleteMarker").map(|v| v.trim() == "true");
                Some(Expiration { days, date, expired_object_delete_marker: marker })
            }
        };
        let (noncurrent_days, newer_noncurrent) = match extract_xml_tag(b, "NoncurrentVersionExpiration") {
            None => (None, None),
            Some(n) => (
                extract_xml_tag(&n, "NoncurrentDays").map(|d| parse_u32("NoncurrentDays", &d)).transpose()?,
                extract_xml_tag(&n, "NewerNoncurrentVersions").map(|d| parse_u32("NewerNoncurrentVersions", &d)).transpose()?,
            ),
        };
        let abort_mpu_days = extract_xml_tag(b, "AbortIncompleteMultipartUpload")
            .and_then(|a| extract_xml_tag(&a, "DaysAfterInitiation"))
            .map(|d| parse_u32("DaysAfterInitiation", &d))
            .transpose()?;
        let mut transitions = Vec::new();
        for t in ["Transition", "NoncurrentVersionTransition"] {
            transitions.extend(extract_all_xml_tags(b, t).into_iter().map(|x| format!("<{t}>{x}</{t}>")));
        }
        rules.push(Rule { id, enabled, filter, expiration, noncurrent_days, newer_noncurrent, abort_mpu_days, transitions });
    }
    let mut ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(Invalid("InvalidArgument", "rule IDs must be unique".into()));
    }
    Ok(Config { rules })
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The configuration as `GetBucketLifecycleConfiguration` returns it.
pub fn to_xml(c: &Config) -> String {
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<LifecycleConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for r in &c.rules {
        x.push_str("<Rule>");
        x.push_str(&format!("<ID>{}</ID>", esc(&r.id)));
        match r.filter.form {
            FilterForm::Legacy => x.push_str(&format!("<Prefix>{}</Prefix>", esc(&r.filter.prefix))),
            FilterForm::Filter => {
                let f = &r.filter;
                let parts = (!f.prefix.is_empty()) as usize + f.tags.len() + f.size_gt.is_some() as usize + f.size_lt.is_some() as usize;
                let mut inner = String::new();
                if !f.prefix.is_empty() {
                    inner.push_str(&format!("<Prefix>{}</Prefix>", esc(&f.prefix)));
                }
                for (k, v) in &f.tags {
                    inner.push_str(&format!("<Tag><Key>{}</Key><Value>{}</Value></Tag>", esc(k), esc(v)));
                }
                if let Some(n) = f.size_gt {
                    inner.push_str(&format!("<ObjectSizeGreaterThan>{n}</ObjectSizeGreaterThan>"));
                }
                if let Some(n) = f.size_lt {
                    inner.push_str(&format!("<ObjectSizeLessThan>{n}</ObjectSizeLessThan>"));
                }
                if parts > 1 {
                    x.push_str(&format!("<Filter><And>{inner}</And></Filter>"));
                } else {
                    x.push_str(&format!("<Filter>{inner}</Filter>"));
                }
            }
        }
        x.push_str(&format!("<Status>{}</Status>", if r.enabled { "Enabled" } else { "Disabled" }));
        if let Some(e) = &r.expiration {
            x.push_str("<Expiration>");
            if let Some(d) = e.days {
                x.push_str(&format!("<Days>{d}</Days>"));
            }
            if let Some(d) = &e.date {
                x.push_str(&format!("<Date>{}</Date>", esc(d)));
            }
            if let Some(m) = e.expired_object_delete_marker {
                x.push_str(&format!("<ExpiredObjectDeleteMarker>{m}</ExpiredObjectDeleteMarker>"));
            }
            x.push_str("</Expiration>");
        }
        if r.noncurrent_days.is_some() || r.newer_noncurrent.is_some() {
            x.push_str("<NoncurrentVersionExpiration>");
            if let Some(d) = r.noncurrent_days {
                x.push_str(&format!("<NoncurrentDays>{d}</NoncurrentDays>"));
            }
            if let Some(n) = r.newer_noncurrent {
                x.push_str(&format!("<NewerNoncurrentVersions>{n}</NewerNoncurrentVersions>"));
            }
            x.push_str("</NoncurrentVersionExpiration>");
        }
        if let Some(d) = r.abort_mpu_days {
            x.push_str(&format!("<AbortIncompleteMultipartUpload><DaysAfterInitiation>{d}</DaysAfterInitiation></AbortIncompleteMultipartUpload>"));
        }
        for t in &r.transitions {
            x.push_str(t);
        }
        x.push_str("</Rule>");
    }
    x.push_str("</LifecycleConfiguration>");
    x
}

impl Rule {
    /// Whether the rule's filter selects an object with this key, tags and
    /// size. A tag filter needs every tag it names on the object.
    pub fn matches(&self, key: &str, tags: &[(String, String)], size: u64) -> bool {
        let f = &self.filter;
        key.starts_with(&f.prefix)
            && f.tags.iter().all(|t| tags.contains(t))
            && f.size_gt.is_none_or(|n| size > n)
            && f.size_lt.is_none_or(|n| size < n)
    }
}

/// The length of a lifecycle "day". `WARPDRIVE_LIFECYCLE_DEBUG_DAY_SECS`
/// shortens it for tests only (the S3 compatibility suite runs with 10 s,
/// like Ceph's `rgw_lc_debug_interval`); in production it's 24 hours.
pub fn day() -> Duration {
    static SECS: std::sync::LazyLock<i64> = std::sync::LazyLock::new(|| {
        std::env::var("WARPDRIVE_LIFECYCLE_DEBUG_DAY_SECS").ok().and_then(|v| v.trim().parse().ok()).filter(|s: &i64| *s > 0).unwrap_or(86_400)
    });
    Duration::seconds(*SECS)
}

/// Whether lifecycle days are shortened for testing.
pub fn debug_days() -> bool {
    day() != Duration::days(1)
}

/// An S3 timestamp as the metadata stores write it.
pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .or_else(|| chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok().map(|t| Utc.from_utc_datetime(&t)))
        .or_else(|| DateTime::parse_from_rfc2822(s.trim()).map(|t| t.with_timezone(&Utc)).ok())
}

/// When an object created at `created` expires under `e`, if it does: the
/// date, or the creation time plus the days (S3 rounds that up to the next
/// midnight UTC; with debug days, no rounding).
pub fn expires_at(e: &Expiration, created: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some(d) = &e.date {
        return DateTime::parse_from_rfc3339(d).ok().map(|t| t.with_timezone(&Utc));
    }
    let at = created + day() * e.days? as i32;
    if debug_days() {
        return Some(at);
    }
    let midnight = at.date_naive().and_hms_opt(0, 0, 0).map(|t| Utc.from_utc_datetime(&t))?;
    Some(if midnight == at { at } else { midnight + Duration::days(1) })
}

/// The `x-amz-expiration` header for an object, from the first enabled rule
/// with an expiration that selects it: `expiry-date="...", rule-id="..."`.
/// The date uses real days, whatever the debug day length.
pub fn expiration_header(c: &Config, key: &str, tags: &[(String, String)], size: u64, created: DateTime<Utc>) -> Option<String> {
    for r in c.rules.iter().filter(|r| r.enabled && r.matches(key, tags, size)) {
        let Some(e) = &r.expiration else { continue };
        let at = if let Some(d) = &e.date {
            DateTime::parse_from_rfc3339(d).ok()?.with_timezone(&Utc)
        } else {
            let Some(days) = e.days else { continue };
            let t = created + Duration::days(days as i64);
            let midnight = Utc.from_utc_datetime(&NaiveDate::from(t.date_naive()).and_hms_opt(0, 0, 0)?);
            if midnight == t { t } else { midnight + Duration::days(1) }
        };
        return Some(format!("expiry-date=\"{}\", rule-id=\"{}\"", at.format("%a, %d %b %Y %H:%M:%S GMT"), r.id));
    }
    None
}

/// The `x-amz-expiration` header for an object in `bucket`, from the
/// bucket's stored rules, if any rule expires it. A bucket without rules
/// costs one bucket lookup; `tags` is called only when a rule filters on
/// tags, so the object's tags are read only then.
pub fn header_for(
    db: &crate::service::metadata_service::MetadataService,
    bucket: &str,
    key: &str,
    tags: impl FnOnce() -> Vec<(String, String)>,
    size: u64,
    created: DateTime<Utc>,
) -> Option<String> {
    let xml = db.get_bucket_lifecycle(bucket).ok()??;
    let config = parse(&xml).ok()?;
    let tags = if config.rules.iter().any(|r| !r.filter.tags.is_empty()) { tags() } else { Vec::new() };
    expiration_header(&config, key, &tags, size, created)
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<LifecycleConfiguration><Rule><ID>rule1</ID><Prefix>test1/</Prefix><Status>Enabled</Status><Expiration><Days>1</Days></Expiration></Rule><Rule><ID>t</ID><Filter><And><Prefix>p/</Prefix><Tag><Key>k</Key><Value>v</Value></Tag></And></Filter><Status>Disabled</Status><NoncurrentVersionExpiration><NoncurrentDays>2</NoncurrentDays></NoncurrentVersionExpiration><AbortIncompleteMultipartUpload><DaysAfterInitiation>3</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#;

    #[test]
    fn parses_writes_back_and_parses_again_to_the_same_rules() {
        let c = parse(XML).unwrap();
        assert_eq!(c.rules.len(), 2);
        assert_eq!(c.rules[0].filter, Filter { prefix: "test1/".into(), form: FilterForm::Legacy, ..Default::default() });
        assert_eq!(c.rules[1].filter.tags, vec![("k".to_string(), "v".to_string())]);
        assert_eq!((c.rules[1].noncurrent_days, c.rules[1].abort_mpu_days, c.rules[1].enabled), (Some(2), Some(3), false));
        assert_eq!(parse(&to_xml(&c)).unwrap(), c);
    }

    #[test]
    fn rejects_what_s3_rejects() {
        let rule = |body: &str| format!("<LifecycleConfiguration><Rule>{body}</Rule></LifecycleConfiguration>");
        assert_eq!(parse(&rule("<ID>a</ID><Prefix/><Status>Enabled</Status><Expiration><Days>0</Days></Expiration>")).unwrap_err().0, "InvalidArgument");
        assert_eq!(parse(&rule("<ID>a</ID><Prefix/><Status>enabled</Status>")).unwrap_err().0, "MalformedXML");
        assert_eq!(parse(&rule(&format!("<ID>{}</ID><Status>Enabled</Status>", "x".repeat(256)))).unwrap_err().0, "InvalidArgument");
        assert_eq!(parse(&rule("<ID>a</ID><Status>Enabled</Status><Expiration><Date>2027-01-01T12:00:00Z</Date></Expiration>")).unwrap_err().0, "InvalidArgument");
        let two = "<LifecycleConfiguration><Rule><ID>a</ID><Status>Enabled</Status></Rule><Rule><ID>a</ID><Status>Enabled</Status></Rule></LifecycleConfiguration>";
        assert_eq!(parse(two).unwrap_err().0, "InvalidArgument");
    }

    #[test]
    fn a_rule_without_an_id_gets_one() {
        let c = parse("<LifecycleConfiguration><Rule><Prefix>a/</Prefix><Status>Enabled</Status><Expiration><Days>3</Days></Expiration></Rule></LifecycleConfiguration>").unwrap();
        assert!(!c.rules[0].id.is_empty());
    }

    #[test]
    fn matching_needs_the_prefix_every_tag_and_the_size_bounds() {
        let mut r = parse(XML).unwrap().rules[1].clone();
        assert!(r.matches("p/x", &[("k".into(), "v".into()), ("o".into(), "1".into())], 10));
        assert!(!r.matches("p/x", &[], 10));
        assert!(!r.matches("q/x", &[("k".into(), "v".into())], 10));
        r.filter.size_gt = Some(100);
        assert!(!r.matches("p/x", &[("k".into(), "v".into())], 10));
    }

    #[test]
    fn the_header_rounds_up_to_the_next_midnight() {
        let c = parse(XML).unwrap();
        let created = Utc.with_ymd_and_hms(2026, 11, 23, 7, 30, 0).unwrap();
        let h = expiration_header(&c, "test1/a", &[], 1, created).unwrap();
        assert_eq!(h, "expiry-date=\"Wed, 25 Nov 2026 00:00:00 GMT\", rule-id=\"rule1\"");
        assert!(expiration_header(&c, "other", &[], 1, created).is_none());
    }
}
