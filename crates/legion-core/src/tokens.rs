//! `tokens.toml`: the bearer tokens `legion serve` accepts, each standing for one principal.
//!
//! Only a token's BLAKE3 hash is kept, so reading the file (or a backup of it) gives nobody a
//! token that works. A token is long and random, so a fast hash is enough: there is no password
//! to guess, only 256 bits.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::Deserialize;

use crate::config::Document;
use crate::policy::Principal;

/// How every token starts, so a token pasted where it should not be is recognised for what it is,
/// by people and by the audit log's redaction alike.
pub const PREFIX: &str = "lgn_";

/// How a stored hash starts, naming the function, as every hash Legion writes does.
const HASH_PREFIX: &str = "blake3:";

/// The longest a token's name may be.
const LONGEST_NAME: usize = 64;

/// `tokens.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tokens {
    /// Every token, by the name it was given when it was made.
    #[serde(default)]
    pub tokens: BTreeMap<String, TokenEntry>,
}

/// One token as the file keeps it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenEntry {
    /// The principal every request carrying the token acts as.
    pub principal: Principal,
    /// `blake3:<hex>` of the token.
    pub hash: String,
    /// When it was made, RFC 3339.
    pub created: String,
    /// When it stops working, RFC 3339; never when absent.
    #[serde(default)]
    pub expires: Option<String>,
}

impl Document for Tokens {
    const FILE: &'static str = "tokens.toml";
    // No tokens is the default, and it lets nobody in.
    const EMPTY_IS_DEFAULT: bool = true;

    fn validate(&self) -> Result<(), String> {
        for (name, entry) in &self.tokens {
            check_name(name)?;
            if parse_hash(&entry.hash).is_none() {
                return Err(format!(
                    "tokens.{name}.hash must be blake3: followed by 64 hexadecimal digits"
                ));
            }
            parse_time(&entry.created)
                .map_err(|error| format!("tokens.{name}.created: {error}"))?;
            if let Some(expires) = &entry.expires {
                parse_time(expires).map_err(|error| format!("tokens.{name}.expires: {error}"))?;
            }
        }
        Ok(())
    }
}

impl Tokens {
    /// The principal a presented token stands for, or `None` when it is no token here or one that
    /// has expired.
    ///
    /// Every stored hash is compared, each in constant time, whether or not an earlier one
    /// matched, so how long the answer takes does not say which entry was close. Expiry is checked
    /// only after, and the caller learns nothing about why a token was refused.
    pub fn verify(&self, presented: &str, now: Timestamp) -> Option<&str> {
        let presented = blake3::hash(presented.as_bytes());
        let mut found = None;
        for entry in self.tokens.values() {
            // blake3::Hash compares in constant time.
            let matches = parse_hash(&entry.hash).is_some_and(|stored| stored == presented);
            if matches && found.is_none() {
                found = Some(entry);
            }
        }
        let entry = found?;
        let expired = entry
            .expires
            .as_deref()
            .is_some_and(|expires| parse_time(expires).map_or(true, |expires| now >= expires));
        (!expired).then(|| entry.principal.as_str())
    }
}

/// What a token is stored as: `blake3:<hex>` of its text.
pub fn hash(token: &str) -> String {
    format!("{HASH_PREFIX}{}", blake3::hash(token.as_bytes()).to_hex())
}

/// Checks a token's name: lower-case letters, digits, `-` and `_`, starting with a letter or digit,
/// so it is a plain key in the file and a plain word on a command line.
pub fn check_name(name: &str) -> Result<(), String> {
    let starts_well = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_fits = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if starts_well && rest_fits && name.len() <= LONGEST_NAME {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a token name: use lower-case letters, digits, - and _, starting with a letter or digit, at most {LONGEST_NAME} characters"
        ))
    }
}

/// An RFC 3339 time as the file writes it.
pub fn parse_time(text: &str) -> Result<Timestamp, String> {
    text.parse::<Timestamp>()
        .map_err(|error| format!("{text:?} is not an RFC 3339 time: {error}"))
}

fn parse_hash(text: &str) -> Option<blake3::Hash> {
    text.strip_prefix(HASH_PREFIX)
        .and_then(|hex| blake3::Hash::from_hex(hex).ok())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::parse;

    fn file(text: &str) -> Result<Tokens, String> {
        parse::<Tokens>(Path::new("tokens.toml"), text.as_bytes()).map_err(|error| error.message)
    }

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn a_token_is_its_principal_until_it_expires() {
        let tokens = file(&format!(
            "[tokens.phone]\nprincipal = \"phone\"\nhash = \"{}\"\ncreated = \"2026-01-01T00:00:00Z\"\nexpires = \"2027-01-01T00:00:00Z\"\n\n[tokens.laptop]\nprincipal = \"laptop\"\nhash = \"{}\"\ncreated = \"2026-01-01T00:00:00Z\"\n",
            hash("lgn_phone"),
            hash("lgn_laptop")
        ))
        .unwrap();
        let before = at("2026-06-01T00:00:00Z");
        let after = at("2027-01-01T00:00:00Z");
        assert_eq!(tokens.verify("lgn_phone", before), Some("phone"));
        assert_eq!(tokens.verify("lgn_phone", after), None);
        assert_eq!(tokens.verify("lgn_laptop", after), Some("laptop"));
        assert_eq!(tokens.verify("lgn_other", before), None);
        assert_eq!(tokens.verify("", before), None);
        assert_eq!(Tokens::default().verify("lgn_phone", before), None);
    }

    #[test]
    fn the_file_is_checked_strictly() {
        let good = hash("x");
        assert!(file("").is_ok(), "an empty file is no tokens");
        for (text, says) in [
            (
                format!(
                    "[tokens.Phone]\nprincipal = \"p\"\nhash = \"{good}\"\ncreated = \"2026-01-01T00:00:00Z\"\n"
                ),
                "not a token name",
            ),
            (
                "[tokens.p]\nprincipal = \"p\"\nhash = \"sha256:00\"\ncreated = \"2026-01-01T00:00:00Z\"\n".to_owned(),
                "hash must be",
            ),
            (
                format!("[tokens.p]\nprincipal = \"p\"\nhash = \"{good}\"\ncreated = \"soon\"\n"),
                "created",
            ),
            (
                format!(
                    "[tokens.p]\nprincipal = \"p\"\nhash = \"{good}\"\ncreated = \"2026-01-01T00:00:00Z\"\nexpires = \"later\"\n"
                ),
                "expires",
            ),
            (
                format!(
                    "[tokens.p]\nprincipal = \"p\"\nhash = \"{good}\"\ncreated = \"2026-01-01T00:00:00Z\"\ntoken = \"lgn_x\"\n"
                ),
                "unknown field",
            ),
        ] {
            let error = file(&text).unwrap_err();
            assert!(error.contains(says), "{error}");
        }
    }

    #[test]
    fn a_stored_hash_names_its_function() {
        let stored = hash("lgn_abc");
        assert!(stored.starts_with("blake3:"));
        assert_eq!(stored.len(), "blake3:".len() + 64);
        assert!(parse_hash(&stored).is_some());
    }
}
