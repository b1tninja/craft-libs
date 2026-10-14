//! The idempotency tag: a short random reference that one [`PrintJob`](crate::PrintJob) puts on
//! every attempt of one print, so that a later reconcile can find a job that an attempt created even
//! when its reply was lost.
//!
//! The tag is a *correlation id*, not a secret: where the protocol has no dedicated field it travels
//! as a suffix of the job name (`"Quarterly report [ref abc123de]"`), visible to anyone who can see
//! the queue. See `docs/job-tracking.md` for why it is on by default for network backends and off
//! for the local spooler.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// A short random reference (8 lowercase hexadecimal characters), generated once per print.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JobTag(String);

const PREFIX: &str = " [ref ";

impl JobTag {
    /// The number of characters in a tag.
    pub const LEN: usize = 8;

    /// A new tag from the process's randomly keyed hasher and a counter: unique within the process,
    /// unpredictable across runs. It is a correlation id, not a security token.
    pub fn random() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        let bits = hasher.finish();
        // Fold the 64 bits to 32 so that both halves of the hash contribute.
        let folded = ((bits >> 32) ^ (bits & 0xFFFF_FFFF)) as u32;
        Self(format!("{folded:08x}"))
    }

    /// The tag spelled `text`: exactly [`Self::LEN`] lowercase hexadecimal characters.
    pub fn parse(text: &str) -> Option<Self> {
        let valid = text.len() == Self::LEN && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        valid.then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The suffix that carries the tag in a name: `" [ref abc123de]"`.
    pub fn suffix(&self) -> String {
        format!("{PREFIX}{}]", self.0)
    }

    /// The tag a name ends with, when it ends with one.
    pub fn find_in(name: &str) -> Option<Self> {
        split_suffix(name).map(|(_, tag)| tag)
    }

    /// `name` without a trailing tag suffix.
    pub fn strip(name: &str) -> &str {
        split_suffix(name).map_or(name, |(base, _)| base)
    }

    /// `name` with this tag as its suffix, **exactly once**: a suffix the name already ends with is
    /// replaced, never doubled. With `max_chars` the name is cut so that the whole result fits and
    /// the suffix survives; a limit shorter than the suffix itself leaves the tag out (the cut name
    /// alone is returned).
    pub fn apply_to(&self, name: &str, max_chars: Option<usize>) -> String {
        let base = Self::strip(name);
        let suffix = self.suffix();
        let Some(max) = max_chars else {
            return format!("{base}{suffix}");
        };
        let room = max.saturating_sub(suffix.chars().count());
        if room == 0 && max < suffix.chars().count() {
            return base.chars().take(max).collect();
        }
        let mut out: String = base.chars().take(room).collect();
        out.push_str(&suffix);
        out
    }
}

impl std::fmt::Display for JobTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn split_suffix(name: &str) -> Option<(&str, JobTag)> {
    let body = name.strip_suffix(']')?;
    let start = body.rfind(PREFIX)?;
    let tag = JobTag::parse(body.get(start + PREFIX.len()..)?)?;
    Some((body.get(..start)?, tag))
}

/// Where a backend carries the tag of a [`SubmitRequest`](crate::SubmitRequest).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TagPlacement {
    /// As a suffix of the job name (`"title [ref abc123de]"`). The backend applies it with
    /// [`JobTag::apply_to`], within its own length limit.
    JobName,
    /// As the same suffix on the document name.
    DocumentName,
    /// In a dedicated job attribute of the protocol, which the title does not show.
    Native,
    /// The backend cannot carry a tag; the registry does not hand it one.
    #[default]
    None,
}

/// What a backend can do with a tag: where it carries it and whether a job listing returns it, so
/// that a reconcile can match on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct TagSupport {
    pub placement: TagPlacement,
    /// [`Backend::list_jobs`](crate::backends::Backend::list_jobs) returns the tag of the jobs that
    /// carry it ([`RemoteJob::tag`](crate::RemoteJob)). Without it a reconcile falls back to title,
    /// user and time.
    pub matchable: bool,
}

impl TagSupport {
    /// No tag support (the default).
    pub const NONE: Self = Self { placement: TagPlacement::None, matchable: false };

    pub const fn new(placement: TagPlacement, matchable: bool) -> Self {
        Self { placement, matchable }
    }

    /// Whether the backend carries a tag at all.
    pub const fn carries(self) -> bool {
        !matches!(self.placement, TagPlacement::None)
    }
}

/// Whether a [`PrintJob`](crate::PrintJob) puts its tag on the requests it sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TagPolicy {
    /// On for [`BackendClass::Network`](crate::BackendClass::Network) backends, off for local ones.
    #[default]
    Auto,
    /// On wherever the backend can carry a tag.
    Always,
    /// Never.
    Never,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_random_tag_is_eight_hex_characters_and_differs_between_calls() {
        let a = JobTag::random();
        let b = JobTag::random();
        assert_eq!(a.as_str().len(), JobTag::LEN);
        assert_eq!(JobTag::parse(a.as_str()), Some(a.clone()));
        assert_ne!(a, b);
    }

    #[test]
    fn parse_is_strict() {
        assert!(JobTag::parse("abc123de").is_some());
        for bad in ["", "abc123d", "abc123def", "ABC123DE", "abc123dg", "abc 23de"] {
            assert_eq!(JobTag::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_suffix_is_applied_once_and_found_again() {
        let tag = JobTag::parse("abc123de").unwrap_or_else(JobTag::random);
        let named = tag.apply_to("Report", None);
        assert_eq!(named, "Report [ref abc123de]");
        assert_eq!(tag.apply_to(&named, None), named, "applying twice does not double the suffix");
        assert_eq!(JobTag::find_in(&named), Some(tag.clone()));
        assert_eq!(JobTag::strip(&named), "Report");
        assert_eq!(JobTag::strip("Report"), "Report");
        assert_eq!(JobTag::find_in("Report [ref nothex!!]"), None);
        assert_eq!(JobTag::find_in("Report [ref abc123de] more"), None, "only a trailing suffix counts");
    }

    #[test]
    fn a_length_limit_cuts_the_title_and_keeps_the_suffix() {
        let tag = JobTag::parse("abc123de").unwrap_or_else(JobTag::random);
        let long = "x".repeat(400);
        let named = tag.apply_to(&long, Some(255));
        assert_eq!(named.chars().count(), 255);
        assert!(named.ends_with(" [ref abc123de]"));
        assert_eq!(JobTag::find_in(&named), Some(tag.clone()));
        assert_eq!(tag.apply_to("abc", Some(5)), "abc", "a limit below the suffix leaves the tag out");
        let multibyte = tag.apply_to(&"é".repeat(300), Some(255));
        assert_eq!(multibyte.chars().count(), 255, "limits count characters, not bytes");
    }

    #[test]
    fn support_helpers() {
        assert!(!TagSupport::NONE.carries());
        assert!(TagSupport::new(TagPlacement::Native, true).carries());
        assert_eq!(TagSupport::default(), TagSupport::NONE);
    }
}
