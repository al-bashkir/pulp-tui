//! Content View filter rules, evaluated client-side and deterministically on content
//! listed from Pulp. Syntax (one rule; several rules are separated by `;`):
//!
//! ```text
//! include name=kernel* arch=x86_64 evr>=5.14-1
//! exclude name=*-debuginfo
//! include name=nginx* arch=amd64 version>=1.22    (Deb repositories: version, not evr)
//! exclude path=isos/*.iso            (File repositories)
//! ```
//!
//! A unit is selected when (there are no include rules OR an include rule matches) AND no
//! exclude rule matches. All criteria of one rule must match.

use crate::pulp::RepoType;
use crate::pulp::models::{DebPackage, FileContent, Package};
use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Include,
    Exclude,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterRule {
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// Comparison operator and EVR, e.g. (">=", "1:2.0-3").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evr: Option<(String, String)>,
    /// Comparison operator and Debian version, e.g. (">=", "1:2.0-3").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl std::fmt::Display for FilterRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", if self.action == Action::Include { "include" } else { "exclude" })?;
        if let Some(n) = &self.name {
            write!(f, " name={n}")?;
        }
        if let Some(a) = &self.arch {
            write!(f, " arch={a}")?;
        }
        if let Some((op, v)) = &self.evr {
            write!(f, " evr{op}{v}")?;
        }
        if let Some((op, v)) = &self.version {
            write!(f, " version{op}{v}")?;
        }
        if let Some(p) = &self.path {
            write!(f, " path={p}")?;
        }
        Ok(())
    }
}

pub fn format_rules(rules: &[FilterRule]) -> String {
    rules.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
}

/// Form help text with example rules for a repository type.
pub fn filter_help(t: RepoType) -> &'static str {
    match t {
        RepoType::Rpm => "e.g. include name=kernel* arch=x86_64; exclude name=*-debuginfo (empty = all)",
        RepoType::File => "e.g. include path=docs/* (empty = all)",
        RepoType::Deb => "e.g. include name=nginx* arch=amd64 version>=1.22; exclude name=*-dbg (empty = all)",
    }
}

pub fn parse_rules(s: &str, t: RepoType) -> Result<Vec<FilterRule>, String> {
    s.split(';').map(str::trim).filter(|r| !r.is_empty()).map(|r| parse_rule(r, t)).collect()
}

pub fn parse_rule(s: &str, t: RepoType) -> Result<FilterRule, String> {
    let mut words = s.split_whitespace();
    let action = match words.next() {
        Some("include") => Action::Include,
        Some("exclude") => Action::Exclude,
        other => return Err(format!("rule must start with include/exclude, got {other:?}")),
    };
    let mut r = FilterRule { action, name: None, arch: None, evr: None, version: None, path: None };
    for w in words {
        if let Some(v) = w.strip_prefix("name=") {
            r.name = Some(v.into());
        } else if let Some(v) = w.strip_prefix("arch=") {
            r.arch = Some(v.into());
        } else if let Some(v) = w.strip_prefix("path=") {
            r.path = Some(v.into());
        } else if let Some(rest) = w.strip_prefix("evr") {
            r.evr = Some(parse_cmp(w, rest, "evr")?);
        } else if let Some(rest) = w.strip_prefix("version") {
            r.version = Some(parse_cmp(w, rest, "version")?);
        } else {
            return Err(format!("unknown criterion {w:?} (use name=, arch=, evr<op>, version<op>, path=)"));
        }
    }
    let rpm_crit = r.name.is_some() || r.arch.is_some() || r.evr.is_some();
    // Each type refuses the criteria it lacks and names the types that have them.
    let (path_only, evr_only, version_only) = (
        "path= applies to File repositories only",
        "evr applies to RPM repositories only (use version for Deb)",
        "version applies to Deb repositories only",
    );
    match t {
        RepoType::Rpm => {
            if r.path.is_some() {
                return Err(path_only.into());
            }
            if r.version.is_some() {
                return Err(version_only.into());
            }
        }
        RepoType::File => {
            if r.name.is_some() || r.arch.is_some() {
                return Err("name=/arch= apply to RPM and Deb repositories only".into());
            }
            if r.evr.is_some() {
                return Err(evr_only.into());
            }
            if r.version.is_some() {
                return Err(version_only.into());
            }
        }
        RepoType::Deb => {
            if r.path.is_some() {
                return Err(path_only.into());
            }
            if r.evr.is_some() {
                return Err(evr_only.into());
            }
        }
    }
    if !rpm_crit && r.version.is_none() && r.path.is_none() {
        return Err(format!("rule {s:?} has no criteria"));
    }
    for g in [&r.name, &r.arch, &r.path].into_iter().flatten() {
        Glob::new(g).map_err(|e| format!("bad pattern {g}: {e}"))?;
    }
    Ok(r)
}

