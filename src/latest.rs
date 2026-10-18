//! "Latest" model aliases: `<family>-latest` and `<family>-<tier>-latest`.
//!
//! An alias is resolved by the proxy on every request, so a subagent card that
//! names `gpt-sol-latest` never has to change when a newer `gpt-sol` appears.
//!
//! The algorithm is pure and deterministic:
//!
//! 1. A model id is parsed as `<family>-<version>[-<tier>]`, where the family is
//!    one of [`FAMILIES`] (only `gpt`, the Codex models), the version is
//!    dot-separated integers and the tier is one or more lowercase letter words.
//!    Ids that do not fit (`claude-opus-5`, `gpt-4-0613`, ids with digits in the
//!    tier) do not take part.
//! 2. Ids with a `fast` word are priority-tier copies of another model and never
//!    take part. A tier containing a word of [`VARIANT_WORDS`] (`mini`, `codex`,
//!    `spark`, ...) marks a variant: it is left out of `<family>-latest` and is
//!    reachable only through its explicit tier alias (`gpt-mini-latest`).
//! 3. Versions compare as tuples of integers, trailing zeros ignored, so
//!    `6.1 > 6 > 5.6` and `10 > 9`.
//! 4. `<family>-latest` is the highest version among the non-variant ids of the
//!    family; `<family>-<tier>-latest` the highest among the ids with exactly
//!    that tier. On equal versions the alphabetically first id wins.
//!
//! `CCP_LATEST_ALIASES=0` switches the aliases off.

use std::collections::BTreeSet;

/// `0`/`false`/`off`/`no` disables the aliases.
pub const LATEST_ENV: &str = "CCP_LATEST_ALIASES";
/// Families that get aliases: the Codex `gpt` models only.
pub const FAMILIES: [&str; 1] = ["gpt"];
/// Tier words that make a model a variant, excluded from `<family>-latest`.
pub const VARIANT_WORDS: [&str; 14] = [
    "mini",
    "nano",
    "codex",
    "spark",
    "image",
    "realtime",
    "audio",
    "transcribe",
    "tts",
    "search",
    "preview",
    "lite",
    "oss",
    "embedding",
];
const SUFFIX: &str = "-latest";
const MAX_VERSION_PARTS: usize = 6;

pub fn enabled() -> bool {
    !matches!(
        std::env::var(LATEST_ENV).ok().as_deref().map(str::trim),
        Some("0" | "false" | "off" | "no")
    )
}

/// A model id split into the parts the algorithm compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedId {
    pub family: &'static str,
    /// Integers with trailing zeros removed (`6.0` is `[6]`).
    pub version: Vec<u64>,
    /// Lowercase words joined by `-`, `None` when the id has none.
    pub tier: Option<String>,
    /// True when the tier contains a word of [`VARIANT_WORDS`].
    pub variant: bool,
}

fn is_word(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_lowercase())
}

fn parse_version(token: &str) -> Option<Vec<u64>> {
    let mut parts = Vec::new();
    for part in token.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parts.push(part.parse::<u64>().ok()?);
    }
    if parts.len() > MAX_VERSION_PARTS {
        return None;
    }
    while parts.last() == Some(&0) {
        parts.pop();
    }
    Some(parts)
}

/// Parse `id`, or `None` when it is not eligible (see the module docs).
pub fn parse_id(id: &str) -> Option<ParsedId> {
    let (family, rest) = id.split_once('-')?;
    let family = FAMILIES.iter().copied().find(|f| *f == family)?;
    let mut segments = rest.split('-');
    let version = parse_version(segments.next()?)?;
    let words: Vec<&str> = segments.collect();
    if !words.iter().all(|w| is_word(w)) || words.iter().any(|w| matches!(*w, "fast" | "latest")) {
        return None;
    }
    let variant = words.iter().any(|w| VARIANT_WORDS.contains(w));
    Some(ParsedId {
        family,
        version,
        tier: (!words.is_empty()).then(|| words.join("-")),
        variant,
    })
}

/// A resolved alias: `alias` currently points at `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestAlias {
    pub alias: String,
    pub family: String,
    pub tier: Option<String>,
    pub target: String,
    /// Provider that serves `target`.
    pub provider: String,
}

