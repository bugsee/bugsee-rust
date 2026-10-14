//
//  scrub.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! General-purpose PII / secret scrubber applied to everything that goes into a
//! report bundle (capture channels, `crash.json`, report metadata, environment).
//!
//! It is a small hand-written, regex-free matcher (no new dependency) that
//! recognises:
//!
//! * **emails** (`user@example.com` -> `[email]`),
//! * **tokens / keys** with a well-known shape (JWTs, `sk_live_...`, `ghp_...`,
//!   `AKIA...`, `xox?-...`, `hf_...`, `Bearer ...`, PEM private-key blocks) and
//!   the password in a URL's userinfo (`redis://:pw@host`) -> `[token]` /
//!   `[redacted]`,
//! * **keyed secrets**: the value after `password=`, `"api_key": "..."`,
//!   `Authorization: ...` and similar -> `[redacted]`; and, in structured JSON,
//!   every string under an object key that names a secret,
//! * **card-like numbers** (real card layouts only, Luhn-valid, plausible
//!   issuer) -> `[card]`,
//! * **home-directory usernames** in paths (`/Users/alice/...`,
//!   `/home/alice/...`, `C:\Users\alice\...`) -> `[user]`.
//!
//! # Where it runs
//!
//! At report **assembly** time (the "exporter"), on the capture worker thread,
//! *after* `before_send`, so values a host callback adds are scrubbed too. It is
//! never run on a fault path and never on the caller's thread. Parts on the
//! local disk are not rewritten; only what leaves the process is scrubbed.
//! Binary payloads (user attachments, minidumps) are opaque and untouched.
//!
//! # Cost
//!
//! Each detector makes one forward pass over a string with no backtracking, and
//! the pathological-input tests bound the whole thing to a few passes (the
//! detectors share run caches so adversarial input such as `sk-sk-sk-...` or
//! `password=password=...` cannot go quadratic). Clean strings are returned
//! borrowed (no allocation); a string is copied only when something matched.
//! Measured in release on an Apple-silicon laptop
//! (`cargo test -p bugsee-core --release scrub_throughput -- --ignored --nocapture`):
//! about 0.4 us for an 80-byte clean string (~5 ns/byte), and about 135 us for a
//! 100-record, 13 KB log channel (~10 ns/byte including the record split). Exporting a
//! capture channel splits it into records and only fully parses a record when a
//! cheap byte pre-check says it could contain something; see
//! [`Scrubber::scrub_envelope`]. A full 100k-entry window is a fraction of a
//! second of worker-thread time per report, which the rate limiter bounds.
//!
//! # Extension points
//!
//! [`ScrubConfig::allow`] keeps specific matched values, [`ScrubConfig::deny`]
//! always redacts literal substrings, and [`ScrubConfig::sensitive_keys`] adds
//! key names whose values are treated as secrets. Each detector can be turned
//! off, and [`ScrubConfig::disabled`] switches the whole scrubber off.
//!
//! # Key matching
//!
//! [`is_sensitive_key`] is the one credential-key matcher; `bugsee-reqwest`'s
//! URL/header/body sanitiser uses it too. In *structured* data (JSON keys,
//! headers, query parameters) it is a normalised substring test. In *free
//! text* (`name=value`, `name: value`) a looser test would redact ordinary
//! prose, so only unambiguous names (`password`, `token`, ...) match as a
//! suffix, and the ambiguous ones (`key`, `auth`, `session`, `signature`, ...)
//! additionally need `=` or a quoted key, and a value that looks like a secret
//! (8+ characters including a digit).

use std::borrow::Cow;

use serde_json::Value;

const EMAIL: &str = "[email]";
const TOKEN: &str = "[token]";
const CARD: &str = "[card]";
const USER: &str = "[user]";
const REDACTED: &str = "[redacted]";
const MARKERS: &[&str] = &[EMAIL, TOKEN, CARD, USER, REDACTED];

/// What the scrubber does. The default scrubs everything built in.
///
/// Non-exhaustive so options can be added without a breaking change; build with
/// [`ScrubConfig::default`] and the chainable setters, or assign fields.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ScrubConfig {
    /// Master switch. `false` makes the scrubber a no-op.
    pub enabled: bool,
    /// Redact email addresses.
    pub emails: bool,
    /// Redact well-known token / key shapes, PEM private keys, `Bearer`
    /// credentials and URL userinfo passwords.
    pub tokens: bool,
    /// Redact `key=value` / `"key": "value"` pairs whose key names a secret
    /// (and strings under such keys in structured JSON).
    pub keyed: bool,
    /// Redact Luhn-valid card-like numbers.
    pub cards: bool,
    /// Redact the user name in home-directory paths.
    pub paths: bool,
    /// Matched values left alone. A match is kept when its text equals an entry,
    /// or when the entry starts with `@` and the (email) match ends with it, so
    /// `"@example.com"` keeps every example.com address. `deny` beats `allow`:
    /// a literal in `deny` is redacted even where an `allow` entry would keep it.
    pub allow: Vec<String>,
    /// Literal substrings that are always redacted (e.g. an internal host name
    /// or a customer id).
    pub deny: Vec<String>,
    /// Extra key names treated as secrets, in addition to the built-ins.
    /// Compared case-insensitively, ignoring `_`/`-`/spaces; a substring match
    /// in structured data, a suffix match in free text.
    pub sensitive_keys: Vec<String>,
}

impl Default for ScrubConfig {
    fn default() -> Self {
        ScrubConfig {
            enabled: true,
            emails: true,
            tokens: true,
            keyed: true,
            cards: true,
            paths: true,
            allow: Vec::new(),
            deny: Vec::new(),
            sensitive_keys: Vec::new(),
        }
    }
}

impl ScrubConfig {
    /// A configuration with the scrubber switched off entirely.
    pub fn disabled() -> Self {
        ScrubConfig {
            enabled: false,
            ..Default::default()
        }
    }

    /// Keep matches equal to `value` (or, for `@domain`, emails at that domain).
    pub fn allow(mut self, value: impl Into<String>) -> Self {
        self.allow.push(value.into());
        self
    }

    /// Always redact the literal substring `value`.
    pub fn deny(mut self, value: impl Into<String>) -> Self {
        self.deny.push(value.into());
        self
    }

    /// Treat values under the key name `name` as secrets.
    pub fn sensitive_key(mut self, name: impl Into<String>) -> Self {
        self.sensitive_keys.push(name.into());
        self
    }

    /// Enable/disable email redaction.
    pub fn emails(mut self, on: bool) -> Self {
        self.emails = on;
        self
    }

    /// Enable/disable token / key redaction.
    pub fn tokens(mut self, on: bool) -> Self {
        self.tokens = on;
        self
    }

    /// Enable/disable keyed-secret redaction.
    pub fn keyed(mut self, on: bool) -> Self {
        self.keyed = on;
        self
    }

    /// Enable/disable card-number redaction.
    pub fn cards(mut self, on: bool) -> Self {
        self.cards = on;
        self
    }

    /// Enable/disable home-directory user-name redaction.
    pub fn paths(mut self, on: bool) -> Self {
        self.paths = on;
        self
    }
}

/// Credential-bearing key stems for **structured** data (normalised: lowercase
/// alphanumerics). Substring match, so `clientSecret`, `x-session-id`,
/// `refresh_token` and `AccountKey` are all covered.
///
/// `key` on its own is deliberately NOT a substring stem (it would redact
/// `keyboard`, `monkey`, `keyword`); it, `sig`, `pin` and `otp` are matched
/// exactly instead. Compound `...key` names that matter are listed (`apikey`,
/// `accountkey`, `accesskey`, `privatekey`).
const STEMS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "pwd",
    "secret",
    "token",
    "apikey",
    "accountkey",
    "accesskey",
    "privatekey",
    "auth",
    "session",
    "cookie",
    "credential",
    "signature",
    "csrf",
    "xsrf",
    "bearer",
    "jwt",
    // OAuth 1.0a `oauth_verifier` and PKCE `code_verifier`.
    "verifier",
];

/// Names matched only exactly (too ambiguous to match as substrings).
const EXACT: &[&str] = &["key", "sig", "pin", "otp"];

/// Free-text names that are unambiguous: matched as a suffix.
const TEXT_STRONG: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "pwd",
    "secret",
    "token",
    "apikey",
    "accountkey",
    "accesskey",
    "privatekey",
    "authorization",
    "credential",
    "credentials",
    "cookie",
    "csrf",
    "xsrf",
    "jwt",
    "bearer",
    "sessionid",
    "sessiontoken",
    // OAuth 1.0a verifier/PIN and PKCE `code_verifier` (kept in step with `STEMS`).
    "verifier",
    "oauthpin",
];

/// Free-text names that are also ordinary words; see the module docs.
const TEXT_AMBIGUOUS_SUFFIX: &[&str] = &["auth", "session", "signature"];

/// Normalise `name` (lowercase alphanumerics only) into a small stack buffer,
/// falling back to the heap for very long keys.
fn with_normalized<R>(name: &str, f: impl FnOnce(&[u8]) -> R) -> R {
    let mut buf = [0u8; 64];
    let mut n = 0;
    for c in name.bytes().filter(u8::is_ascii_alphanumeric) {
        if n == buf.len() {
            let all: Vec<u8> = name
                .bytes()
                .filter(u8::is_ascii_alphanumeric)
                .map(|c| c.to_ascii_lowercase())
                .collect();
            return f(&all);
        }
        buf[n] = c.to_ascii_lowercase();
        n += 1;
    }
    f(&buf[..n])
}