/// Split the `<op><value>` of an `evr` or `version` criterion; `w` is the whole word.
fn parse_cmp(w: &str, rest: &str, what: &str) -> Result<(String, String), String> {
    let op =
        ["<=", ">=", "<", ">", "="].into_iter().find(|op| rest.starts_with(op)).ok_or_else(|| format!("bad {what} operator in {w}"))?;
    let v = &rest[op.len()..];
    if v.is_empty() {
        return Err(format!("empty {what} in {w}"));
    }
    // `evr=>1` would compare with ">1"; a non-numeric epoch would silently become 0.
    if !v.starts_with(|c: char| c.is_ascii_alphanumeric()) || v.split_once(':').is_some_and(|(e, _)| e.parse::<u64>().is_err()) {
        return Err(format!("bad {what} value in {w}"));
    }
    Ok((op.into(), v.into()))
}

/// A rule with its name, arch and path globs compiled.
type Compiled = (FilterRule, Option<GlobMatcher>, Option<GlobMatcher>, Option<GlobMatcher>);

/// Compiled rules for fast evaluation over large content sets.
pub struct Selector {
    rules: Vec<Compiled>,
    has_include: bool,
}

fn glob(p: &Option<String>) -> Option<GlobMatcher> {
    p.as_ref().and_then(|g| Glob::new(g).ok()).map(|g| g.compile_matcher())
}

impl Selector {
    pub fn new(rules: &[FilterRule]) -> Self {
        Self {
            has_include: rules.iter().any(|r| r.action == Action::Include),
            rules: rules.iter().map(|r| (r.clone(), glob(&r.name), glob(&r.arch), glob(&r.path))).collect(),
        }
    }

    fn select(&self, matches: impl Fn(&FilterRule, &Option<GlobMatcher>, &Option<GlobMatcher>, &Option<GlobMatcher>) -> bool) -> bool {
        let m = |a: Action| self.rules.iter().filter(|r| r.0.action == a).any(|(r, n, ar, p)| matches(r, n, ar, p));
        (!self.has_include || m(Action::Include)) && !m(Action::Exclude)
    }

    pub fn package(&self, p: &Package) -> bool {
        self.select(|r, name, arch, _| {
            name.as_ref().is_none_or(|g| g.is_match(&p.name))
                && arch.as_ref().is_none_or(|g| g.is_match(&p.arch))
                && r.evr.as_ref().is_none_or(|(op, v)| evr_matches(p, op, v))
        })
    }

    pub fn deb_package(&self, p: &DebPackage) -> bool {
        self.select(|r, name, arch, _| {
            name.as_ref().is_none_or(|g| g.is_match(&p.package))
                && arch.as_ref().is_none_or(|g| g.is_match(&p.architecture))
                && r.version.as_ref().is_none_or(|(op, v)| deb_version_matches(&p.version, op, v))
        })
    }

