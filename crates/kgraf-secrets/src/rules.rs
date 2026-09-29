//! Pattern rules ported from gitleaks's default rule set (MIT license).
//!
//! Source: `third_party/gitleaks/config/gitleaks-rules-subset.toml`, copied
//! verbatim from gitleaks/gitleaks `config/gitleaks.toml` at commit
//! `b58d3f102cf3a2c84cb7f923d05c25c9b1aed84b` (see `third_party/gitleaks/UPSTREAM`).
//! Each `pattern` field below is the upstream `regex` value, with one deliberate
//! widening: the trailing delimiter class also accepts `, ) ] } > . :`, so a
//! key that ends a list, a call, a JSON value or a sentence is still found.
//! Upstream's class stops at a quote, whitespace or `;`. Go's
//! `regexp` (RE2) and Rust's `regex` crate share the same non-backtracking
//! syntax, so every pattern here compiles as it does upstream. `kind` is
//! gitleaks's rule `id`, kept as the transcript's typed marker so a redaction
//! names what was found without keeping the value (`[redacted:<kind>]`).
//!
//! Curated subset, not the full ~220-rule set (SR: harness-path code is
//! benchmarked and reviewed; every included rule needs a true- and a
//! false-positive test, so the set stays the ones kGraf's own paths can emit:
//! the AI providers in the fleet catalog, common cloud/VCS/package-registry
//! credentials a tool's output could echo, and generic high-entropy shapes
//! (JWT, PEM private key, a `key=`/`token=`-shaped assignment). Extending this
//! table is: add a row, add its two tests.
//!
//! Two rules are not from gitleaks, and are new code, not ports.
//! `generic-url-credential`: none of gitleaks' default rules cover a bare
//! `scheme://user:pass@host` URL in general (only per-vendor ones, e.g.
//! `sidekiq-sensitive-url`), and the task calls out "credentials in URLs" by
//! name. `git-url-token`: a bare 40-hex token as a URL's
//! whole userinfo, which `generic-url-credential` misses.

/// One pattern rule: `kind` is the typed marker, `pattern` the regex.
///
/// `capture`: which capture group to redact. `Some(1)` for a rule whose group 1
/// bounds exactly the secret (context such as `key=` or a URL host stays legible);
/// `None` to redact the whole match, for a rule with no such group or whose named
/// groups (`jwt-base64`) only dispatch on a header fragment rather than bound the
/// secret.
///
/// `entropy`: upstream's Shannon-entropy floor for this rule (`config/gitleaks.toml`'s
/// `entropy` field), in bits per byte of the captured value. Enforced by
/// `Scrubber::scrub_persist` only for `generic-api-key` (a structural rule with a fixed prefix and a fixed length (`ghp_` plus
/// exactly 36 characters, and the like) is already low-false-positive by
/// construction -- every known false positive was a `generic-api-key` match -- so gating the other 42 rules on
/// entropy too would only add untested, unmeasured behavior for a risk the data
/// does not show. `Some` is still recorded here for every rule upstream gates,
/// as a documented, faithful value, not a dead field: extending the gate to
/// another rule is a one-line change in `scrub_persist`, not a data migration.
pub struct PatternRule {
    pub kind: &'static str,
    pub pattern: &'static str,
    pub capture: Option<usize>,
    pub entropy: Option<f64>,
}