/// Case-insensitive `starts_with` that lets `_`, `-`, space and `.` appear
/// between the letters/digits of `needle` (but not before the first).
fn fuzzy_starts_with(rest: &[u8], needle: &[u8]) -> bool {
    let mut ri = 0;
    for (ni, &nc) in needle.iter().enumerate() {
        if ni > 0 && nc.is_ascii_alphanumeric() {
            while ri < rest.len() && matches!(rest[ri], b'_' | b'-' | b' ' | b'.') {
                ri += 1;
            }
        }
        if ri >= rest.len() || !rest[ri].eq_ignore_ascii_case(&nc) {
            return false;
        }
        ri += 1;
    }
    true
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// Whether a key (JSON object key, header name, query/form parameter) names a
/// value that must be redacted. The single credential-key matcher shared by the
/// scrubber and `bugsee-reqwest`.
///
/// A stem does not count when the surrounding word makes it something else:
/// `author`/`authority`/`:authority` (not `auth`), `tokenizer`/`token_type`/
/// `token_count` (not `token`), `secretary`, `session_state`/`sessionStart`/
/// `session_timeout` (not a session id), `method_signature`. Remaining
/// trade-offs: `pwd` also matches the shell's `PWD`/`OLDPWD` variables (their
/// values are a working directory, so the path detector still scrubs the
/// username), and any other key containing a stem (`password_hint`) is redacted.
pub fn is_sensitive_key(name: &str) -> bool {
    with_normalized(name, |n| {
        EXACT.iter().any(|e| n == e.as_bytes()) || STEMS.iter().any(|s| stem_hit(n, s.as_bytes()))
    })
}

/// After `oauth` (the `o` is the stem's `before`), a name that is public
/// metadata (`oauth_client_id`, `oauth2_redirect_uri`, `oauth_scope`), as opposed
/// to `oauth_verifier`/`oauth_pin`/bare `oauth`, which stay secret. Token- and
/// secret-bearing names are caught by their own stems regardless.
fn public_oauth_suffix(rest: &[u8]) -> bool {
    let rest = &rest[rest.iter().take_while(|c| c.is_ascii_digit()).count()..];
    [
        &b"clientid"[..],
        b"clientname",
        b"redirect",
        b"callback",
        b"scope",
        b"issuer",
        b"endpoint",
        b"url",
        b"uri",
        b"version",
        b"provider",
        b"enabled",
        b"flow",
        b"config",
    ]
    .iter()
    .any(|x| rest.starts_with(x))
}

/// Whether `stem` occurs in `n` in a sense that is not excluded by its context.
fn stem_hit(n: &[u8], stem: &[u8]) -> bool {
    let mut from = 0;
    while from + stem.len() <= n.len() {
        let Some(p) = n[from..].windows(stem.len()).position(|w| w == stem) else {
            return false;
        };
        let at = from + p;
        let (before, rest) = (&n[..at], &n[at + stem.len()..]);
        let excluded = match stem {
            // `author`/`authority` (but not `authorization`), and
            // `www-authenticate`/`authentication`.
            // Public OIDC/OAuth metadata is excluded too: `authorization_endpoint`
            // /`_url`/`_uri`, `oauth*` names (`oauth_client_id`; token- and
            // secret-bearing ones such as `oauth_token`/`oauth_client_secret`
            // still hit the `token`/`secret` stems), and `auth_method`/
            // `token_endpoint_auth_signing_alg`.
            b"auth" => {
                (rest.starts_with(b"or") && !rest.starts_with(b"oriz"))
                    || rest.starts_with(b"enticat")
                    || (rest.starts_with(b"orization")
                        && [&b"endpoint"[..], b"url", b"uri"]
                            .iter()
                            .any(|x| rest[9..].starts_with(x)))
                    || (before.ends_with(b"o") && public_oauth_suffix(rest))
                    || [&b"url"[..], b"uri", b"endpoint"]
                        .iter()
                        .any(|x| rest.starts_with(x))
                    || rest.starts_with(b"method")
                    || rest.starts_with(b"signingalg")
            }
            // `tokenizer`, `token_type`/`_count`/`_ttl`/`_endpoint`/`_url`, and
            // `cancellation_token` (a cooperative-cancel handle, not a secret).
            b"token" => {
                [
                    &b"iz"[..],
                    b"type",
                    b"count",
                    b"len",
                    b"limit",
                    b"usage",
                    b"used",
                    b"budget",
                    b"endpoint",
                    b"url",
                    b"uri",
                    b"expir",
                    b"ttl",
                    b"scope",
                    b"kind",
                ]
                .iter()
                .any(|x| rest.starts_with(x))
                    || before.ends_with(b"cancellation")
                    || before.ends_with(b"cancel")
            }
            b"secret" => rest.starts_with(b"ary"),
            b"session" => [
                &b"start"[..],
                b"state",
                b"status",
                b"timeout",
                b"duration",
                b"count",
                b"length",
                b"type",
                b"ttl",
                b"age",
                b"expir",
            ]
            .iter()
            .any(|x| rest.starts_with(x)),
            b"signature" => [&b"method"[..], b"function", b"fn", b"type", b"call"]
                .iter()
                .any(|x| before.ends_with(x)),
            _ => false,
        };
        if !excluded {
            return true;
        }
        from = at + 1;
    }
    false
}

/// What a prefixed token's tail must look like beyond its length, so ordinary
/// identifiers that merely start with the prefix (`sk-cluster-node-01-primary`)
/// do not match.
#[derive(Clone, Copy)]
enum Need {
    None,
    /// A digit and both letter cases (OpenAI-style `sk-` keys).
    DigitAndMixed,
    /// A digit or both letter cases.
    DigitOrMixed,
    /// Exactly 16 characters of `[A-Z0-9]` (AWS access key ids).
    Upper16,
}

/// Prefixed token shapes: `(prefix, min tail length, tail shape)`. The tail is
/// a run of `[A-Za-z0-9_-]`.
const TOKEN_PREFIXES: &[(&str, usize, Need)] = &[
    ("sk_live_", 8, Need::None),
    ("sk_test_", 8, Need::None),
    ("rk_live_", 8, Need::None),
    ("rk_test_", 8, Need::None),
    ("whsec_", 16, Need::None),
    ("ghp_", 20, Need::None),
    ("gho_", 20, Need::None),
    ("ghu_", 20, Need::None),
    ("ghs_", 20, Need::None),
    ("ghr_", 20, Need::None),
    ("github_pat_", 20, Need::None),
    ("glpat-", 15, Need::None),
    ("xoxb-", 10, Need::None),
    ("xoxp-", 10, Need::None),
    ("xoxa-", 10, Need::None),
    ("xoxs-", 10, Need::None),
    ("xoxr-", 10, Need::None),
    ("sk-ant-", 20, Need::None),
    ("sk-", 20, Need::DigitAndMixed),
    ("hf_", 20, Need::DigitOrMixed),
    ("AKIA", 16, Need::Upper16),
    ("ASIA", 16, Need::Upper16),
    ("AIza", 30, Need::None),
    ("SG.", 20, Need::None),
    ("npm_", 30, Need::None),
    ("shpat_", 20, Need::None),
];

/// "Top-level domains" that are really file extensions (`logo@2x.png`).
const FILE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "bmp", "tif", "tiff", "css", "js", "ts",
    "json", "toml", "yaml", "yml", "txt", "md", "pdf", "html", "htm", "rs", "so", "dll", "dylib",
    "exe", "zip", "gz", "mp3", "mp4", "mov", "woff", "woff2", "ttf",
];

/// First bytes of every shape `tokens` can match (a word-start filter).
const TOKEN_FIRST: &[u8] = b"srgxwhASnebB";

struct Span {
    start: usize,
    end: usize,
    repl: &'static str,
}

/// A configured scrubber. Cheap to share (`Arc`) and `Send + Sync`.
#[derive(Debug, Clone)]
pub struct Scrubber {
    cfg: ScrubConfig,
    /// `cfg.sensitive_keys`, normalised once.
    extra_keys: Vec<String>,
}

impl Default for Scrubber {
    fn default() -> Self {
        Scrubber::new(ScrubConfig::default())
    }
}

/// How a secret-named structured key is redacted.
#[derive(Clone, Copy)]
struct StructKey {
    /// Only strings that look like a credential (see [`looks_like_secret`]).
    looks_only: bool,
    /// Numbers under the key are redacted too.
    numeric: bool,
}

/// A long, space-free, colon-free string: plausibly a credential rather than an
/// ordinary word or a `user:42:profile` cache key.
fn looks_like_secret(s: &str) -> bool {
    s.len() >= 16 && !s.contains(char::is_whitespace) && !s.contains(':')
}

/// Which free-text key class a name belongs to.
#[derive(PartialEq, Eq, Clone, Copy)]
enum KeyClass {
    None,
    Strong,
    Ambiguous,
    /// `Cookie`/`Set-Cookie`: the value runs to the end of the line.
    Cookie,
    /// A key the host listed in `sensitive_keys`: always redacted.
    Explicit,
    /// `pin` / `otp`: short numeric codes, redacted after `=` or a quoted key.
    Pin,
}

impl Scrubber {
    pub fn new(cfg: ScrubConfig) -> Self {
        let extra_keys = cfg
            .sensitive_keys
            .iter()
            .map(|k| {
                k.bytes()
                    .filter(u8::is_ascii_alphanumeric)
                    .map(|c| c.to_ascii_lowercase() as char)
                    .collect::<String>()
            })
            .filter(|k| !k.is_empty())
            .collect();
        Scrubber { cfg, extra_keys }
    }