    pub fn file(&self, f: &FileContent) -> bool {
        self.select(|_, _, _, path| path.as_ref().is_none_or(|g| g.is_match(&f.relative_path)))
    }
}

/// Split `[epoch:]version[-release]`.
pub fn split_evr(s: &str) -> (u64, &str, Option<&str>) {
    let (epoch, rest) = match s.split_once(':') {
        Some((e, r)) => (e.parse().unwrap_or(0), r),
        None => (0, s),
    };
    match rest.rsplit_once('-') {
        Some((v, r)) => (epoch, v, Some(r)),
        None => (epoch, rest, None),
    }
}

fn evr_matches(p: &Package, op: &str, want: &str) -> bool {
    let (we, wv, wr) = split_evr(want);
    let pe: u64 = p.epoch.parse().unwrap_or(0);
    let mut ord = pe.cmp(&we).then_with(|| rpmvercmp(&p.version, wv));
    if let Some(wr) = wr {
        ord = ord.then_with(|| rpmvercmp(&p.release, wr));
    }
    op_holds(ord, op)
}

fn op_holds(ord: Ordering, op: &str) -> bool {
    match op {
        "<" => ord == Ordering::Less,
        "<=" => ord != Ordering::Greater,
        ">" => ord == Ordering::Greater,
        ">=" => ord != Ordering::Less,
        _ => ord == Ordering::Equal,
    }
}

/// Compare `have` with `want` (`[epoch:]upstream[-revision]`). Like `evr`, a rule without a
/// revision matches every revision of the upstream version.
pub fn deb_version_matches(have: &str, op: &str, want: &str) -> bool {
    let (he, hv, hr) = split_evr(have);
    let (we, wv, wr) = split_evr(want);
    let mut ord = he.cmp(&we).then_with(|| dpkg_verrevcmp(hv, wv));
    if let Some(wr) = wr {
        ord = ord.then_with(|| dpkg_verrevcmp(hr.unwrap_or(""), wr));
    }
    op_holds(ord, op)
}

/// Port of dpkg's `verrevcmp` (lib/dpkg/version.c) for an upstream version or a revision.
pub fn dpkg_verrevcmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0); // 0 = end of string
    let order = |c: u8| match c {
        b'~' => -1,
        c if c == 0 || c.is_ascii_digit() => 0,
        c if c.is_ascii_alphabetic() => c as i32,
        c => c as i32 + 256,
    };
    let non_digit = |c: u8| c != 0 && !c.is_ascii_digit();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        while non_digit(at(a, i)) || non_digit(at(b, j)) {
            let (ac, bc) = (order(at(a, i)), order(at(b, j)));
            if ac != bc {
                return ac.cmp(&bc);
            }
            i += 1;
            j += 1;
        }
        while at(a, i) == b'0' {
            i += 1;
        }
        while at(b, j) == b'0' {
            j += 1;
        }
        let mut first_diff = 0;
        while at(a, i).is_ascii_digit() && at(b, j).is_ascii_digit() {
            if first_diff == 0 {
                first_diff = at(a, i) as i32 - at(b, j) as i32;
            }
            i += 1;
            j += 1;
        }
        // The longer digit run is the bigger number.
        if at(a, i).is_ascii_digit() {
            return Ordering::Greater;
        }
        if at(b, j).is_ascii_digit() {
            return Ordering::Less;
        }
        if first_diff != 0 {
            return first_diff.cmp(&0);
        }
    }
    Ordering::Equal
}