impl LatestAlias {
    /// Used by the card description and the model listing.
    pub fn scope(&self) -> String {
        match &self.tier {
            Some(tier) => format!("newest {tier} model of the {} family", self.family),
            None => format!("newest model of the {} family", self.family),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Not shaped like an alias, or aliases are off: route the id as it is.
    NotAlias,
    Resolved(LatestAlias),
    /// Shaped like an alias but no model matches.
    NoCandidates {
        alias: String,
    },
}

struct Entry<'a> {
    id: &'a str,
    provider: &'a str,
    parsed: ParsedId,
}

fn entries<'a>(models: &'a [(String, String)]) -> Vec<Entry<'a>> {
    models
        .iter()
        .filter_map(|(id, provider)| {
            Some(Entry {
                id,
                provider,
                parsed: parse_id(id)?,
            })
        })
        .collect()
}

/// Highest version; on equal versions the alphabetically first id.
fn best<'a>(candidates: impl Iterator<Item = &'a Entry<'a>>) -> Option<&'a Entry<'a>> {
    candidates.reduce(|a, b| {
        let better = b.parsed.version > a.parsed.version
            || (b.parsed.version == a.parsed.version && b.id < a.id);
        if better { b } else { a }
    })
}

fn alias_from(entry: &Entry<'_>, family: &str, tier: Option<&str>) -> LatestAlias {
    let alias = match tier {
        Some(tier) => format!("{family}-{tier}{SUFFIX}"),
        None => format!("{family}{SUFFIX}"),
    };
    LatestAlias {
        alias,
        family: family.to_string(),
        tier: tier.map(str::to_string),
        target: entry.id.to_string(),
        provider: entry.provider.to_string(),
    }
}

/// Every alias that has at least one candidate, sorted by alias. `models` are
/// `(id, provider)` pairs; an alias equal to a real id is dropped.
pub fn compute(models: &[(String, String)]) -> Vec<LatestAlias> {
    let entries = entries(models);
    let real: BTreeSet<&str> = models.iter().map(|(id, _)| id.as_str()).collect();
    let mut out = Vec::new();
    for family in FAMILIES {
        let of_family = || entries.iter().filter(move |e| e.parsed.family == family);
        if let Some(entry) = best(of_family().filter(|e| !e.parsed.variant)) {
            out.push(alias_from(entry, family, None));
        }
        let tiers: BTreeSet<&str> = of_family()
            .filter_map(|e| e.parsed.tier.as_deref())
            .collect();
        for tier in tiers {
            if let Some(entry) =
                best(of_family().filter(|e| e.parsed.tier.as_deref() == Some(tier)))
            {
                out.push(alias_from(entry, family, Some(tier)));
            }
        }
    }
    out.retain(|a| !real.contains(a.alias.as_str()));
    out.sort_by(|a, b| a.alias.cmp(&b.alias));
    out
}

/// Split `gpt-sol-latest` into `("gpt", Some("sol"))`; `gpt-latest` into
/// `("gpt", None)`. `None` when the text is not shaped like an alias.
pub fn parse_alias(name: &str) -> Option<(&'static str, Option<String>)> {
    let base = name.strip_suffix(SUFFIX)?;
    let (family, tier) = match base.split_once('-') {
        Some((family, tier)) => (family, Some(tier)),
        None => (base, None),
    };
    let family = FAMILIES.iter().copied().find(|f| *f == family)?;
    match tier {
        Some(tier) if !tier.split('-').all(is_word) => None,
        Some(tier) => Some((family, Some(tier.to_string()))),
        None => Some((family, None)),
    }
}