/// Ported gitleaks rules, plus `generic-url-credential` and `git-url-token`
/// (see module docs).
pub static RULES: &[PatternRule] = &[
    PatternRule {
        kind: "anthropic-admin-api-key",
        pattern: r#"\b(sk-ant-admin01-[a-zA-Z0-9_\-]{93}AA)(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: None,
    },
    PatternRule {
        kind: "anthropic-api-key",
        pattern: r#"\b(sk-ant-api03-[a-zA-Z0-9_\-]{93}AA)(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: None,
    },
    PatternRule {
        kind: "openai-api-key",
        pattern: r#"\b(sk-(?:proj|svcacct|admin)-(?:[A-Za-z0-9_-]{74}|[A-Za-z0-9_-]{58})T3BlbkFJ(?:[A-Za-z0-9_-]{74}|[A-Za-z0-9_-]{58})\b|sk-[a-zA-Z0-9]{20}T3BlbkFJ[a-zA-Z0-9]{20})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "cohere-api-token",
        pattern: r#"[\w.-]{0,50}?(?i:[\w.-]{0,50}?(?:cohere|CO_API_KEY)(?:[ \t\w.-]{0,20})[\s'"]{0,3})(?:=|>|:{1,3}=|\|\||:|=>|\?=|,)[\x60'"\s=]{0,5}([a-zA-Z0-9]{40})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(4.0),
    },
    PatternRule {
        kind: "huggingface-access-token",
        pattern: r#"\b(hf_(?i:[a-z]{34}))(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(2.0),
    },
    PatternRule {
        kind: "huggingface-organization-api-token",
        pattern: r#"\b(api_org_(?i:[a-z]{34}))(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(2.0),
    },
    PatternRule {
        kind: "perplexity-api-key",
        pattern: r#"\b(pplx-[a-zA-Z0-9]{48})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$|\b)"#,
        capture: Some(1),
        entropy: Some(4.0),
    },
    PatternRule {
        kind: "aws-access-token",
        pattern: r#"\b((?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16})\b"#,
        capture: Some(1),
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "gcp-api-key",
        pattern: r#"\b(AIza[\w-]{35})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(4.0),
    },
    PatternRule {
        kind: "azure-ad-client-secret",
        pattern: r#"(?:^|[\\'"\x60\s>=:(,)])([a-zA-Z0-9_~.]{3}\dQ~[a-zA-Z0-9_~.-]{31,34})(?:$|[\\'"\x60\s<),])"#,
        capture: Some(1),
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "digitalocean-access-token",
        pattern: r#"\b(doo_v1_[a-f0-9]{64})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "digitalocean-pat",
        pattern: r#"\b(dop_v1_[a-f0-9]{64})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "github-pat",
        pattern: r#"ghp_[0-9a-zA-Z]{36}"#,
        capture: None,
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "github-fine-grained-pat",
        pattern: r#"github_pat_\w{82}"#,
        capture: None,
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "github-oauth",
        pattern: r#"gho_[0-9a-zA-Z]{36}"#,
        capture: None,
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "github-app-token",
        pattern: r#"(?:ghu|ghs)_[0-9a-zA-Z]{36}"#,
        capture: None,
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "github-refresh-token",
        pattern: r#"ghr_[0-9a-zA-Z]{36}"#,
        capture: None,
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "gitlab-pat",
        pattern: r#"glpat-[\w-]{20}"#,
        capture: None,
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "gitlab-pat-routable",
        pattern: r#"\bglpat-[0-9a-zA-Z_-]{27,300}\.[0-9a-z]{2}[0-9a-z]{7}\b"#,
        capture: None,
        entropy: Some(4.0),
    },
    PatternRule {
        kind: "jwt",
        pattern: r#"\b(ey[a-zA-Z0-9]{17,}\.ey[a-zA-Z0-9\/\\_-]{17,}\.(?:[a-zA-Z0-9\/\\_-]{10,}={0,2})?)(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "jwt-base64",
        pattern: r#"\bZXlK(?:(?P<alg>aGJHY2lPaU)|(?P<apu>aGNIVWlPaU)|(?P<apv>aGNIWWlPaU)|(?P<aud>aGRXUWlPaU)|(?P<b64>aU5qUWlP)|(?P<crit>amNtbDBJanBi)|(?P<cty>amRIa2lPaU)|(?P<epk>bGNHc2lPbn)|(?P<enc>bGJtTWlPaU)|(?P<jku>cWEzVWlPaU)|(?P<jwk>cWQyc2lPb)|(?P<iss>cGMzTWlPaU)|(?P<iv>cGRpSTZJ)|(?P<kid>cmFXUWlP)|(?P<key_ops>clpYbGZiM0J6SWpwY)|(?P<kty>cmRIa2lPaUp)|(?P<nonce>dWIyNWpaU0k2)|(?P<p2c>d01tTWlP)|(?P<p2s>d01uTWlPaU)|(?P<ppt>d2NIUWlPaU)|(?P<sub>emRXSWlPaU)|(?P<svt>emRuUWlP)|(?P<tag>MFlXY2lPaU)|(?P<typ>MGVYQWlPaUp)|(?P<url>MWNtd2l)|(?P<use>MWMyVWlPaUp)|(?P<ver>MlpYSWlPaU)|(?P<version>MlpYSnphVzl1SWpv)|(?P<x>NElqb2)|(?P<x5c>NE5XTWlP)|(?P<x5t>NE5YUWlPaU)|(?P<x5ts256>NE5YUWpVekkxTmlJNkl)|(?P<x5u>NE5YVWlPaU)|(?P<zip>NmFYQWlPaU))[a-zA-Z0-9\/\\_+\-\r\n]{40,}={0,2}"#,
        capture: None,
        entropy: Some(2.0),
    },
    PatternRule {
        // MODIFIED from upstream (see third_party/gitleaks/UPSTREAM): the body
        // class `[\s\S-]` ("any character, redundantly including `-`", Go's
        // way of spelling dotall without an inline flag) is replaced with the
        // equivalent `(?su:.)`. `Scrubber::new` builds the whole `RULES` table
        // with `.unicode(false)` (needed so 44 rules combine into one
        // `RegexSet` under a sane compiled-size limit), which also turns `.`
        // into "any byte", and a byte class can split a multi-byte UTF-8
        // sequence, which the `str`-based `Regex` refuses outright. Re-opening
        // Unicode mode for just this one group (`u`) keeps `.` matching one
        // whole scalar value, body content included, same as upstream; `s`
        // keeps it matching a newline the way `[\s\S]` always did.
        kind: "private-key",
        pattern: r#"(?i)-----BEGIN[ A-Z0-9_-]{0,100}PRIVATE KEY(?: BLOCK)?-----(?su:.{64,}?)KEY(?: BLOCK)?-----"#,
        capture: None,
        entropy: None,
    },
    PatternRule {
        kind: "slack-webhook-url",
        pattern: r#"(?:https?://)?hooks\.slack\.com/(?:services|workflows|triggers)/[A-Za-z0-9+/]{43,56}"#,
        capture: None,
        entropy: None,
    },
    PatternRule {
        kind: "slack-bot-token",
        pattern: r#"xoxb-[0-9]{10,13}-[0-9]{10,13}[a-zA-Z0-9-]*"#,
        capture: None,
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "slack-legacy-token",
        pattern: r#"xox[os]-\d+-\d+-\d+-[a-fA-F\d]+"#,
        capture: None,
        entropy: Some(2.0),
    },
    PatternRule {
        kind: "discord-api-token",
        pattern: r#"(?i)[\w.-]{0,50}?(?:discord)(?:[ \t\w.-]{0,20})[\s'"]{0,3}(?:=|>|:{1,3}=|\|\||:|=>|\?=|,)[\x60'"\s=]{0,5}([a-f0-9]{64})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: None,
    },
    PatternRule {
        kind: "stripe-access-token",
        pattern: r#"\b((?:sk|rk)_(?:test|live|prod)_[a-zA-Z0-9]{10,99})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(2.0),
    },
    PatternRule {
        kind: "sidekiq-sensitive-url",
        pattern: r#"(?i)\bhttps?://([a-f0-9]{8}:[a-f0-9]{8})@(?:gems\.contribsys\.com|enterprise\.contribsys\.com)(?:[\/|\#|\?|:]|$)"#,
        capture: Some(1),
        entropy: None,
    },
    PatternRule {
        kind: "npm-access-token",
        pattern: r#"(?i)\b(npm_[a-z0-9]{36})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(2.0),
    },
    PatternRule {
        kind: "pypi-upload-token",
        pattern: r#"pypi-AgEIcHlwaS5vcmc[\w-]{50,1000}"#,
        capture: None,
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "rubygems-api-token",
        pattern: r#"\b(rubygems_[a-f0-9]{48})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(2.0),
    },
    PatternRule {
        kind: "hashicorp-tf-api-token",
        pattern: r#"(?i)[a-z0-9]{14}\.(?-i:atlasv1)\.[a-z0-9\-_=]{60,70}"#,
        capture: None,
        entropy: Some(3.5),
    },
    PatternRule {
        kind: "vault-service-token",
        pattern: r#"\b((?:hvs\.[\w-]{90,120}|s\.(?i:[a-z0-9]{24})))(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(3.5),
    },
    PatternRule {
        kind: "vault-batch-token",
        pattern: r#"\b(hvb\.[\w-]{138,300})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(4.0),
    },
    PatternRule {
        kind: "twilio-api-key",
        pattern: r#"SK[0-9a-fA-F]{32}"#,
        capture: None,
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "sendgrid-api-token",
        pattern: r#"\b(SG\.(?i)[a-z0-9=_\-\.]{66})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(2.0),
    },
    PatternRule {
        kind: "mailgun-private-api-token",
        pattern: r#"(?i)[\w.-]{0,50}?(?:mailgun)(?:[ \t\w.-]{0,20})[\s'"]{0,3}(?:=|>|:{1,3}=|\|\||:|=>|\?=|,)[\x60'"\s=]{0,5}(key-[a-f0-9]{32})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: None,
    },
    PatternRule {
        kind: "square-access-token",
        pattern: r#"\b((?:EAAA|sq0atp-)[\w-]{22,60})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(2.0),
    },
    PatternRule {
        kind: "shopify-access-token",
        pattern: r#"shpat_[a-fA-F0-9]{32}"#,
        capture: None,
        entropy: Some(2.0),
    },
    PatternRule {
        kind: "new-relic-user-api-key",
        pattern: r#"(?i)[\w.-]{0,50}?(?:new-relic|newrelic|new_relic)(?:[ \t\w.-]{0,20})[\s'"]{0,3}(?:=|>|:{1,3}=|\|\||:|=>|\?=|,)[\x60'"\s=]{0,5}(NRAK-[a-z0-9]{27})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: None,
    },
    PatternRule {
        kind: "grafana-api-key",
        pattern: r#"(?i)\b(eyJrIjoi[A-Za-z0-9]{70,400}={0,3})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(3.0),
    },
    PatternRule {
        kind: "planetscale-password",
        pattern: r#"(?i)\b(pscale_pw_(?i)[\w=\.-]{32,64})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(3.0),
    },
    PatternRule {
        // Placed last (not in its upstream table position): several other
        // rules above also share this rule's `key=`/`token=`-shaped context,
        // so a value like `discord_token="<64 hex chars>"` matches both this
        // rule and `discord-api-token` over the same span. `Scrubber::new`
        // collects matches by ascending rule index and its sort is stable, so
        // whichever rule is earlier in `RULES` wins an exact-span tie; putting
        // the generic fallback last means every specific rule above it always
        // wins that tie, and this one only ever fires alone, on a shape none
        // of the specific rules recognized.
        kind: "generic-api-key",
        pattern: r#"(?i)[\w.-]{0,50}?(?:access|auth|(?-i:[Aa]pi|API)|credential|creds|key|passw(?:or)?d|secret|token)(?:[ \t\w.-]{0,20})[\s'"]{0,3}(?:=|>|:{1,3}=|\|\||:|=>|\?=|,)[\x60'"\s=]{0,5}([\w.=-]{10,150}|[a-z0-9][a-z0-9+/]{11,}={0,3})(?:[\x60'"\s;,)\]}>.:]|\\[nr]|$)"#,
        capture: Some(1),
        entropy: Some(3.5),
    },
    PatternRule {
        // `(?u:...)`: the negated classes (`[^\s:@/'"]`) would otherwise, under
        // `Scrubber::new`'s ASCII-only `RegexSet`, match any byte outside that
        // set, continuation bytes of a multi-byte UTF-8 sequence included,
        // which the `str`-based `Regex` refuses to compile. Scoping Unicode
        // back on for this one rule costs nothing measurable (it is one rule
        // among 44, not the 32-way alternation that motivated ASCII-only in
        // the first place) and a URL's userinfo is never meant to hold
        // non-ASCII credential bytes regardless.
        kind: "generic-url-credential",
        pattern: r#"(?u:\b[a-zA-Z][a-zA-Z0-9+.-]*://([^\s:@/'"]+:[^\s@/'"]{3,})@[^\s/'"]+)"#,
        capture: Some(1),
        entropy: None,
    },
    PatternRule {
        // Not from gitleaks: a bare 40-hex token as
        // a URL's whole userinfo, with no `:password` part, the form
        // `https://<token>@github.com/...` takes with a classic GitHub token
        // or a GitLab or Gitea access token. `generic-url-credential` needs
        // `user:pass` and misses it. Forty hex characters then `@` right
        // after `://` is not a commit hash, which sits in a path.
        kind: "git-url-token",
        pattern: r#"\b[a-zA-Z][a-zA-Z0-9+.-]*://([0-9a-fA-F]{40})@"#,
        capture: Some(1),
        entropy: None,
    },
];