/// Port of rpm's `rpmvercmp` (including `~` pre-release and `^` post-release handling).
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    let sep = |c: u8| !c.is_ascii_alphanumeric() && c != b'~' && c != b'^';
    loop {
        while let Some((&c, rest)) = a.split_first() {
            if sep(c) { a = rest } else { break }
        }
        while let Some((&c, rest)) = b.split_first() {
            if sep(c) { b = rest } else { break }
        }
        // Tilde sorts before everything, even the end of the string.
        match (a.first() == Some(&b'~'), b.first() == Some(&b'~')) {
            (true, true) => {
                a = &a[1..];
                b = &b[1..];
                continue;
            }
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        // Caret sorts after the end of the string but before anything else.
        match (a.first() == Some(&b'^'), b.first() == Some(&b'^')) {
            (true, true) => {
                a = &a[1..];
                b = &b[1..];
                continue;
            }
            (true, false) => return if b.is_empty() { Ordering::Greater } else { Ordering::Less },
            (false, true) => return if a.is_empty() { Ordering::Less } else { Ordering::Greater },
            _ => {}
        }
        if a.is_empty() || b.is_empty() {
            break;
        }
        let numeric = a[0].is_ascii_digit();
        let take = |s: &[u8]| s.iter().take_while(|c| if numeric { c.is_ascii_digit() } else { c.is_ascii_alphabetic() }).count();
        let (la, lb) = (take(a), take(b));
        let (sa, sb) = (&a[..la], &b[..lb]);
        if lb == 0 {
            // Numeric segments are newer than alphabetic ones.
            return if numeric { Ordering::Greater } else { Ordering::Less };
        }
        let ord = if numeric {
            let trim = |s: &[u8]| -> usize { s.iter().take_while(|&&c| c == b'0').count() };
            let (ta, tb) = (&sa[trim(sa)..], &sb[trim(sb)..]);
            ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb))
        } else {
            sa.cmp(sb)
        };
        if ord != Ordering::Equal {
            return ord;
        }
        a = &a[la..];
        b = &b[lb..];
    }
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        _ => Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::*;

    fn pkg(name: &str, evr: &str, arch: &str) -> Package {
        let (e, v, r) = split_evr(evr);
        Package {
            pulp_href: format!("/{name}/"),
            name: name.into(),
            epoch: e.to_string(),
            version: v.into(),
            release: r.unwrap_or("1").into(),
            arch: arch.into(),
            location_href: String::new(),
            sha256: None,
        }
    }

    #[test]
    fn rpmvercmp_matches_rpm_test_suite() {
        // Cases from rpm's tests/rpmvercmp.at
        let cases = [
            ("1.0", "1.0", Equal),
            ("1.0", "2.0", Less),
            ("2.0.1", "2.0", Greater),
            ("5.5p1", "5.5p2", Less),
            ("5.5p10", "5.5p1", Greater),
            ("10xyz", "10.1xyz", Less),
            ("xyz10", "xyz10.1", Less),
            ("1.0aa", "1.0a", Greater),
            ("10", "9", Greater),
            ("1.010", "1.9", Greater),
            ("1.0a", "1.0", Greater),
            ("2a", "2.0", Less),
            ("6.0.rc1", "6.0", Greater),
            ("1.0~rc1", "1.0", Less),
            ("1.0~rc1", "1.0~rc2", Less),
            ("1.0~rc1~git123", "1.0~rc1", Less),
            ("1.0^", "1.0", Greater),
            ("1.0^git1", "1.0^git2", Less),
            ("1.0^git1", "1.01", Less),
            ("1.0~rc1^git1", "1.0~rc1", Greater),
            ("1.0^git1~pre", "1.0^git1", Less),
            ("b", "a", Greater),
            ("1_0", "1.0", Equal),
        ];
        for (a, b, want) in cases {
            assert_eq!(rpmvercmp(a, b), want, "{a} vs {b}");
            assert_eq!(rpmvercmp(b, a), want.reverse(), "{b} vs {a}");
        }
    }

    #[test]
    fn rules_parse_and_roundtrip() {
        let rs = parse_rules("include name=kernel* arch=x86_64 evr>=5.14-1; exclude name=*-debuginfo", RepoType::Rpm).unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].evr, Some((">=".into(), "5.14-1".into())));
        assert_eq!(parse_rules(&format_rules(&rs), RepoType::Rpm).unwrap(), rs);
        assert!(parse_rule("include path=*.iso", RepoType::Rpm).is_err());
        assert!(parse_rule("exclude name=x", RepoType::File).is_err());
        assert!(parse_rule("drop name=x", RepoType::Rpm).is_err());
        assert!(parse_rule("include", RepoType::Rpm).is_err());
        assert!(parse_rule("include evr~1", RepoType::Rpm).is_err());
        assert_eq!(parse_rule("include name=kernel evr=>5.14", RepoType::Rpm).unwrap_err(), "bad evr value in evr=>5.14");
        assert!(parse_rule("include evr>=x:1.0", RepoType::Rpm).is_err(), "epoch must be a number");
        assert!(parse_rule("include evr>=1:1.0-1", RepoType::Rpm).is_ok());
        assert!(parse_rules("", RepoType::File).unwrap().is_empty());
    }

    #[test]
    fn each_type_refuses_the_criteria_of_the_others_and_names_the_types_that_have_them() {
        let (path, evr, version) = (
            "path= applies to File repositories only",
            "evr applies to RPM repositories only (use version for Deb)",
            "version applies to Deb repositories only",
        );
        let names = "name=/arch= apply to RPM and Deb repositories only";
        let err = |rule: &str, t| parse_rule(rule, t).unwrap_err();
        let r = parse_rule("include name=nginx* arch=amd64 version>=1.22", RepoType::Deb).unwrap();
        assert_eq!(r.version, Some((">=".into(), "1.22".into())));
        assert_eq!(err("include path=x", RepoType::Deb), path);
        assert_eq!(err("include evr>=1.0", RepoType::Deb), evr);
        assert_eq!(err("include path=x", RepoType::Rpm), path);
        assert_eq!(err("include version>=1.0", RepoType::Rpm), version);
        assert_eq!(err("include name=x", RepoType::File), names);
        assert_eq!(err("include arch=x", RepoType::File), names);
        assert_eq!(err("include evr>=1.0", RepoType::File), evr);
        assert_eq!(err("include version>=1.0", RepoType::File), version);
        assert!(parse_rule("exclude path=isos/*", RepoType::File).is_ok());
    }

    #[test]
    fn selector_semantics() {
        let rs = parse_rules("include name=bash*; include name=kernel evr<5.14; exclude arch=i686", RepoType::Rpm).unwrap();
        let s = Selector::new(&rs);
        assert!(s.package(&pkg("bash", "5.2-1", "x86_64")));
        assert!(s.package(&pkg("bash-completion", "2.11-1", "noarch")));
        assert!(!s.package(&pkg("bash", "5.2-1", "i686")));
        assert!(s.package(&pkg("kernel", "5.10-1", "x86_64")));
        assert!(!s.package(&pkg("kernel", "5.14-1", "x86_64")));
        assert!(!s.package(&pkg("kernel", "1:4.0-1", "x86_64"))); // epoch wins
        assert!(!s.package(&pkg("zsh", "5.9-1", "x86_64")));
        // Exclude-only: everything else stays.
        let s = Selector::new(&parse_rules("exclude name=*-debuginfo", RepoType::Rpm).unwrap());
        assert!(s.package(&pkg("zsh", "5.9-1", "x86_64")));
        assert!(!s.package(&pkg("zsh-debuginfo", "5.9-1", "x86_64")));
        let s = Selector::new(&parse_rules("exclude path=isos/*", RepoType::File).unwrap());
        let f = |p: &str| FileContent { pulp_href: "/f/".into(), relative_path: p.into(), sha256: None };
        assert!(s.file(&f("docs/readme.txt")));
        assert!(!s.file(&f("isos/a.iso")));
        assert!(Selector::new(&[]).file(&f("x")));
    }

    #[test]
    fn dpkg_verrevcmp_matches_dpkg_rules() {
        let cases = [
            ("1.0", "1.0", Equal),
            ("1.0", "1.1", Less),
            ("1.0~rc1", "1.0", Less),
            ("1.0~~", "1.0~", Less),
            ("1.0~", "1.0", Less),
            ("1.0", "1.0+b1", Less),
            ("1.0a", "1.0", Greater),
            ("1.0a", "1.0+", Less),
            ("2.0", "10.0", Less),
            ("1.001", "1.1", Equal),
            ("1.2.3", "1.2", Greater),
            ("1.0", "1.0.0", Less),
            ("a", "b", Less),
        ];
        for (a, b, want) in cases {
            assert_eq!(dpkg_verrevcmp(a, b), want, "{a} vs {b}");
            assert_eq!(dpkg_verrevcmp(b, a), want.reverse(), "{b} vs {a}");
        }
    }

    #[test]
    fn deb_version_rule_semantics() {
        assert!(deb_version_matches("1.22-1", "=", "1.22")); // no revision in the rule
        assert!(deb_version_matches("1.22-1", "<=", "1.22"));
        assert!(deb_version_matches("1.22-1", "<", "1.22-2"));
        assert!(deb_version_matches("1.22-1", ">", "1.22-1~bpo1"));
        assert!(deb_version_matches("1:1.0-1", ">", "2.0")); // epoch wins
        assert!(deb_version_matches("1.22", "<", "1.22-1")); // missing revision = ""
        assert!(deb_version_matches("1.2-3-4", "=", "1.2-3-4")); // revision after the last '-'
        assert!(!deb_version_matches("1.21-9", ">=", "1.22"));
    }

    #[test]
    fn version_criterion_parses_roundtrips_and_matches_deb_packages() {
        assert!(parse_rule("include version>=1.0", RepoType::Rpm).is_err(), "version needs a Deb repository");
        assert!(parse_rule("include version>=1.0", RepoType::File).is_err());
        assert!(parse_rule("include version>=1.0", RepoType::Deb).is_ok());
        let r = FilterRule {
            action: Action::Include,
            name: Some("fixture-*".into()),
            arch: None,
            evr: None,
            version: Some((">=".into(), "1.0-2".into())),
            path: None,
        };
        assert_eq!(format_rules(std::slice::from_ref(&r)), "include name=fixture-* version>=1.0-2");
        assert_eq!(serde_json::from_str::<FilterRule>(&serde_json::to_string(&r).unwrap()).unwrap(), r);
        // Rules saved before `version` existed still load.
        let old = serde_json::from_str::<FilterRule>(r#"{"action":"exclude","name":"x"}"#).unwrap();
        assert_eq!(old.version, None);

        let deb = |name: &str, v: &str| DebPackage {
            pulp_href: format!("/{name}/"),
            package: name.into(),
            version: v.into(),
            architecture: "amd64".into(),
            relative_path: String::new(),
            sha256: None,
        };
        let s = Selector::new(&parse_rules("exclude name=*-dbg", RepoType::Deb).unwrap());
        assert!(s.deb_package(&deb("fixture-beta", "1.0-1")));
        assert!(!s.deb_package(&deb("fixture-beta-dbg", "1.0-1")));
        let s = Selector::new(&[r]);
        assert!(s.deb_package(&deb("fixture-beta", "1.0-2")));
        assert!(s.deb_package(&deb("fixture-beta", "1:0.1-1")));
        assert!(!s.deb_package(&deb("fixture-beta", "1.0-1")));
        assert!(!s.deb_package(&deb("other", "2.0-1")));
    }

    #[test]
    fn evr_with_release() {
        let p = pkg("x", "1.2-3.el9", "noarch");
        assert!(evr_matches(&p, "=", "1.2-3.el9"));
        assert!(evr_matches(&p, "=", "1.2"));
        assert!(evr_matches(&p, "<", "1.2-4"));
        assert!(evr_matches(&p, ">", "0:1.1"));
    }
}