/// Resolve one requested model id against the available models.
pub fn resolve(models: &[(String, String)], requested: &str) -> Resolution {
    if !enabled() {
        return Resolution::NotAlias;
    }
    let Some((family, tier)) = parse_alias(requested) else {
        return Resolution::NotAlias;
    };
    // A real model with this exact id wins over the alias shape.
    if models.iter().any(|(id, _)| id == requested) {
        return Resolution::NotAlias;
    }
    let entries = entries(models);
    let of_family = entries.iter().filter(|e| e.parsed.family == family);
    let found = match tier.as_deref() {
        Some(tier) => best(of_family.filter(|e| e.parsed.tier.as_deref() == Some(tier))),
        None => best(of_family.filter(|e| !e.parsed.variant)),
    };
    match found {
        Some(entry) => Resolution::Resolved(alias_from(entry, family, tier.as_deref())),
        None => Resolution::NoCandidates {
            alias: requested.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models(ids: &[&str]) -> Vec<(String, String)> {
        ids.iter()
            .map(|id| ((*id).to_string(), "codex".to_string()))
            .collect()
    }

    fn target(set: &[&str], alias: &str) -> Option<String> {
        match resolve(&models(set), alias) {
            Resolution::Resolved(a) => Some(a.target),
            _ => None,
        }
    }

    const REAL: &[&str] = &[
        "gpt-5.2",
        "gpt-5.2-fast",
        "gpt-5.3-codex",
        "gpt-5.3-codex-fast",
        "gpt-5.3-codex-spark",
        "gpt-5.3-codex-spark-fast",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.4-mini-fast",
        "gpt-5.5",
        "gpt-5.6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-6-astra",
        "gpt-6-luna",
        "gpt-6-sol",
        "gpt-6.1-sol",
        "gpt-6.1-sol-fast",
        "claude-opus-5",
        "composer-2.5",
        "cursor:gpt-5.5",
    ];

    #[test]
    fn parses_family_version_and_tier() {
        let p = parse_id("gpt-6.1-sol").unwrap();
        assert_eq!((p.family, p.version.clone()), ("gpt", vec![6, 1]));
        assert_eq!(p.tier.as_deref(), Some("sol"));
        assert!(!p.variant);
        let p = parse_id("gpt-5.3-codex-spark").unwrap();
        assert_eq!(p.tier.as_deref(), Some("codex-spark"));
        assert!(p.variant);
        assert_eq!(parse_id("gpt-5.5").unwrap().tier, None);
        // Trailing zeros do not change a version.
        assert_eq!(parse_id("gpt-6.0-sol"), parse_id("gpt-6-sol"));
    }

    #[test]
    fn unparseable_and_fast_ids_do_not_take_part() {
        for id in [
            "gpt-6-sol-fast",
            "gpt-5.3-codex-spark-fast",
            "grok-4.7",
            "kimi-k3",
            "k3",
            "composer-2.5",
            "cursor:gpt-5.5",
            "claude-opus-5",
            "gpt-4-0613",
            "gpt-6.-sol",
            "gpt-6..1",
            "gpt-sol",
            "gpt-6-latest",
            "gpt-",
            "gpt",
            "",
            "GPT-6-sol",
            "gpt-99999999999999999999-sol",
        ] {
            assert_eq!(parse_id(id), None, "{id}");
        }
    }

    #[test]
    fn versions_compare_as_integer_tuples() {
        let set = ["gpt-5.6-sol", "gpt-6-sol", "gpt-6.1-sol"];
        assert_eq!(target(&set, "gpt-sol-latest").unwrap(), "gpt-6.1-sol");
        assert_eq!(
            target(&["gpt-9-sol", "gpt-10-sol"], "gpt-sol-latest").unwrap(),
            "gpt-10-sol"
        );
        assert_eq!(
            target(&["gpt-5.9-sol", "gpt-5.10-sol"], "gpt-sol-latest").unwrap(),
            "gpt-5.10-sol"
        );
        assert_eq!(
            target(&["gpt-6-sol", "gpt-6.0.1-sol"], "gpt-sol-latest").unwrap(),
            "gpt-6.0.1-sol"
        );
    }

    #[test]
    fn ties_go_to_the_alphabetically_first_id() {
        let set = ["gpt-6-sol", "gpt-6-luna", "gpt-6-astra", "gpt-5.6-terra"];
        assert_eq!(target(&set, "gpt-latest").unwrap(), "gpt-6-astra");
        // Same id order regardless of input order.
        let reversed = ["gpt-5.6-terra", "gpt-6-astra", "gpt-6-luna", "gpt-6-sol"];
        assert_eq!(target(&reversed, "gpt-latest").unwrap(), "gpt-6-astra");
        // 6 and 6.0 are one version: the tie falls back to the id.
        assert_eq!(
            target(&["gpt-6.0-sol", "gpt-6-sol"], "gpt-sol-latest").unwrap(),
            "gpt-6-sol"
        );
    }

    #[test]
    fn variants_stay_out_of_the_family_latest_but_have_their_own_alias() {
        let set = [
            "gpt-5.5",
            "gpt-5.6-mini",
            "gpt-5.7-codex",
            "gpt-5.8-codex-spark",
        ];
        assert_eq!(target(&set, "gpt-latest").unwrap(), "gpt-5.5");
        assert_eq!(target(&set, "gpt-mini-latest").unwrap(), "gpt-5.6-mini");
        assert_eq!(target(&set, "gpt-codex-latest").unwrap(), "gpt-5.7-codex");
        assert_eq!(
            target(&set, "gpt-codex-spark-latest").unwrap(),
            "gpt-5.8-codex-spark"
        );
        // A fast copy never wins, even when it is the newest id.
        assert_eq!(
            target(&["gpt-6-sol", "gpt-7-sol-fast"], "gpt-sol-latest").unwrap(),
            "gpt-6-sol"
        );
    }

    #[test]
    fn real_list_gives_the_documented_table() {
        let aliases = compute(&models(REAL));
        let table: Vec<(&str, &str)> = aliases
            .iter()
            .map(|a| (a.alias.as_str(), a.target.as_str()))
            .collect();
        assert_eq!(
            table,
            vec![
                ("gpt-astra-latest", "gpt-6-astra"),
                ("gpt-codex-latest", "gpt-5.3-codex"),
                ("gpt-codex-spark-latest", "gpt-5.3-codex-spark"),
                ("gpt-latest", "gpt-6.1-sol"),
                ("gpt-luna-latest", "gpt-6-luna"),
                ("gpt-mini-latest", "gpt-5.4-mini"),
                ("gpt-sol-latest", "gpt-6.1-sol"),
                ("gpt-terra-latest", "gpt-5.6-terra"),
            ]
        );
    }

    #[test]
    fn aliases_never_collide_with_real_ids() {
        let aliases = compute(&models(REAL));
        for alias in &aliases {
            assert!(!REAL.contains(&alias.alias.as_str()), "{}", alias.alias);
        }
        // A real id with an alias shape wins: the alias is dropped.
        let mut set = REAL.to_vec();
        set.push("gpt-sol-latest");
        assert!(
            compute(&models(&set))
                .iter()
                .all(|a| a.alias != "gpt-sol-latest")
        );
        assert_eq!(
            resolve(&models(&set), "gpt-sol-latest"),
            Resolution::NotAlias
        );
    }

    #[test]
    fn a_new_model_takes_over_without_any_other_change() {
        let mut set = REAL.to_vec();
        assert_eq!(target(&set, "gpt-sol-latest").unwrap(), "gpt-6.1-sol");
        set.push("gpt-7-sol");
        assert_eq!(target(&set, "gpt-sol-latest").unwrap(), "gpt-7-sol");
        assert_eq!(target(&set, "gpt-latest").unwrap(), "gpt-7-sol");
        assert_eq!(target(&set, "gpt-luna-latest").unwrap(), "gpt-6-luna");
    }

    #[test]
    fn alias_shapes_and_missing_candidates() {
        assert_eq!(parse_alias("gpt-latest"), Some(("gpt", None)));
        assert_eq!(
            parse_alias("gpt-codex-spark-latest"),
            Some(("gpt", Some("codex-spark".to_string())))
        );
        for bad in [
            "latest",
            "-latest",
            "gpt-",
            "gpt-6-latest",
            "claude-latest",
            "gpt-sol",
        ] {
            assert_eq!(parse_alias(bad), None, "{bad}");
        }
        let set = models(&["gpt-6-sol"]);
        assert_eq!(
            resolve(&set, "gpt-nova-latest"),
            Resolution::NoCandidates {
                alias: "gpt-nova-latest".into()
            }
        );
        // Only the gpt family has aliases: other families are not alias-shaped.
        for other in [
            "grok-latest",
            "kimi-latest",
            "claude-latest",
            "cursor-latest",
        ] {
            assert_eq!(parse_alias(other), None, "{other}");
            assert_eq!(resolve(&set, other), Resolution::NotAlias, "{other}");
        }
        assert_eq!(resolve(&set, "gpt-6-sol"), Resolution::NotAlias);
        assert_eq!(resolve(&set, "opus"), Resolution::NotAlias);
    }

    #[test]
    fn scope_text_names_tier_and_family() {
        let aliases = compute(&models(REAL));
        let sol = aliases
            .iter()
            .find(|a| a.alias == "gpt-sol-latest")
            .unwrap();
        assert_eq!(sol.scope(), "newest sol model of the gpt family");
        let any = aliases.iter().find(|a| a.alias == "gpt-latest").unwrap();
        assert_eq!(any.scope(), "newest model of the gpt family");
    }
}