    /// Whether this scrubber does anything.
    pub fn is_enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Scrub a string. Borrowed (no allocation) when nothing matched.
    pub fn scrub_str<'a>(&self, s: &'a str) -> Cow<'a, str> {
        if !self.cfg.enabled || s.is_empty() {
            return Cow::Borrowed(s);
        }
        let mut spans: Vec<Span> = Vec::new();
        if self.cfg.emails {
            self.emails(s, &mut spans);
        }
        if self.cfg.tokens {
            self.tokens(s, &mut spans);
            self.pem_blocks(s, &mut spans);
            self.url_userinfo(s, &mut spans);
        }
        if self.cfg.keyed {
            self.keyed(s, &mut spans);
        }
        if self.cfg.cards {
            self.cards(s, &mut spans);
        }
        if self.cfg.paths {
            self.paths(s, &mut spans);
        }
        for needle in &self.cfg.deny {
            if needle.is_empty() {
                continue;
            }
            for (at, m) in s.match_indices(needle.as_str()) {
                spans.push(Span {
                    start: at,
                    end: at + m.len(),
                    repl: REDACTED,
                });
            }
        }
        if spans.is_empty() {
            return Cow::Borrowed(s);
        }
        spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        let mut out = String::with_capacity(s.len());
        let mut at = 0;
        for sp in &spans {
            if sp.end <= at {
                continue; // fully inside an earlier span
            }
            if sp.start >= at {
                out.push_str(&s[at..sp.start]);
                out.push_str(sp.repl);
            }
            // A partial overlap extends the earlier redaction: nothing of the
            // later span's tail may survive.
            at = sp.end;
        }
        out.push_str(&s[at..]);
        Cow::Owned(out)
    }

    /// Scrub every string in `v` in place, and every string under an object key
    /// that names a secret. Numbers and booleans are never rewritten. Returns
    /// whether anything changed.
    pub fn scrub_value(&self, v: &mut Value) -> bool {
        if !self.cfg.enabled {
            return false;
        }
        // A card number stored as a JSON integer.
        if let Value::Number(n) = &*v {
            let hit = self.cfg.cards
                && n.as_u64().is_some_and(|u| {
                    let d = u.to_string();
                    (13..=19).contains(&d.len()) && self.has_card(&d)
                });
            if hit {
                *v = Value::String(CARD.to_string());
            }
            return hit;
        }
        match v {
            Value::String(s) => match self.scrub_str(s) {
                Cow::Borrowed(_) => false,
                Cow::Owned(n) => {
                    *s = n;
                    true
                }
            },
            Value::Array(a) => a.iter_mut().fold(false, |c, x| self.scrub_value(x) | c),
            Value::Object(m) => {
                let mut changed = false;
                for (k, x) in m.iter_mut() {
                    if let Some(sk) = self.structured_key(k).filter(|_| self.cfg.keyed) {
                        changed |= self.redact_strings(x, sk);
                    } else {
                        changed |= self.scrub_value(x);
                    }
                }
                changed
            }
            _ => false,
        }
    }

    /// Replace every string leaf under `v` (a secret-keyed value) by a marker;
    /// numbers too when the key unambiguously holds a secret (`pin`, `otp`,
    /// `password`, ...). Booleans and nulls are never touched.
    fn redact_strings(&self, v: &mut Value, sk: StructKey) -> bool {
        match v {
            Value::String(s) => {
                if s.is_empty()
                    || s == REDACTED
                    || self.allowed(s)
                    || (sk.looks_only && !looks_like_secret(s))
                {
                    false
                } else {
                    *s = REDACTED.to_string();
                    true
                }
            }
            Value::Number(_) if sk.numeric => {
                *v = Value::String(REDACTED.to_string());
                true
            }
            Value::Array(a) => a
                .iter_mut()
                .fold(false, |c, x| self.redact_strings(x, sk) | c),
            Value::Object(m) => m
                .values_mut()
                .fold(false, |c, x| self.redact_strings(x, sk) | c),
            _ => false,
        }
    }

    /// How a structured key should be redacted, or `None` if it is not secret.
    fn structured_key(&self, key: &str) -> Option<StructKey> {
        if !self.structured_key_is_sensitive(key) {
            return None;
        }
        Some(with_normalized(key, |n| StructKey {
            // `key`/`sig` are also ordinary words (`{"key":"Enter"}`, cache
            // keys): only redact values that look like a credential.
            looks_only: n == b"key" || n == b"sig",
            numeric: n == b"pin"
                || n == b"otp"
                || TEXT_STRONG.iter().any(|k| n.ends_with(k.as_bytes()))
                || self.extra_keys.iter().any(|k| n.ends_with(k.as_bytes())),
        }))
    }

    fn has_card(&self, digits: &str) -> bool {
        let mut spans = Vec::new();
        self.cards(digits, &mut spans);
        !spans.is_empty()
    }

    /// Scrub a serialized JSON document. `None` when nothing changed (the
    /// caller keeps its original bytes). Unparseable input is scrubbed as text.
    pub fn scrub_json_bytes(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        self.scrub_json_bytes_keeping(bytes, &[])
    }

    /// [`scrub_json_bytes`](Self::scrub_json_bytes), leaving the given TOP-LEVEL
    /// keys untouched (SDK-owned structure such as `crash.json`'s `signatures`,
    /// which the `signature` key stem would otherwise redact).
    pub fn scrub_json_bytes_keeping(&self, bytes: &[u8], keep: &[&str]) -> Option<Vec<u8>> {
        if !self.cfg.enabled {
            return None;
        }
        match serde_json::from_slice::<Value>(bytes) {
            Ok(mut v) => {
                let held: Vec<(String, Value)> = match &mut v {
                    Value::Object(m) => keep
                        .iter()
                        .filter_map(|k| m.remove(*k).map(|x| ((*k).to_string(), x)))
                        .collect(),
                    _ => Vec::new(),
                };
                let changed = self.scrub_value(&mut v);
                if let Value::Object(m) = &mut v {
                    for (k, x) in held {
                        m.insert(k, x);
                    }
                }
                if changed {
                    serde_json::to_vec(&v).ok()
                } else {
                    None
                }
            }
            Err(_) => match self.scrub_str(&String::from_utf8_lossy(bytes)) {
                Cow::Borrowed(_) => None,
                Cow::Owned(n) => Some(n.into_bytes()),
            },
        }
    }

    /// Scrub a capture-channel document of the form `{"...":[rec,rec,...]}`
    /// (the export envelope). The array is split into records without parsing,
    /// and a record is only parsed and rewritten when [`might_match`] says it
    /// could contain something; clean records are copied through byte for byte.
    /// `None` when nothing changed.
    ///
    /// [`might_match`]: Scrubber::might_match
    pub fn scrub_envelope(&self, doc: &[u8]) -> Option<Vec<u8>> {
        if !self.cfg.enabled {
            return None;
        }
        let (Some(open), true) = (doc.iter().position(|&c| c == b'['), doc.ends_with(b"]}")) else {
            return self.scrub_json_bytes(doc);
        };
        let body_start = open + 1;
        let body_end = doc.len() - 2;
        if body_start > body_end {
            return self.scrub_json_bytes(doc);
        }
        let mut changes: Vec<(usize, usize, Vec<u8>)> = Vec::new();
        let (mut depth, mut in_str, mut esc) = (0usize, false, false);
        let mut rec_start = body_start;
        let mut i = body_start;
        while i <= body_end {
            let at_end = i == body_end;
            let c = if at_end { b',' } else { doc[i] };
            if !at_end && in_str {
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else if !at_end && c == b'"' {
                in_str = true;
            } else if !at_end && (c == b'{' || c == b'[') {
                depth += 1;
            } else if !at_end && (c == b'}' || c == b']') {
                depth = depth.saturating_sub(1);
            } else if c == b',' && depth == 0 {
                let rec = &doc[rec_start..i];
                if !rec.is_empty() && self.might_match(rec) {
                    if let Some(clean) = self.scrub_json_bytes(rec) {
                        changes.push((rec_start, i, clean));
                    }
                }
                rec_start = i + 1;
            }
            i += 1;
        }
        if changes.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(doc.len());
        let mut at = 0;
        for (s, e, clean) in changes {
            out.extend_from_slice(&doc[at..s]);
            out.extend_from_slice(&clean);
            at = e;
        }
        out.extend_from_slice(&doc[at..]);
        Some(out)
    }

    /// A conservative byte-level pre-check: `false` means the scrubber would
    /// not change `raw` (a JSON text), so callers can skip parsing it. `true`
    /// just means "look closer". Always `true` when user-defined deny literals
    /// or sensitive keys are configured (they are arbitrary), and whenever the
    /// text contains a `\u` escape (the checked substrings could be spelled
    /// through it). Key stems are matched allowing `_`, `-`, space and `.`
    /// between letters (`p_w_d`, `P-W-D`), which is the separator set real
    /// keys use; the scrubber's normaliser would accept any punctuation, so
    /// the guarantee assumes those four separators.
    pub fn might_match(&self, raw: &[u8]) -> bool {
        if !self.cfg.deny.is_empty() || !self.extra_keys.is_empty() {
            return true;
        }
        const NEEDLES: &[&[u8]] = &[
            b"pass",
            b"pwd",
            b"secret",
            b"token",
            b"key",
            b"auth",
            b"session",
            b"cookie",
            b"credential",
            b"sig",
            b"csrf",
            b"xsrf",
            b"bearer",
            b"basic",
            b"jwt",
            b"pin",
            b"otp",
            b"sk-",
            b"sk_",
            b"rk_",
            b"gh",
            b"glpat",
            b"xox",
            b"akia",
            b"asia",
            b"aiza",
            b"sg.",
            b"npm_",
            b"shpat",
            b"hf_",
            b"whsec_",
            b"eyj",
            b"-----begin",
            b"users",
            b"home",
            b"://",
        ];
        let mut digits = 0usize;
        let mut gap = 0usize;
        for (i, &c) in raw.iter().enumerate() {
            if c == b'@' || c == b'=' || (c == b'\\' && raw.get(i + 1) == Some(&b'u')) {
                return true;
            }
            if c.is_ascii_digit() {
                digits += 1;
                gap = 0;
                if digits >= 13 {
                    return true;
                }
            } else {
                gap += 1;
                // A card number may be separated by single spaces/dashes.
                if gap > 1 || !(c == b' ' || c == b'-') {
                    digits = 0;
                }
            }
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphabetic() || matches!(lc, b'-' | b':') {
                let rest = &raw[i..];
                for n in NEEDLES {
                    if n[0] == lc && fuzzy_starts_with(rest, n) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// A scrubbed copy of a report's metadata: summary, description, labels and
    /// attributes. The reporter's own `email` identity, the dedup `signatures`
    /// and the attachments are deliberately left alone.
    pub fn scrub_meta(&self, meta: &crate::reporting::ReportMeta) -> crate::reporting::ReportMeta {
        let mut out = meta.clone();
        if !self.cfg.enabled {
            return out;
        }
        for s in [&mut out.summary, &mut out.description]
            .into_iter()
            .flatten()
        {
            if let Cow::Owned(n) = self.scrub_str(s) {
                *s = n;
            }
        }
        for l in &mut out.labels {
            if let Cow::Owned(n) = self.scrub_str(l) {
                *l = n;
            }
        }
        let mut attrs = Value::Object(std::mem::take(&mut out.attrs));
        self.scrub_value(&mut attrs);
        if let Value::Object(m) = attrs {
            out.attrs = m;
        }
        out
    }

    // ---- helpers ----------------------------------------------------------

    fn allowed(&self, span: &str) -> bool {
        self.cfg
            .allow
            .iter()
            .any(|a| a == span || (a.starts_with('@') && span.ends_with(a.as_str())))
    }

    fn structured_key_is_sensitive(&self, key: &str) -> bool {
        is_sensitive_key(key)
            || (!self.extra_keys.is_empty()
                && with_normalized(key, |n| {
                    self.extra_keys.iter().any(|k| contains(n, k.as_bytes()))
                }))
    }

    fn text_key_class(&self, key: &str) -> KeyClass {
        with_normalized(key, |n| {
            if n.ends_with(b"cookie") {
                KeyClass::Cookie
            } else if self.extra_keys.iter().any(|k| n.ends_with(k.as_bytes())) {
                KeyClass::Explicit
            } else if TEXT_STRONG.iter().any(|k| n.ends_with(k.as_bytes())) {
                KeyClass::Strong
            } else if n == b"pin" || n == b"otp" {
                KeyClass::Pin
            } else if EXACT.iter().any(|k| n == k.as_bytes())
                || TEXT_AMBIGUOUS_SUFFIX
                    .iter()
                    .any(|k| n.ends_with(k.as_bytes()))
            {
                KeyClass::Ambiguous
            } else {
                KeyClass::None
            }
        })
    }

    // ---- detectors --------------------------------------------------------

    fn emails(&self, s: &str, out: &mut Vec<Span>) {
        let b = s.as_bytes();
        let mut from = 0;
        while let Some(rel) = s[from..].find('@') {
            let at = from + rel;
            from = at + 1;
            let mut st = at;
            while st > 0 && is_email_local(b[st - 1]) {
                st -= 1;
            }
            let mut en = at + 1;
            while en < b.len() && is_email_domain(b[en]) {
                en += 1;
            }
            if st == at || en <= at + 1 {
                continue;
            }
            // The domain ends at its alphabetic top-level label, so trailing
            // `-token` junk glued on by the domain charset is not swallowed.
            let dom = &s[at + 1..en];
            let Some(dot) = dom.rfind('.') else { continue };
            if dot == 0 {
                continue;
            }
            let tld_len = dom[dot + 1..]
                .bytes()
                .take_while(u8::is_ascii_alphabetic)
                .count();
            if tld_len < 2 {
                continue;
            }
            // `icon@2x.png` is an asset name, not an address.
            let tld = &dom[dot + 1..dot + 1 + tld_len];
            if FILE_EXTENSIONS.iter().any(|x| tld.eq_ignore_ascii_case(x)) {
                continue;
            }
            let en = at + 1 + dot + 1 + tld_len;
            if self.allowed(&s[st..en]) {
                continue;
            }
            out.push(Span {
                start: st,
                end: en,
                repl: EMAIL,
            });
        }
    }

    /// One pass over word starts (a position whose predecessor is not a word
    /// character), testing each known token shape there. Runs of token
    /// characters are scanned once and cached, so adversarial input such as
    /// `sk-sk-sk-...` costs O(n), not O(n^2).
    fn tokens(&self, s: &str, out: &mut Vec<Span>) {
        let b = s.as_bytes();
        let mut run = Run::default();
        let mut i = 0;
        while i < b.len() {
            if !b[i].is_ascii_alphanumeric()
                || (i > 0 && is_word(b[i - 1]))
                || !TOKEN_FIRST.contains(&b[i])
            {
                i += 1;
                continue;
            }
            let end = prefixed_token(b, i, &mut run)
                .or_else(|| jwt(b, i, &mut run))
                .or_else(|| credential_after_scheme(b, i));
            match end {
                Some((ts, te)) if !self.allowed(&s[ts..te]) => {
                    out.push(Span {
                        start: ts,
                        end: te,
                        repl: TOKEN,
                    });
                    i = te.max(i + 1);
                }
                _ => i += 1,
            }
        }
    }

    /// `-----BEGIN ... PRIVATE KEY-----` blocks, through the matching END line
    /// (or the end of the string when it was truncated).
    fn pem_blocks(&self, s: &str, out: &mut Vec<Span>) {
        const BEGIN: &str = "-----BEGIN ";
        let mut from = 0;
        while let Some(rel) = s[from..].find(BEGIN) {
            let st = from + rel;
            let label_start = st + BEGIN.len();
            let Some(lrel) = s[label_start..].find("-----") else {
                break;
            };
            let label = &s[label_start..label_start + lrel];
            let body = label_start + lrel + 5;
            from = body;
            if !label.ends_with("PRIVATE KEY") {
                continue;
            }
            let end = match s[body..].find("-----END ") {
                Some(e) => {
                    let after = body + e + 9;
                    s[after..].find("-----").map_or(s.len(), |x| after + x + 5)
                }
                None => s.len(),
            };
            out.push(Span {
                start: st,
                end,
                repl: TOKEN,
            });
            from = end;
        }
    }

    /// The password in `scheme://user:password@host` (the user name may be
    /// empty: `redis://:pw@host`), regardless of what the host looks like.
    fn url_userinfo(&self, s: &str, out: &mut Vec<Span>) {
        let b = s.as_bytes();
        let mut from = 0;
        while let Some(rel) = s[from..].find("://") {
            let p = from + rel + 3;
            from = p;
            let mut j = p;
            let mut colon = None;
            while j < b.len()
                && !matches!(
                    b[j],
                    b'@' | b'/' | b'?' | b'#' | b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\''
                )
            {
                if b[j] == b':' && colon.is_none() {
                    colon = Some(j);
                }
                j += 1;
            }
            if let (Some(c), true) = (colon, j < b.len() && b[j] == b'@') {
                if c + 1 < j && !self.allowed(&s[c + 1..j]) {
                    out.push(Span {
                        start: c + 1,
                        end: j,
                        repl: REDACTED,
                    });
                }
            }
        }
    }

    fn keyed(&self, s: &str, out: &mut Vec<Span>) {
        let b = s.as_bytes();
        // Separators before `covered` sit inside an already-claimed value.
        let mut covered = 0usize;
        // The unquoted value run containing the last scanned start: every start
        // inside it ends at the same place, so it is never rescanned.
        let mut vrun = (0usize, 0usize);
        for i in 0..b.len() {
            let sep = b[i];
            if (sep != b'=' && sep != b':') || i < covered {
                continue;
            }
            // `::` paths and `scheme://` are not key/value separators.
            if sep == b':'
                && ((i + 1 < b.len() && matches!(b[i + 1], b':' | b'/'))
                    || (i > 0 && b[i - 1] == b':'))
            {
                continue;
            }
            // Key: [ident] [closing (escaped) quote] [ws] before the separator.
            let mut j = i;
            while j > 0 && matches!(b[j - 1], b' ' | b'\t') {
                j -= 1;
            }
            let mut quoted_key = false;
            if j > 0 && matches!(b[j - 1], b'"' | b'\'') {
                j -= 1;
                quoted_key = true;
                if j > 0 && b[j - 1] == b'\\' {
                    j -= 1;
                }
            }
            let key_end = j;
            while j > 0 && (b[j - 1].is_ascii_alphanumeric() || matches!(b[j - 1], b'_' | b'-')) {
                j -= 1;
            }
            if j == key_end {
                continue;
            }
            let class = self.text_key_class(&s[j..key_end]);
            if class == KeyClass::None {
                continue;
            }
            // `my_app::token: refreshed` - a path segment, not a key.
            if j >= 2 && b[j - 1] == b':' && b[j - 2] == b':' {
                continue;
            }
            // Value.
            let mut k = i + 1;
            while k < b.len() && matches!(b[k], b' ' | b'\t') {
                k += 1;
            }
            let mut escaped = false;
            while k + 1 < b.len() && b[k] == b'\\' && matches!(b[k + 1], b'"' | b'\'') {
                escaped = true;
                k += 1;
            }
            let quote = if k < b.len() && matches!(b[k], b'"' | b'\'') {
                k += 1;
                Some(b[k - 1])
            } else {
                None
            };
            let vs = k;
            let mut ve = match quote {
                Some(q) if escaped => {
                    let e = b[vs..]
                        .iter()
                        .position(|&c| c == q)
                        .map_or(b.len(), |p| vs + p);
                    if e > vs && b[e - 1] == b'\\' {
                        e - 1
                    } else {
                        e
                    }
                }
                Some(q) => find_close(b, vs, q),
                None if class == KeyClass::Cookie => b[vs..]
                    .iter()
                    .position(|&c| c == b'\r' || c == b'\n')
                    .map_or(b.len(), |p| vs + p),
                None if vs < b.len() && matches!(b[vs], b'{' | b'[') => bracketed_end(b, vs),
                None => {
                    if vrun.0 <= vs && vs < vrun.1 {
                        vrun.1
                    } else {
                        let mut e = vs;
                        while e < b.len()
                            && !matches!(
                                b[e],
                                b' ' | b'\t'
                                    | b'\r'
                                    | b'\n'
                                    | b'&'
                                    | b','
                                    | b';'
                                    | b')'
                                    | b'}'
                                    | b'"'
                                    | b'\''
                                    | b'<'
                                    | b'>'
                            )
                        {
                            e += 1;
                        }
                        vrun = (vs, e);
                        e
                    }
                }
            };
            if ve == vs {
                continue;
            }
            let value = &s[vs..ve];
            if MARKERS.contains(&value) {
                continue;
            }
            // `Authorization: Bearer abc` - take the credential, not the scheme.
            if quote.is_none()
                && ["bearer", "basic", "digest", "token"]
                    .iter()
                    .any(|w| value.eq_ignore_ascii_case(w))
                && ve < b.len()
                && b[ve] == b' '
            {
                let mut e = ve;
                while e < b.len() && b[e] == b' ' {
                    e += 1;
                }
                while e < b.len()
                    && !matches!(b[e], b' ' | b'\t' | b'\r' | b'\n' | b',' | b';' | b'"')
                {
                    e += 1;
                }
                ve = e;
            }
            // Bounded looks (the first 64 bytes), never O(len) per attempt.
            let head = &b[vs..ve.min(vs + 64)];
            if class == KeyClass::Ambiguous {
                // Ordinary words ("auth: failed", "session: started") are not
                // secrets: need `=` or a quoted key and a secret-looking value
                // (8+ chars with a digit; `user:42:profile` cache keys excluded).
                let secretish =
                    ve - vs >= 8 && head.iter().any(u8::is_ascii_digit) && !head.contains(&b':');
                if !(sep == b'=' || quoted_key) || !secretish {
                    continue;
                }
            } else if class == KeyClass::Pin {
                // 4-10 digits; `pin=home` or "otp: sent" are prose.
                let v = &b[vs..ve];
                let codeish = (4..=10).contains(&v.len()) && v.iter().all(u8::is_ascii_digit);
                if !(sep == b'=' || quoted_key) || !codeish {
                    continue;
                }
            } else if class == KeyClass::Strong
                && sep == b':'
                && !quoted_key
                && quote.is_none()
                && with_normalized(&s[j..key_end], |n| n == b"token")
            {
                // Compiler-error wording, not a credential: "invalid token:
                // expected ident". ONLY the bare word `token` gets this leniency:
                // `password: secret` / `invalid password: letmein` are redacted
                // however plain the value looks. A real `token: abc123` still is.
                let loose = ve - vs >= 6 && !head.iter().all(u8::is_ascii_lowercase);
                if !loose {
                    continue;
                }
            }
            if self.allowed(&s[vs..ve]) {
                continue;
            }
            covered = ve;
            out.push(Span {
                start: vs,
                end: ve,
                repl: REDACTED,
            });
        }
    }

    fn cards(&self, s: &str, out: &mut Vec<Span>) {
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if !b[i].is_ascii_digit() || (i > 0 && b[i - 1].is_ascii_alphanumeric()) {
                i += 1;
                continue;
            }
            // Digit groups separated by single ' ' or '-'.
            let mut starts = [0usize; 5];
            let mut lens = [0usize; 5];
            let mut g = 0;
            let mut p = i;
            loop {
                let mut e = p;
                while e < b.len() && b[e].is_ascii_digit() {
                    e += 1;
                }
                starts[g] = p;
                lens[g] = e - p;
                g += 1;
                p = e;
                if g == 5 {
                    break;
                }
                if p + 1 < b.len() && matches!(b[p], b' ' | b'-') && b[p + 1].is_ascii_digit() {
                    p += 1;
                } else {
                    break;
                }
            }
            let first_end = starts[0] + lens[0];
            let mut hit = None;
            for k in (1..=g).rev() {
                if !card_layout(&lens[..k]) {
                    continue;
                }
                let end = starts[k - 1] + lens[k - 1];
                if end < b.len() && b[end].is_ascii_alphanumeric() {
                    continue;
                }
                let mut digits = [0u8; 19];
                let mut n = 0;
                for gi in 0..k {
                    for &d in &b[starts[gi]..starts[gi] + lens[gi]] {
                        digits[n] = d - b'0';
                        n += 1;
                    }
                }
                if plausible_issuer(&digits[..n])
                    && luhn(&digits[..n])
                    && !in_uuid_shape(b, i, end)
                    && !self.allowed(&s[i..end])
                {
                    hit = Some(end);
                    break;
                }
            }
            match hit {
                Some(end) => {
                    out.push(Span {
                        start: i,
                        end,
                        repl: CARD,
                    });
                    i = end;
                }
                None => i = first_end.max(i + 1),
            }
        }
    }

    /// Home-directory user names. Only real filesystem shapes: `/Users/<name>/`
    /// (macOS, capital U), `/home/<name>/` (Linux, lowercase) and
    /// `X:\Users\<name>\`, each starting a token and followed by a deeper
    /// path, so URL routes (`/users/123/orders`, `/home/feed`, `/users/{id}`)
    /// are left alone.
    fn paths(&self, s: &str, out: &mut Vec<Span>) {
        let b = s.as_bytes();
        for i in 0..b.len() {
            if b[i] != b'/' && b[i] != b'\\' {
                continue;
            }
            let rest = &b[i + 1..];
            // `X:\Users\` / `X:/Users/`: a standalone drive letter before.
            let drive = i >= 2
                && b[i - 1] == b':'
                && b[i - 2].is_ascii_alphabetic()
                && (i == 2 || !is_word(b[i - 3]));
            let (root, windows) =
                if drive && rest.len() >= 5 && rest[..5].eq_ignore_ascii_case(b"users") {
                    (5, true)
                } else if b[i] == b'/' {
                    // Starts a token: not glued to a word or path segment.
                    if i > 0
                        && (b[i - 1].is_ascii_alphanumeric()
                            || matches!(b[i - 1], b'/' | b'.' | b'_' | b'-' | b'~' | b'%'))
                        && !(b[i - 1] == b'/' && b[..i].ends_with(b"file://"))
                    {
                        continue;
                    }
                    if rest.starts_with(b"Users") {
                        (5, false)
                    } else if rest.starts_with(b"home") {
                        (4, false)
                    } else {
                        continue;
                    }
                } else {
                    continue;
                };
            if rest.len() <= root || !matches!(rest[root], b'/' | b'\\') {
                continue;
            }
            let ns = i + 1 + root + 1;
            let mut ne = ns;
            while ne < b.len()
                && !matches!(
                    b[ne],
                    b'/' | b'\\' | b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\'' | b')' | b':' | b','
                )
            {
                ne += 1;
            }
            let name = &s[ns..ne];
            let deeper = ne < b.len() && matches!(b[ne], b'/' | b'\\');
            let template = name.starts_with(['{', '<', '*', '$', '[']) || name.starts_with(':');
            let numeric = name.bytes().all(|c| c.is_ascii_digit());
            if name.is_empty()
                || name == USER
                || template
                || numeric
                // `/home/feed` is a route, `/home/bob/x` a path; a bare
                // `/Users/bob` or `X:\Users\bob` still names the user.
                || (!deeper && !windows && root == 4)
                // Shared system folders, not a person.
                || matches!(name, "Shared" | "Public" | "Default")
                || self.allowed(name)
            {
                continue;
            }
            out.push(Span {
                start: ns,
                end: ne,
                repl: USER,
            });
        }
    }
}

/// Position of the closing `q` of a quoted value starting at `vs`, skipping
/// backslash-escaped quotes (`"a\"b"`); the end of the input if unterminated.
fn find_close(b: &[u8], vs: usize, q: u8) -> usize {
    for i in vs..b.len() {
        if b[i] == q {
            let mut k = 0;
            while i > vs + k && b[i - 1 - k] == b'\\' {
                k += 1;
            }
            if k % 2 == 0 {
                return i;
            }
        }
    }
    b.len()
}

/// End of a `{...}` / `[...]` value starting at `vs` (matching close, or the
/// end of the string when unbalanced).
fn bracketed_end(b: &[u8], vs: usize) -> usize {
    let mut depth = 0usize;
    for (off, &c) in b[vs..].iter().enumerate() {
        match c {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return vs + off + 1;
                }
            }
            _ => {}
        }
    }
    b.len()
}

/// A cached run of token characters, so repeated starts inside one run share a
/// single scan.
#[derive(Default)]
struct Run {
    start: usize,
    end: usize,
    last_digit: Option<usize>,
    last_upper: Option<usize>,
    last_lower: Option<usize>,
}

impl Run {
    /// End of the token-character run beginning at `t`.
    fn end_from(&mut self, b: &[u8], t: usize) -> usize {
        if !(self.start <= t && t < self.end) {
            let mut e = t;
            let (mut last_digit, mut last_upper, mut last_lower) = (None, None, None);
            while e < b.len() && is_token_char(b[e]) {
                if b[e].is_ascii_digit() {
                    last_digit = Some(e);
                } else if b[e].is_ascii_uppercase() {
                    last_upper = Some(e);
                } else if b[e].is_ascii_lowercase() {
                    last_lower = Some(e);
                }
                e += 1;
            }
            *self = Run {
                start: t,
                end: e,
                last_digit,
                last_upper,
                last_lower,
            };
        }
        self.end
    }

    /// Whether the run, from `t` on, contains a digit (valid after `end_from(t)`).
    fn digit_from(&self, t: usize) -> bool {
        self.last_digit.is_some_and(|d| d >= t)
    }

    /// Whether the run, from `t` on, has both upper- and lower-case letters.
    fn mixed_from(&self, t: usize) -> bool {
        self.last_upper.is_some_and(|d| d >= t) && self.last_lower.is_some_and(|d| d >= t)
    }
}

/// `(start, end)` of a prefixed token starting at `i`.
fn prefixed_token(b: &[u8], i: usize, run: &mut Run) -> Option<(usize, usize)> {
    let rest = &b[i..];
    for &(prefix, min_tail, need) in TOKEN_PREFIXES {
        // Compare the first byte inline; most words stop here.
        if rest[0] != prefix.as_bytes()[0] || !rest.starts_with(prefix.as_bytes()) {
            continue;
        }
        let t = i + prefix.len();
        let e = run.end_from(b, t);
        let ok = e - t >= min_tail
            && match need {
                Need::None => true,
                Need::DigitAndMixed => run.digit_from(t) && run.mixed_from(t),
                Need::DigitOrMixed => run.digit_from(t) || run.mixed_from(t),
                Need::Upper16 => {
                    e - t == 16
                        && b[t..e]
                            .iter()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                }
            };
        if ok {
            return Some((i, e));
        }
    }
    None
}

/// `eyJ<seg>.<seg>.<seg>` at `i`.
fn jwt(b: &[u8], i: usize, run: &mut Run) -> Option<(usize, usize)> {
    if !b[i..].starts_with(b"eyJ") {
        return None;
    }
    let mut en = i;
    for seg in 0..3 {
        let e = run.end_from(b, en);
        if e == en {
            return None;
        }
        en = e;
        if seg < 2 {
            if en >= b.len() || b[en] != b'.' {
                return None;
            }
            en += 1;
        }
    }
    Some((i, en))
}

/// `Bearer[:] <credential>` / `Basic <base64>` starting at word start `i`;
/// covers just the credential.
fn credential_after_scheme(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = &b[i..];
    let (word, strict) = if rest.len() > 6 && rest[..6].eq_ignore_ascii_case(b"bearer") {
        (6, false)
    } else if rest.len() > 5 && rest[..5].eq_ignore_ascii_case(b"basic") {
        (5, true)
    } else {
        return None;
    };
    let mut j = i + word;
    if j < b.len() && b[j] == b':' {
        j += 1;
    }
    let sp = j;
    while j < b.len() && b[j] == b' ' {
        j += 1;
    }
    if j == sp {
        return None;
    }
    let ts = j;
    // Base64/URL-safe credential chars: token chars plus `. ~ + / =`.
    // `mixed`: a digit or one of `. ~ + / =` (a plain `-`/`_` joined word like
    // "token-based" is prose); `cased`: both letter cases.
    let (mut mixed, mut upper, mut lower) = (false, false, false);
    while j < b.len() && (is_token_char(b[j]) || matches!(b[j], b'.' | b'~' | b'+' | b'/' | b'=')) {
        mixed |= b[j].is_ascii_digit() || matches!(b[j], b'.' | b'~' | b'+' | b'/' | b'=');
        upper |= b[j].is_ascii_uppercase();
        lower |= b[j].is_ascii_lowercase();
        j += 1;
    }
    let len = j - ts;
    // "Bearer tokens are ..." is prose; a credential has a digit/symbol, mixed
    // case, or is long.
    let ok = if strict {
        len >= 8 && mixed
    } else {
        len >= 6 && (mixed || (upper && lower) || len >= 20)
    };
    ok.then_some((ts, j))
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn is_email_local(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_email_domain(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-')
}

/// Real card layouts: contiguous 13-19 digits, or the printed groupings
/// 4-4-4-4, 4-4-4-4-3, 4-6-4 and 4-6-5.
fn card_layout(lens: &[usize]) -> bool {
    match lens {
        [n] => (13..=19).contains(n),
        [4, 4, 4, 4] | [4, 4, 4, 4, 3] | [4, 6, 4] | [4, 6, 5] => true,
        _ => false,
    }
}

/// Whether `[start,end)` sits inside a canonical 8-4-4-4-12 hex UUID.
fn in_uuid_shape(b: &[u8], start: usize, end: usize) -> bool {
    let is_hexish = |c: u8| c.is_ascii_hexdigit() || c == b'-';
    let mut ts = start;
    while ts > 0 && is_hexish(b[ts - 1]) {
        ts -= 1;
    }
    let mut te = end;
    while te < b.len() && is_hexish(b[te]) {
        te += 1;
    }
    // A UUID may be a window inside a longer hex/dash chain; test every
    // 36-byte window that covers the candidate.
    let lo = ts.max(end.saturating_sub(36));
    let hi = start.min(te.saturating_sub(36));
    (lo..=hi).any(|w| w + 36 <= te && is_uuid(&b[w..w + 36]))
}

fn is_uuid(w: &[u8]) -> bool {
    w.iter().enumerate().all(|(i, &c)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            c == b'-'
        } else {
            c.is_ascii_hexdigit()
        }
    })
}

/// Card numbers start with 3 (Amex/Diners/JCB), 4 (Visa), 5 (Mastercard),
/// 6 (Discover/UnionPay) or a 2221-2720 Mastercard BIN. Notably NOT `1`, which
/// is what epoch-millisecond/microsecond timestamps start with.
fn plausible_issuer(d: &[u8]) -> bool {
    match d[0] {
        3..=6 => true,
        2 => {
            let bin = d[..4].iter().fold(0u32, |a, &x| a * 10 + x as u32);
            (2221..=2720).contains(&bin)
        }
        _ => false,
    }
}

fn luhn(d: &[u8]) -> bool {
    let mut sum = 0u32;
    for (i, &x) in d.iter().rev().enumerate() {
        let mut v = x as u32;
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum % 10 < 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> String {
        Scrubber::default().scrub_str(x).into_owned()
    }

    /// Join halves at runtime so no whole secret-shaped literal appears in the
    /// source (GitHub push protection rejects those).
    fn j(a: &str, b: &str) -> String {
        format!("{a}{b}")
    }

    #[test]
    fn clean_strings_are_borrowed() {
        let sc = Scrubber::default();
        for t in [
            "connection refused",
            "took 1712345678901 ms",
            "token_count: 5000",
            "see /opt/home/x and /usr/lib",
            "a@b",
            "version 1.2.3",
            "auth: failed",
            "session: started",
            "GET /users/123/orders",
            "GET /home/feed",
            "/users/{id}/x",
            "my_app::auth::login::verify",
            "cancellation_token::CancellationToken",
        ] {
            assert!(matches!(sc.scrub_str(t), Cow::Borrowed(_)), "{t}");
        }
    }

    #[test]
    fn emails() {
        assert_eq!(s("mail bob.smith+x@Example.co.uk now"), "mail [email] now");
        assert_eq!(s("(a@b.io)"), "([email])");
    }

    #[test]
    fn tokens() {
        assert_eq!(s(&j("k=sk_", "live_abcdefgh12345678")), "k=[token]");
        assert_eq!(
            s(&j("gh", "p_abcdefghijklmnopqrstuvwxyz0123456789")),
            "[token]"
        );
        assert_eq!(s(&j("hf", "_abcdefghijklmnopqrstuvwxyzABCDEF")), "[token]");
        assert_eq!(s(&j("whsec", "_abcdefghijklmnopqrstuvwx")), "[token]");
        assert_eq!(
            s(&format!("id {}{} x", "AKI", "AIOSFODNN7EXAMPLE")),
            "id [token] x"
        );
        let jwt = format!(
            "{}.{}.{}",
            j("ey", "JhbGciOiJIUzI1NiJ9"),
            j("ey", "JzdWIiOiIxMjM0NTY3ODkwIn0"),
            "dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        );
        assert_eq!(s(&format!("t {jwt}!")), "t [token]!");
        assert_eq!(s("Bearer abcdef0123456789"), "Bearer [token]");
        assert_eq!(s("Bearer: abc123"), "Bearer: [token]");
        assert_eq!(s("Bearer abc123"), "Bearer [token]");
    }

    #[test]
    fn token_shapes_are_tight() {
        // Prose and identifiers that merely start like a token stay readable.
        for t in [
            "bearer tokens are rotated daily",
            "Bearer token-based auth",
            "sk-cluster-node-01-primary-east",
            "sk-learn-something-long-enough",
            "AKIA is the prefix",
            concat!("AKI", "AIOSFODNN7EXAMPLEXX"),
            "AKIAiosfodnn7example1234",
            "hf_abcdefghijklmnopqrstuvwxyz",
        ] {
            assert_eq!(s(t), t, "{t}");
        }
        // Real shapes still match.
        assert_eq!(s(&format!("{}{}", "AKI", "AIOSFODNN7EXAMPLE")), "[token]");
        assert_eq!(s(&j("sk-", "ProjAbCdEfGhIjKlMnOp0123456789xyz")), "[token]");
        assert_eq!(s(&j("hf_", "abcdEFGHijklMNOPqrstUVWX")), "[token]");
    }

    #[test]
    fn pem_private_keys_and_url_userinfo() {
        let begin = j("-----BEGIN RSA ", "PRIVATE KEY-----");
        let end = j("-----END RSA ", "PRIVATE KEY-----");
        let pem = format!("x {begin}\nMIIEow\nIBAAKCAQEA\n{end} y");
        assert_eq!(s(&pem), "x [token] y");
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        assert_eq!(s(cert), cert);
        assert_eq!(
            s("redis://:s3cr3t@cache:6379/0"),
            "redis://:[redacted]@cache:6379/0"
        );
        // A dotted host makes `pw@host.tld` look like an email too; either
        // marker is fine, the password must be gone.
        let out = s("postgres://app:pw@db.internal.example.com/x");
        assert!(!out.contains("pw"), "{out}");
        assert_eq!(s("https://example.com/a:b@c"), "https://example.com/a:b@c");
    }

    #[test]
    fn keyed() {
        assert_eq!(s("password=hunter2&x=1"), "password=[redacted]&x=1");
        assert_eq!(
            s(r#"{"api_key": "abc123"}"#),
            r#"{"api_key": "[redacted]"}"#
        );
        assert_eq!(
            s("Authorization: Bearer abc.def"),
            "Authorization: [redacted]"
        );
        assert_eq!(s("access_token=xyz"), "access_token=[redacted]");
        assert_eq!(s("password=[redacted]"), "password=[redacted]");
        assert_eq!(s("password=[a, b, c] tail"), "password=[redacted] tail");
        assert_eq!(s(r#"password={"a":"b"} tail"#), "password=[redacted] tail");
        assert_eq!(s("AccountKey=abc123def456=="), "AccountKey=[redacted]");
        // Escaped quotes in raw JSON text.
        assert_eq!(
            s(r#"{\"password\": \"hunter2\", \"n\": 1}"#),
            r#"{\"password\": \"[redacted]\", \"n\": 1}"#
        );
        // Ambiguous names need `=`/quoted key and a secret-looking value.
        assert_eq!(s("session=abcd1234efgh"), "session=[redacted]");
        assert_eq!(s("key=short"), "key=short");
        // OAuth 1.0a verifier/PIN and short OTP/PIN codes in free text.
        assert_eq!(
            s("oauth_verifier=abcXYZ&x=1"),
            "oauth_verifier=[redacted]&x=1"
        );
        assert_eq!(s("oauth_pin=1234"), "oauth_pin=[redacted]");
        assert_eq!(s("code_verifier=abcXYZ123"), "code_verifier=[redacted]");
        assert_eq!(s("pin=1234 otp=654321"), "pin=[redacted] otp=[redacted]");
        assert_eq!(s(r#"{"otp": "654321"}"#), r#"{"otp": "[redacted]"}"#);
        assert_eq!(
            s("pin=home otp: sent spinner=12"),
            "pin=home otp: sent spinner=12"
        );
        assert_eq!(s("auth: failed for user"), "auth: failed for user");
    }

    #[test]
    fn double_colon_paths_survive() {
        for t in [
            "my_app::auth::login::verify",
            "cancellation_token::CancellationToken::new",
            "app::secret::Vault::open",
            "std::sync::password::x",
            "<T as app::token::Token>::parse",
        ] {
            assert_eq!(s(t), t);
        }
    }

    #[test]
    fn cards_need_real_layout_luhn_and_issuer() {
        assert_eq!(s("card 4111 1111 1111 1111."), "card [card].");
        assert_eq!(s("4111-1111-1111-1111"), "[card]");
        assert_eq!(s("378282246310005"), "[card]");
        assert_eq!(s("3782 822463 10005"), "[card]");
        assert_eq!(s("4111111111111112"), "4111111111111112"); // bad Luhn
        assert_eq!(s("1712345678901"), "1712345678901"); // epoch ms
        assert_eq!(s("4111 1111 11 1111 11"), "4111 1111 11 1111 11"); // not a layout
                                                                       // UUID / Mach-O code_id shapes never match.
        for u in [
            "41111111-1111-1111-1111-111111111111",
            "code_id 4A111111-1111-1111-1111-111111111111",
            "55555555-5555-5555-5555-555555555555",
        ] {
            assert_eq!(s(u), u, "{u}");
        }
    }

    #[test]
    fn paths() {
        assert_eq!(s("/Users/alice/proj/x.rs"), "/Users/[user]/proj/x.rs");
        assert_eq!(
            s("at /home/bob/.cargo/a.rs:3"),
            "at /home/[user]/.cargo/a.rs:3"
        );
        assert_eq!(s(r"C:\Users\Carol\app\m.rs"), r"C:\Users\[user]\app\m.rs");
        assert_eq!(s("file:///Users/dan/x"), "file:///Users/[user]/x");
        assert_eq!(s("/opt/home/dan/x"), "/opt/home/dan/x");
        assert_eq!(s("/api/Users/dan/x"), "/api/Users/dan/x");
        // Routes are not paths.
        for r in [
            "/users/123/orders",
            "/home/feed",
            "/users/{id}",
            "/home/{id}/x",
            "/Users/42/x",
        ] {
            assert_eq!(s(r), r, "{r}");
        }
    }

    #[test]
    fn overlapping_spans_merge_without_leaking_the_tail() {
        let g = j("gh", "p_abcdefghijklmnopqrstuvwxyz0123456789");
        let out = s(&format!("a@b.com-{g} end"));
        assert!(!out.contains("ghp_") && !out.contains("abcdefgh"), "{out}");
        let out = s(&format!("redis://u:{g}@h.example.com/x"));
        assert!(!out.contains("abcdefghijkl"), "{out}");
    }

    #[test]
    fn allow_and_deny_and_extra_keys() {
        let sc = Scrubber::new(
            ScrubConfig::default()
                .allow("@example.com")
                .allow("ops")
                .deny("acme-internal")
                .sensitive_key("x-customer"),
        );
        assert_eq!(
            sc.scrub_str("a@example.com b@other.org").as_ref(),
            "a@example.com [email]"
        );
        assert_eq!(sc.scrub_str("/home/ops/x").as_ref(), "/home/ops/x");
        assert_eq!(
            sc.scrub_str("host acme-internal.lan").as_ref(),
            "host [redacted].lan"
        );
        assert_eq!(
            sc.scrub_str("X-Customer: 42").as_ref(),
            "X-Customer: [redacted]"
        );
    }

    #[test]
    fn off_switch_and_detector_toggles() {
        let off = Scrubber::new(ScrubConfig::disabled());
        assert_eq!(off.scrub_str("a@b.com").as_ref(), "a@b.com");
        let no_email = Scrubber::new(ScrubConfig::default().emails(false));
        assert_eq!(no_email.scrub_str("a@b.com").as_ref(), "a@b.com");
    }

    #[test]
    fn json_values_and_keys() {
        let sc = Scrubber::default();
        let mut v = serde_json::json!({
            "message": "user a@b.com failed",
            "headers": {"Authorization": "Bearer zzz", "Cookie": "s=1", "token_count": 5},
            "list": ["/Users/eve/x", 7],
            "password": null,
            "api_key": ["a", {"b": "c", "n": 3}, true],
            "session_ttl": 30,
            "signatures": ["a1b2c3"]
        });
        let keep = v.clone();
        assert!(sc.scrub_value(&mut v));
        assert_eq!(v["message"], "user [email] failed");
        assert_eq!(v["headers"]["Authorization"], "[redacted]");
        assert_eq!(v["headers"]["Cookie"], "[redacted]");
        assert_eq!(
            v["headers"]["token_count"], 5,
            "numbers are never rewritten"
        );
        assert_eq!(v["list"][0], "/Users/[user]/x");
        assert_eq!(v["api_key"][0], "[redacted]");
        assert_eq!(v["api_key"][1]["b"], "[redacted]");
        // Numbers under an unambiguous secret key are redacted too.
        assert_eq!(v["api_key"][1]["n"], "[redacted]");
        assert_eq!(v["api_key"][2], true);
        assert_eq!(v["session_ttl"], 30);
        // `signatures` is only exempt where the SDK says so (crash.json).
        assert_eq!(v["signatures"][0], "[redacted]");
        let out = sc
            .scrub_json_bytes_keeping(&serde_json::to_vec(&keep).unwrap(), &["signatures"])
            .unwrap();
        let v2: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v2["signatures"][0], "a1b2c3");
        assert_eq!(v2["message"], "user [email] failed");
        assert!(sc.scrub_json_bytes(br#"{"a":"fine"}"#).is_none());
    }

    #[test]
    fn crash_json_structure_survives() {
        let sc = Scrubber::default();
        let crash = serde_json::json!({
            "exception": {"name": "cancellation_token::CancellationToken", "message": "boom"},
            "signatures": ["0123456789abcdef0123456789abcdef01234567"],
            "threads": [{"frames": [
                {"trace": "my_app::auth::login::verify (src/auth.rs:42)", "module": "my_app"},
                {"trace": "tokio::runtime::task::harness::Harness<T,S>::poll::h1406d87bf3ffb336"}
            ]}],
            "modules": [{
                "filename": "/usr/lib/libsystem_kernel.dylib",
                "code_id": "A1B2C3D4-1234-4567-89AB-0123456789AB",
                "debug_id": "41111111-1111-1111-1111-111111111111"
            }]
        });
        assert!(sc
            .scrub_json_bytes_keeping(&serde_json::to_vec(&crash).unwrap(), &["signatures"])
            .is_none());
        assert!(sc
            .scrub_envelope(br#"{"version":2,"events":[{"t":"app::auth::x"}]}"#)
            .is_none());
    }

    #[test]
    fn envelope_scrubs_only_matching_records() {
        let sc = Scrubber::default();
        let doc = br#"{"version":2,"events":[{"m":"fine, with a comma"},{"m":"mail a@b.com [x]"},{"m":"ok"}]}"#;
        let out = sc.scrub_envelope(doc).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["events"][0]["m"], "fine, with a comma");
        assert_eq!(v["events"][1]["m"], "mail [email] [x]");
        assert_eq!(v["events"][2]["m"], "ok");
        let empty = br#"{"version":2,"events":[]}"#;
        assert!(sc.scrub_envelope(empty).is_none());
    }

    #[test]
    fn unicode_is_boundary_safe() {
        assert_eq!(
            s("héllo ünï@dömain.com wörld"),
            "héllo ünï@dömain.com wörld"
        );
        assert_eq!(
            s("日本 password=секрет 日本"),
            "日本 password=[redacted] 日本"
        );
        assert_eq!(s("/Users/ёлка/x"), "/Users/[user]/x");
    }

    #[test]
    fn numeric_secrets_and_card_integers() {
        let sc = Scrubber::default();
        let mut v = serde_json::json!({
            "pin": 1234, "otp": 654321, "password": 12345678, "secret": [1, 2],
            "token_count": 7, "retry": 3, "key": 42,
            "card": 4111111111111111u64, "ts": 1712345678901u64, "ok": true
        });
        assert!(sc.scrub_value(&mut v));
        assert_eq!(v["pin"], "[redacted]");
        assert_eq!(v["otp"], "[redacted]");
        assert_eq!(v["password"], "[redacted]");
        assert_eq!(v["secret"][1], "[redacted]");
        assert_eq!(v["token_count"], 7);
        assert_eq!(v["key"], 42);
        assert_eq!(v["card"], "[card]");
        assert_eq!(v["ts"], 1712345678901u64);
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn colon_prose_is_not_a_secret() {
        for t in [
            "invalid token: expected ident",
            "unexpected token: `}`",
            "INFO my_app::token: refreshed",
            "token: expected one of `,` or `)`",
        ] {
            assert_eq!(s(t), t, "{t}");
        }
        assert_eq!(s("password: hunter2"), "password: [redacted]");
        // Only bare `token` gets the prose leniency.
        assert_eq!(s("password: secret"), "password: [redacted]");
        assert_eq!(
            s("invalid password: letmein"),
            "invalid password: [redacted]"
        );
        assert_eq!(s("passwd: letmein"), "passwd: [redacted]");
        assert_eq!(s("secret: required"), "secret: [redacted]");
        assert_eq!(s("api_key: abcdefgh"), "api_key: [redacted]");
        assert_eq!(s("access_token: abcdefgh"), "access_token: [redacted]");
        assert_eq!(s("token: abc123def456"), "token: [redacted]");
        assert_eq!(
            s(r#"{"password": "short"}"#),
            r#"{"password": "[redacted]"}"#
        );
    }

    #[test]
    fn cookie_header_takes_the_whole_line() {
        assert_eq!(
            s("Cookie: a=1; sid=abcdef1234; theme=dark\nHost: x"),
            "Cookie: [redacted]\nHost: x"
        );
        assert_eq!(s("Set-Cookie: id=1; Path=/"), "Set-Cookie: [redacted]");
    }

    #[test]
    fn escaped_quotes_inside_quoted_values() {
        assert_eq!(
            s(r#"{"password": "a\"bcdefgh tail", "n": 1}"#),
            r#"{"password": "[redacted]", "n": 1}"#
        );
    }

    #[test]
    fn stem_exceptions_and_key_value_looks() {
        for k in [
            "author",
            "authority",
            ":authority",
            "tokenizer",
            "token_type",
            "token_count",
            "method_signature",
            "secretary",
            "sessionStart",
            "session_state",
            "session_timeout",
            "keyboard",
            "monkey",
            "authorization_endpoint",
            "authorization_url",
            "authorization_uri",
            "oauth_client_id",
            "auth_url",
            "auth_endpoint",
            "oauth_scope",
            "oauth2_redirect_uri",
            "token_endpoint_auth_method",
            "token_endpoint_auth_signing_alg_values_supported",
            "auth_method",
            "client_id",
            "redirect_uri",
            "issuer",
            "jwks_uri",
            "scope",
            "cancellation_token",
            "CancellationToken",
            "www-authenticate",
            "WWW-Authenticate",
            "token_endpoint",
            "token_url",
            "token_uri",
            "token_expiry",
            "token_expires_at",
            "token_ttl",
            "refresh_token_count",
            "max_token_count",
            "cancel_token",
        ] {
            assert!(!is_sensitive_key(k), "{k}");
        }
        for k in [
            "authorization",
            "oauth",
            "oauth_verifier",
            "code_verifier",
            "oauth_pin",
            "oauth_signature",
            "oauth_token",
            "oauth_client_secret",
            "oauth_access_token",
            "id_token",
            "auth",
            "oauth_token",
            "access_token",
            "client_secret",
            "sessionId",
            "session",
            "signature",
            "signatures",
            "AccountKey",
            "x-api-key",
            "PWD",
            "p_w_d_token",
        ] {
            assert!(is_sensitive_key(k), "{k}");
        }
        let sc = Scrubber::default();
        let mut v = serde_json::json!({"key": "Enter", "sig": "ok", "author": "Bob"});
        assert!(!sc.scrub_value(&mut v));
        let mut v = serde_json::json!({"key": "abcdEFGH1234abcdEFGH1234", "other": {"key": "user:42:profile"}});
        assert!(sc.scrub_value(&mut v));
        assert_eq!(v["key"], "[redacted]");
        assert_eq!(v["other"]["key"], "user:42:profile");
        assert_eq!(s("key=user:42:profile"), "key=user:42:profile");
        assert_eq!(s("key=abcd1234efgh5678"), "key=[redacted]");
    }

    #[test]
    fn file_names_and_shared_folders_are_not_pii() {
        assert_eq!(s("icon@2x.png logo@3x.JPG"), "icon@2x.png logo@3x.JPG");
        assert_eq!(s("a@example.com"), "[email]");
        assert_eq!(s("/Users/Shared/Library/x"), "/Users/Shared/Library/x");
        // deny beats allow.
        let sc = Scrubber::new(ScrubConfig::default().allow("acme").deny("acme"));
        assert_eq!(sc.scrub_str("acme corp").as_ref(), "[redacted] corp");
    }

    /// `might_match == false` must mean "scrubbing changes nothing".
    #[test]
    fn prefilter_never_skips_a_record_that_would_change() {
        let sc = Scrubber::default();
        for rec in [
            r#"{"m":"a@b.com"}"#,
            r#"{"password":"x"}"#,
            r#"{"m":"4111 1111 1111 1111"}"#,
            r#"{"p_w_d":"hunter2"}"#,
            r#"{"P-W-D":"x"}"#,
            r#"{"p w d":"x"}"#,
            r#"{"m":"\u0061@b.com"}"#,
            r#"{"\u0070assword":"x"}"#,
            r#"{"card":4111111111111111}"#,
            r#"{"m":"/home/bob/x"}"#,
            r#"{"m":"Authorization: Bearer abc123"}"#,
        ] {
            if sc.scrub_json_bytes(rec.as_bytes()).is_some() {
                assert!(sc.might_match(rec.as_bytes()), "{rec}");
            }
        }
        assert!(!sc.might_match(br#"{"timestamp":1,"level":2,"message":"connection reset"}"#));
    }

    /// Deterministic pseudo-random (xorshift) soup of every trigger fragment.
    #[test]
    fn fuzz_never_panics() {
        const FRAGS: &[&str] = &[
            "@",
            "a@b.co",
            "sk-",
            "sk_live_",
            "ghp_",
            "eyJ",
            ".",
            "-",
            "_",
            "password=",
            "auth:",
            "key=",
            ":",
            "::",
            "=",
            "\"",
            "\\\"",
            "\\",
            "[",
            "]",
            "{",
            "}",
            "/Users/",
            "/home/",
            "C:\\Users\\",
            "4111 1111 1111 1111",
            "4111",
            "1",
            "9",
            " ",
            "\n",
            "é",
            "日",
            "Bearer ",
            "Basic ",
            concat!("-----BEGIN ", "PRIVATE KEY-----"),
            "-----END ",
            "://",
            "x",
            "token",
            "AKIA",
        ];
        let sc = Scrubber::default();
        let mut x: u64 = 0x2545F4914F6CDD1D;
        for _ in 0..3000 {
            let mut input = String::new();
            for _ in 0..(x % 40) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                input.push_str(FRAGS[(x % FRAGS.len() as u64) as usize]);
            }
            // Output is valid UTF-8 by construction; the point is: no panic
            // on any slice boundary. (Not idempotent in general: replacing a
            // match can expose a new word boundary for the next pass.)
            let _ = sc.scrub_str(&input);
        }
    }

    /// Adversarial 1 MB inputs finish in a bounded time (they used to be
    /// quadratic). The bound is generous so debug builds and loaded CI pass.
    #[test]
    fn pathological_inputs_are_not_quadratic() {
        let sc = Scrubber::default();
        let mb = 1 << 20;
        let limit = std::time::Duration::from_secs(if cfg!(debug_assertions) { 20 } else { 2 });
        for unit in [
            "sk-",
            "cookie:",
            "password: \"a\\\"",
            "\\\"",
            "hf_",
            "bearer: ",
            "eyJ-",
            "eyJ.",
            "auth:",
            "password=",
            "key=",
            "a@",
            "@",
            "4111 ",
            "1-",
            "/Users/",
            "/home/a/",
            "::",
            "Bearer ",
            "basic ",
            "://",
            concat!("-----BEGIN ", "PRIVATE KEY-----"),
            "ghp_",
            "AKIA",
            "[",
            "\\\"password\\\":",
        ] {
            let input = unit.repeat(mb / unit.len());
            let t = std::time::Instant::now();
            let out = sc.scrub_str(&input);
            let took = t.elapsed();
            assert!(
                took < limit,
                "{unit:?} took {took:?} (out {} bytes)",
                out.len()
            );
        }
        // One huge token run and one huge value.
        for input in [
            format!("sk-{}", "a1".repeat(mb / 2)),
            format!("password={}", "x".repeat(mb)),
        ] {
            let t = std::time::Instant::now();
            let _ = sc.scrub_str(&input);
            assert!(t.elapsed() < limit);
        }
    }

    /// Run with `--ignored --nocapture` for the documented hot-path cost.
    #[test]
    #[ignore]
    fn scrub_throughput() {
        let sc = Scrubber::default();
        let clean =
            "GET /api/v1/orders failed: connection reset by peer after 3 retries (attempt=3)";
        let n = 200_000;
        let t = std::time::Instant::now();
        let mut keep = 0;
        for _ in 0..n {
            keep += sc.scrub_str(std::hint::black_box(clean)).len();
        }
        println!("clean {}B: {:?}/str ({keep})", clean.len(), t.elapsed() / n);
        let dirty = "user bob@example.com token=abc123 at /home/bob/app/src/main.rs:10";
        let t = std::time::Instant::now();
        for _ in 0..n {
            keep += sc.scrub_str(std::hint::black_box(dirty)).len();
        }
        println!("dirty {}B: {:?}/str ({keep})", dirty.len(), t.elapsed() / n);
        // A realistic 100-record log channel.
        let rec = r#"{"timestamp":1712345678901,"level":1,"source":2,"message":"GET /api/v1/orders failed: connection reset by peer after 3 retries"}"#;
        let doc = format!(r#"{{"version":2,"events":[{}]}}"#, vec![rec; 100].join(","));
        let t = std::time::Instant::now();
        for _ in 0..2000 {
            keep += sc
                .scrub_envelope(std::hint::black_box(doc.as_bytes()))
                .map_or(0, |v| v.len());
        }
        println!(
            "envelope 100 recs ({}B): {:?}/doc ({keep})",
            doc.len(),
            t.elapsed() / 2000
        );
    }
}
