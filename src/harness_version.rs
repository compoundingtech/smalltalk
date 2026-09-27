//! Shared release-version parsing for the harness admission gates.
//!
//! Each maintained driver gates its provider on a version it was actually measured against. The
//! *policy* — which releases are admitted, and what evidence admitting one costs — stays with the
//! harness that owns it, because those policies genuinely differ: omp gates the launch, opencode
//! degrades to no native delivery, and codex refuses semantic-version reasoning outright.
//!
//! What is shared is only the *parsing*, because getting it wrong is subtle in exactly the same
//! way for every caller: `18.10` is not `18.1`, a bare `18` is not a release, and `18.0.9-rc1` is
//! not `18.0.9`. One tested parser is better than the same three edge cases reimplemented per
//! harness and drifting apart.

/// A parsed `MAJOR.MINOR.PATCH` release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Release {
    /// The `(major, minor)` series a minor-keyed gate admits.
    pub fn series(&self) -> (u32, u32) {
        (self.major, self.minor)
    }
}

/// Parse exactly `MAJOR.MINOR.PATCH`, all ASCII digits. Anything else is `None` so callers fail
/// closed.
///
/// Strictness is the whole point — it is what stands between a minor-keyed gate and "accept
/// anything that starts with 18":
/// - components are compared as NUMBERS by the caller, so `18.10.0` is a different series from
///   `18.1.0` rather than a string-prefix match of it;
/// - exactly three components are required, so a stray `18` or `7` elsewhere in a version banner
///   cannot be mistaken for a release;
/// - a pre-release or build-metadata suffix (`18.0.9-rc1`, `18.0.9+meta`) does NOT parse, and so
///   is never admitted as its base release. That is deliberate: a pre-release is not the build any
///   capture measured, so it has to be measured and admitted on its own.
pub fn parse_release(token: &str) -> Option<Release> {
    let mut parts = token.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let numeric = |part: &str| -> Option<u32> {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    Some(Release {
        major: numeric(major)?,
        minor: numeric(minor)?,
        patch: numeric(patch)?,
    })
}

/// Find the release a `--version` banner reports **for `provider` itself**, tolerating the forms
/// the providers actually print (`omp/18.0.9`, `v1.18.25`, a bare `1.18.25`). Returns the exact
/// token alongside the parsed release so a refusal can quote what the binary said.
///
/// Taking the first parseable token would be wrong, not merely imprecise: a banner that mentions
/// any other version — a runtime, an available-update notice — could bind the gate to a version
/// the provider is not. `runtime 18.0.0 omp/18.1.0` would admit on `18.0.0` and then launch an
/// unverified 18.1. So the provider's own label wins, and an unlabelled banner is only trusted
/// when it is unambiguous:
///
/// 1. the FIRST `<provider>/…` token DECIDES, because it is the provider naming itself. If what
///    it named parses, that is the answer; if it does not, the answer is `None` — the search
///    stops rather than falling through, or `omp/18.1.0-rc1 18.0.9` would admit on a release omp
///    never claimed. Two DISAGREEING own labels are therefore resolved by order, unlike rule 2,
///    where disagreeing unlabelled releases fail closed. No provider prints two, so that
///    asymmetry is recorded rather than resolved;
/// 2. with no own label, every parseable release must agree, since a single release repeated
///    across lines is still unambiguous;
/// 3. anything else (no release, an unreadable own label, or two disagreeing ones) is `None`, and
///    the caller fails closed.
pub fn find_release<'a>(printed: &'a str, provider: &str) -> Option<(&'a str, Release)> {
    let labelled = format!("{provider}/");
    let mut unlabelled: Option<(&'a str, Release)> = None;
    let mut ambiguous = false;

    for token in printed.split_whitespace() {
        if let Some(rest) = token.strip_prefix(labelled.as_str()) {
            // The provider naming itself outranks anything else in the banner — including the
            // case where what it named cannot be read. Falling through to the rest of the banner
            // would let a stray token stand in for a release the provider never claimed.
            return parse_release(rest).map(|release| (rest, release));
        }
        let bare = token.strip_prefix('v').unwrap_or(token);
        let Some(release) = parse_release(bare) else {
            continue;
        };
        match unlabelled {
            Some((_, seen)) if seen != release => ambiguous = true,
            Some(_) => {}
            None => unlabelled = Some((bare, release)),
        }
    }

    if ambiguous { None } else { unlabelled }
}

/// Render admitted series as `18.0.x` for a refusal message.
pub fn series_display(series: &[(u32, u32)]) -> String {
    series
        .iter()
        .map(|(major, minor)| format!("{major}.{minor}.x"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_release_parses_into_its_series() {
        assert_eq!(
            parse_release("18.0.9"),
            Some(Release {
                major: 18,
                minor: 0,
                patch: 9
            })
        );
        assert_eq!(parse_release("18.0.9").unwrap().series(), (18, 0));
    }

    /// The specific way a minor gate turns into "accept anything that starts with 18": series are
    /// numbers, so `18.10` and `18.1` are different, and neither is a prefix of the other.
    #[test]
    fn series_are_numeric_not_string_prefixes() {
        assert_eq!(parse_release("18.1.0").unwrap().series(), (18, 1));
        assert_eq!(parse_release("18.10.0").unwrap().series(), (18, 10));
        assert_ne!(
            parse_release("18.1.0").unwrap().series(),
            parse_release("18.10.0").unwrap().series()
        );
        // Leading zeros must not smuggle a different series past a numeric comparison.
        assert_eq!(parse_release("18.01.0").unwrap().series(), (18, 1));
    }

    #[test]
    fn a_prerelease_or_build_metadata_suffix_does_not_parse() {
        for token in ["18.0.9-rc1", "18.0.9+meta", "18.0.9rc1", "18.0.9_1"] {
            assert_eq!(parse_release(token), None, "{token} must not parse");
        }
    }

    #[test]
    fn only_exactly_three_numeric_components_parse() {
        for token in [
            "18", "18.0", "18.0.9.1", "18..9", "18.0.", ".0.9", "", "x.y.z",
        ] {
            assert_eq!(parse_release(token), None, "{token} must not parse");
        }
    }

    #[test]
    fn a_banner_yields_its_release_and_ignores_stray_numbers() {
        let (token, release) = find_release("build 7\nomp/18.0.9\n", "omp").unwrap();
        assert_eq!(token, "18.0.9");
        assert_eq!(release.series(), (18, 0));
        assert_eq!(
            find_release("opencode/1.18.25", "opencode")
                .unwrap()
                .1
                .series(),
            (1, 18)
        );
        assert_eq!(
            find_release("v1.18.25", "opencode").unwrap().1.series(),
            (1, 18)
        );
        // The shape omp actually prints for `--version` in the wild.
        assert_eq!(
            find_release("omp v18.0.3\n18.0.3\n", "omp")
                .unwrap()
                .1
                .series(),
            (18, 0)
        );
    }

    /// The provider naming itself outranks any other version in the banner. Without this, a
    /// banner mentioning an unrelated release binds the gate to the wrong version — the omp gate
    /// would admit on `18.0.0` and then launch an unverified 18.1 provider.
    #[test]
    fn a_labelled_release_wins_over_any_other_version_in_the_banner() {
        let (token, release) = find_release("runtime 18.0.0 omp/18.1.0", "omp").unwrap();
        assert_eq!(token, "18.1.0");
        assert_eq!(release.series(), (18, 1));

        let (token, release) = find_release("omp/18.0.9 node 22.3.1", "omp").unwrap();
        assert_eq!(token, "18.0.9");
        assert_eq!(release.series(), (18, 0));
    }

    /// With no label to disambiguate, two DIFFERENT releases must fail closed rather than let the
    /// first one win. The same release repeated is still unambiguous and is accepted.
    #[test]
    fn an_unlabelled_banner_is_trusted_only_when_it_is_unambiguous() {
        assert!(find_release("runtime 18.0.0 18.1.0", "omp").is_none());
        assert!(find_release("1.18.25 2.0.0", "opencode").is_none());
        assert_eq!(
            find_release("18.0.3\n18.0.3\n", "omp").unwrap().1.series(),
            (18, 0)
        );
    }

    /// The provider naming itself with something this parser cannot read is a REFUSAL, not a
    /// reason to keep looking. Falling through to the rest of the banner is how the gate ends up
    /// bound to a token the provider never claimed: `omp/18.1.0-rc1 18.0.9` would admit on the
    /// stray `18.0.9` and launch a provider that just said it was `18.1.0-rc1`. DQ-OMP-5 leaves
    /// open whether omp's update banner adds exactly such a second token.
    #[test]
    fn an_unreadable_own_label_fails_closed_instead_of_falling_through() {
        // The banner from the finding: an own label that does not parse, plus a stray release
        // whose minor IS admitted. Reading the stray one is the whole bug.
        assert!(find_release("omp/18.1.0-rc1 18.0.9", "omp").is_none());
        // Not specific to pre-releases: any unreadable own label ends the search.
        assert!(find_release("omp/nightly 18.0.9", "omp").is_none());
        assert!(find_release("omp/ 18.0.9", "omp").is_none());
        // The same parser backs the opencode gate, so it fails closed the same way.
        assert!(find_release("opencode/1.18.25-beta 1.18.19", "opencode").is_none());
        // An own label that DOES parse still decides, so the refusal above is about
        // unreadability and not about labels in general.
        assert_eq!(
            find_release("omp/18.0.11 18.0.9", "omp").unwrap().0,
            "18.0.11"
        );
    }

    /// A label that belongs to a DIFFERENT provider must not be read as this provider's version.
    #[test]
    fn another_providers_label_does_not_satisfy_this_provider() {
        // `opencode/1.18.25` is not omp naming itself; with nothing else in the banner there is
        // no omp release to bind to, so the omp caller fails closed.
        assert!(find_release("opencode/1.18.25", "omp").is_none());
    }

    #[test]
    fn a_banner_with_no_release_is_none() {
        assert!(find_release("not-a-version", "omp").is_none());
        assert!(find_release("18", "omp").is_none());
        assert!(find_release("", "omp").is_none());
    }

    #[test]
    fn series_display_reads_as_a_patch_series() {
        assert_eq!(series_display(&[(18, 0)]), "18.0.x");
        assert_eq!(series_display(&[(1, 18), (1, 19)]), "1.18.x, 1.19.x");
    }
}
