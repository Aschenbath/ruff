//! Loads, matches, and writes diagnostic baselines.
//!
//! Baselines let a project adopt ty without first fixing every existing error or adding individual
//! suppression comments.
//! By recording those errors, a baseline lets you focus on new ones as you work on the code.
//! The same approach is useful when enabling a new rule in an existing project.
//!
//! The diagnostics are stored in a project-wide JSON file. Those matched by the baseline are
//! demoted to [`Severity::Hint`]. The CLI hides them, while the LSP keeps them visible to encourage
//! fixing them or acknowledging them with a suppression comment.
//!
//! Like suppression comments, baselines can only suppress lint diagnostics.
//! Syntax errors and diagnostics such as `reveal_type` cannot be suppressed this way.
//!
//! Inline suppression comments such as `# ty: ignore[invalid-assignment]` take precedence over
//! the baseline. If you add one for a baselined diagnostic, the next baseline update removes
//! that entry: the comment now suppresses it.
//!
//! # Recognizing an existing diagnostic
//!
//! The tricky part is recognizing the same error as the code evolves. Adding an import, for
//! example, shifts this assignment error down by two lines without fixing it:
//!
//! ```diff
//! +import logging
//! +
//!  def get_name() -> str: ...
//!
//!  name: int = get_name()
//! ```
//!
//! To recognize errors across edits, we use two source-text anchors: the code leading up to the
//! error and the code starting at the error itself. A diagnostic matches a baseline entry with the
//! same file and rule if either anchor's normalized text matches, allowing code on the other side
//! to change. This heuristic is inspired by the [two-hash approach][paper] described in section III-C.
//!
//! This can preserve matches after:
//!
//! - Adding or removing lines before the error.
//! - Changing spacing, or indenting or dedenting a block.
//! - Moving or reordering blocks within the same file.
//! - Editing nearby code on one side of the error while the other side stays unchanged.
//! - Editing code outside the hashed context.
//! - Switching between LF, CRLF, and CR line endings.
//!
//! Ideally, a baseline would also follow an error when its file is renamed or the code moves to
//! another file. Our heuristic only matches within a file, so these changes require updating the
//! baseline.
//!
//! # Context hashes
//!
//! The two anchors are windows of source text: one extends backward, the other forward.
//! Both include the word containing the start of the primary range, so they overlap on `get_name`
//! here. The two windows cover the following source text:
//!
//! ```text
//!    name: int = get_name()
//!                ^^^^^^^^^^     primary range
//! ...name: int = get_name        preceding window, read backward
//!                get_name()...  following window, read forward
//! ```
//!
//! We take up to 100 characters (Unicode scalar values) in each direction, stopping at file
//! boundaries. This keeps enough context to distinguish similar errors without depending on too
//! much nearby code. In Home Assistant trials, shorter windows confused similar diagnostics;
//! larger token windows resurfaced more existing diagnostics in Home Assistant and Airflow.
//!
//! We ignore spaces and indentation so that changing spacing or reindenting a block preserves its
//! anchors. Line breaks are retained because they can affect Python semantics, but LF, CRLF, and CR
//! all become `\n` so that changing line endings also preserves matches (e.g. because of Git's
//! automatic newline handling). The character limit applies after this normalization. We don't lex
//! the source, so harmless line breaks inside parentheses are kept too, and rewrapping a call can
//! change its hash.
//!
//! Each anchor is stored as a hash to keep the baseline compact, rather than storing the source text.
//! Including `get_name` in both anchors means replacing it with `get_count` changes both hashes.
//! This prevents unchanged preceding code from hiding a different call, but also means a harmless
//! rename can make an existing diagnostic reappear.
//!
//! # Reordering and duplicates
//!
//! Moving code within a file should not require updating the baseline if its hashes still match.
//! We process diagnostics in source order, giving each the first unused matching entry. In this
//! example, A and B have distinct matching fingerprints:
//!
//! ```text
//! baseline    current    result
//! A, B        B, A       both still match
//! A           A, A       only one matches
//! ```
//!
//! Using each entry only once prevents it from hiding extra occurrences. If two diagnostics have
//! identical fingerprints, though, we cannot tell which is the old one.
//!
//! # Format versions
//!
//! Baseline support is experimental and uses format version 0, which may change between ty releases.
//! We will bump the format version to 1 when promoting the feature to stable. From then on, ty must
//! be able to read and match baselines written by the previous ty version. Updating a baseline
//! may always write the current format.
//!
//! [paper]: https://codeql.github.com/publications/tracking-analysis-violations.pdf

use std::collections::BTreeMap;
use std::fmt;
use std::hash::Hasher;

use anyhow::Context;
use compact_str::CompactString;
use ruff_db::diagnostic::{Annotation, Diagnostic, DiagnosticId, LintName, Severity, Span};
use ruff_db::files::{File, FilePath, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_text_size::{TextRange, TextSize};
use rustc_hash::FxHashMap;
use rustc_stable_hash::{FromStableHash, SipHasher128Hash, StableSipHasher128};
use serde::de::{self, Visitor};
use serde::{Deserialize, Serialize};

use crate::Db;

const BASELINE_VERSION: u32 = 0;
const CONTEXT_SIZE: usize = 100;

/// Returns a diagnostic if the configured baseline cannot be loaded.
pub(crate) fn settings_diagnostic(db: &dyn Db) -> Option<Diagnostic> {
    db.project().settings(db).baseline()?;
    baselines(db).err().map(Error::to_diagnostic)
}

/// Demotes matched diagnostics to hints and adds a note identifying the baseline.
pub(crate) fn demote_diagnostics(db: &dyn Db, file: File, diagnostics: &mut [Diagnostic]) {
    let project = db.project();
    let Some(baseline_path) = project.settings(db).baseline() else {
        return;
    };
    let Some(baseline_entries) = baseline(db, file) else {
        return;
    };
    let baseline_path = baseline_path
        .strip_prefix(project.root(db))
        .unwrap_or(baseline_path);

    let source = source_text(db, file);
    let mut entries: Vec<_> = diagnostics
        .iter()
        .enumerate()
        .filter_map(|(index, diagnostic)| {
            let diagnostic = BaselineDiagnostic::from_diagnostic(diagnostic)?;
            Some((index, diagnostic.entry(source.as_str())))
        })
        .collect();
    entries.sort_by(|(_, left), (_, right)| left.cmp(right));

    let mut matcher = BaselineMatcher::new(baseline_entries);
    for (index, entry) in entries {
        if matcher.consume(&entry) {
            let diagnostic = &mut diagnostics[index];
            diagnostic.set_severity(Severity::Hint);
            diagnostic.info(format_args!("Suppressed by baseline `{baseline_path}`"));
        }
    }
}

pub struct WriteResult {
    /// The number of diagnostics written to the baseline.
    pub count: usize,
    /// The diagnostics that could not be baselined.
    pub diagnostics: Vec<Diagnostic>,
}

/// Writes a new baseline containing all eligible diagnostics.
///
/// Returns how many diagnostics were baselined and the remaining diagnostics.
pub fn write(
    db: &dyn Db,
    path: &SystemPath,
    mut diagnostics: Vec<Diagnostic>,
) -> anyhow::Result<WriteResult> {
    let count = BaselineFile::extract_from_diagnostics(db, &mut diagnostics).write(db, path)?;
    // Errors from loading the old baseline no longer apply after a successful write.
    diagnostics.retain(|diagnostic| diagnostic.id() != DiagnosticId::InvalidBaseline);
    Ok(WriteResult { count, diagnostics })
}

/// Returns the baseline for `file` without depending on the query when baselines are disabled.
fn baseline(db: &dyn Db, file: File) -> Option<&[BaselineEntry]> {
    #[expect(clippy::question_mark, reason = "keep the query guard explicit")]
    let Some(_) = db.project().settings(db).baseline() else {
        return None;
    };

    let baseline = baselines(db).ok()?.as_ref()?;
    baseline.for_file(db, file)
}

/// Loads and validates the configured baseline.
///
/// Reading through `source_text` makes this query dependent on the baseline file's contents.
#[salsa::tracked(returns(as_ref), heap_size = ruff_memory_usage::heap_size)]
fn baselines(db: &dyn Db) -> Result<Option<Baseline>, Error> {
    let project = db.project();
    let Some(path) = project.settings(db).baseline() else {
        return Ok(None);
    };

    let file = system_path_to_file(db, path).map_err(|error| Error::Read {
        path: path.to_path_buf(),
        file: None,
        detail: error.to_string(),
    })?;

    let source = source_text(db, file);
    if let Some(error) = source.read_error() {
        return Err(Error::Read {
            path: path.to_path_buf(),
            file: Some(file),
            detail: error.to_string(),
        });
    }

    let baseline: BaselineFile =
        serde_json::from_str(source.as_str()).map_err(|error| Error::Parse {
            path: path.to_path_buf(),
            file,
            detail: error.to_string(),
        })?;

    if baseline.version != BASELINE_VERSION {
        return Err(Error::UnsupportedVersion {
            file,
            version: baseline.version,
        });
    }

    Ok(Some(Baseline(baseline.files)))
}

#[derive(Debug, Clone, Eq, PartialEq, get_size2::GetSize)]
struct Baseline(BTreeMap<SystemPathBuf, Box<[BaselineEntry]>>);

impl Baseline {
    fn for_file(&self, db: &dyn Db, file: File) -> Option<&[BaselineEntry]> {
        let path = project_relative_path(db, file)?;
        self.0.get(path).map(Box::as_ref)
    }
}

/// The JSON baseline for the entire project: a format version and diagnostics grouped by file.
#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaselineFile {
    version: u32,
    /// Source-file paths relative to the project root.
    files: BTreeMap<SystemPathBuf, Box<[BaselineEntry]>>,
}

impl BaselineFile {
    fn extract_from_diagnostics(db: &dyn Db, diagnostics: &mut Vec<Diagnostic>) -> Self {
        let mut grouped: FxHashMap<File, Vec<BaselineDiagnostic>> = FxHashMap::default();
        diagnostics.retain(|diagnostic| {
            let Some(diagnostic) = BaselineDiagnostic::from_diagnostic(diagnostic) else {
                return true;
            };
            if project_relative_path(db, diagnostic.file).is_none() {
                return true;
            }
            grouped.entry(diagnostic.file).or_default().push(diagnostic);
            false
        });

        let mut files = BTreeMap::new();
        #[expect(
            clippy::iter_over_hash_type,
            reason = "each file is processed independently and its entries are sorted"
        )]
        for (file, diagnostics) in grouped {
            let Some(path) = project_relative_path(db, file) else {
                continue;
            };
            let source = source_text(db, file);
            let mut entries: Vec<_> = diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.entry(source.as_str()))
                .collect();
            entries.sort();
            let mut portable_path = String::with_capacity(path.as_str().len());
            for component in path.components() {
                if !portable_path.is_empty() {
                    portable_path.push('/');
                }
                portable_path.push_str(component.as_str());
            }
            files.insert(
                SystemPathBuf::from(portable_path),
                entries.into_boxed_slice(),
            );
        }

        Self {
            version: BASELINE_VERSION,
            files,
        }
    }

    fn write(self, db: &dyn Db, path: &SystemPath) -> anyhow::Result<usize> {
        let count = self.files.values().map(|entries| entries.len()).sum();
        let mut serialized = serde_json::to_string_pretty(&self)?;
        serialized.push('\n');

        let writable = db
            .system()
            .as_writable()
            .context("The active file system is read-only")?;
        if let Some(parent) = path.parent() {
            writable
                .create_directory_all(parent)
                .with_context(|| format!("Failed to create baseline directory `{parent}`"))?;
        }
        writable
            .write_file(path, &serialized)
            .with_context(|| format!("Failed to write baseline `{path}`"))?;
        Ok(count)
    }
}

/// The rule and context hashes for one diagnostic in a [`BaselineFile`].
#[derive(
    Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, get_size2::GetSize,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaselineEntry {
    /// Used to sort generated entries; baseline files preserve their serialized array order.
    #[serde(skip)]
    offset: TextSize,
    rule: CompactString,
    preceding_hash: BaselineHash,
    following_hash: BaselineHash,
}

impl BaselineEntry {
    fn matches(&self, other: &Self) -> bool {
        self.rule == other.rule
            && (self.preceding_hash == other.preceding_hash
                || self.following_hash == other.following_hash)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error, get_size2::GetSize)]
enum Error {
    #[error("Failed to read baseline `{path}`")]
    Read {
        path: SystemPathBuf,
        file: Option<File>,
        detail: String,
    },

    #[error("Failed to parse baseline `{path}`")]
    Parse {
        path: SystemPathBuf,
        file: File,
        detail: String,
    },

    #[error("Unsupported baseline version `{version}`")]
    UnsupportedVersion { file: File, version: u32 },
}

impl Error {
    fn to_diagnostic(&self) -> Diagnostic {
        let mut diagnostic = Diagnostic::new(
            DiagnosticId::InvalidBaseline,
            Severity::Error,
            self.to_string(),
        );
        let (file, detail) = match self {
            Self::Read { file, detail, .. } => (*file, detail.clone()),
            Self::Parse { file, detail, .. } => (Some(*file), detail.clone()),
            Self::UnsupportedVersion { file, .. } => (
                Some(*file),
                format!("ty supports baseline version `{BASELINE_VERSION}`"),
            ),
        };

        if let Some(file) = file {
            diagnostic.annotate(Annotation::primary(
                Span::from(file).with_range(TextRange::default()),
            ));
        }
        diagnostic.info(detail);
        diagnostic
    }
}

fn project_relative_path(db: &dyn Db, file: File) -> Option<&SystemPath> {
    let FilePath::System(path) = file.path(db) else {
        return None;
    };
    path.strip_prefix(db.project().root(db)).ok()
}

struct BaselineDiagnostic {
    rule: LintName,
    file: File,
    range: TextRange,
}

impl BaselineDiagnostic {
    fn from_diagnostic(diagnostic: &Diagnostic) -> Option<Self> {
        let rule = diagnostic.id().as_lint()?;
        let span = diagnostic.primary_span()?;
        Some(Self {
            rule,
            file: span.expect_ty_file(),
            range: span.range()?,
        })
    }

    fn entry(&self, source: &str) -> BaselineEntry {
        let (preceding_hash, following_hash) = context_hashes(source, self.range.start());
        BaselineEntry {
            offset: self.range.start(),
            rule: CompactString::const_new(self.rule.as_str()),
            preceding_hash,
            following_hash,
        }
    }
}

struct BaselineMatcher<'a> {
    entries: &'a [BaselineEntry],
    next: usize,
    consumed: Vec<bool>,
}

impl<'a> BaselineMatcher<'a> {
    fn new(entries: &'a [BaselineEntry]) -> Self {
        Self {
            entries,
            next: 0,
            consumed: Vec::new(),
        }
    }

    fn consume(&mut self, current: &BaselineEntry) -> bool {
        let remaining = &self.entries[self.next..];
        // `next` always points to the first unused entry. Most diagnostics match it,
        // so they need neither a search nor an allocation to track consumed entries.
        if let Some(entry) = remaining.first()
            && entry.matches(current)
        {
            self.next += 1;
            while self.consumed.get(self.next) == Some(&true) {
                self.next += 1;
            }
            return true;
        }

        let Some((offset, _)) = remaining
            .iter()
            .enumerate()
            .skip(1)
            .find(|(offset, entry)| {
                self.consumed.get(self.next + offset) != Some(&true) && entry.matches(current)
            })
        else {
            return false;
        };

        // Entries before `next` are excluded from searches, so they need no mask bits.
        if self.consumed.is_empty() {
            self.consumed.resize(self.entries.len(), false);
        }
        self.consumed[self.next + offset] = true;
        true
    }
}

/// Anchors both windows at the word containing `offset`; the diagnostic's end is not used.
/// The character limit includes the word itself (truncating long words) and normalized newlines.
fn context_hashes(source: &str, offset: TextSize) -> (BaselineHash, BaselineHash) {
    let offset = usize::from(offset);
    let (word_start, word_end) = word_bounds(source, offset);

    (
        BaselineHash::backward(&source[..word_end]),
        BaselineHash::forward(&source[word_start..]),
    )
}

/// A word is a run of alphanumeric characters or underscores. Other characters stand on their own.
fn word_bounds(source: &str, offset: usize) -> (usize, usize) {
    let Some(character) = source[offset..].chars().next() else {
        return (offset, offset);
    };

    let is_word_character = |character: char| character == '_' || character.is_alphanumeric();
    if !is_word_character(character) {
        return (offset, offset + character.len_utf8());
    }

    let start = source[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_word_character(*character))
        .last()
        .map_or(offset, |(index, _)| index);
    let end = source[offset..]
        .char_indices()
        .find(|(_, character)| !is_word_character(*character))
        .map_or(source.len(), |(index, _)| offset + index);

    (start, end)
}

#[derive(Clone, Copy)]
enum HashDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, get_size2::GetSize)]
struct BaselineHash(u128);

impl BaselineHash {
    fn forward(context: &str) -> Self {
        Self::from_context(context, HashDirection::Forward)
    }

    fn backward(context: &str) -> Self {
        Self::from_context(context, HashDirection::Backward)
    }

    fn from_context(context: &str, direction: HashDirection) -> Self {
        let mut hasher = StableSipHasher128::new();
        let mut characters = context.char_indices();
        let mut length = 0_u8;

        while usize::from(length) < CONTEXT_SIZE {
            let character = match direction {
                HashDirection::Forward => characters.next(),
                HashDirection::Backward => characters.next_back(),
            };
            let Some((index, character)) = character else {
                break;
            };
            let character = match character {
                '\r' if context.as_bytes().get(index + 1) == Some(&b'\n') => continue,
                '\r' | '\n' => '\n',
                character if character.is_whitespace() => continue,
                character => character,
            };

            hasher.write_u32(u32::from(character));
            length += 1;
        }

        hasher.write_u8(length);
        hasher.finish()
    }
}

impl FromStableHash for BaselineHash {
    type Hash = SipHasher128Hash;

    fn from(SipHasher128Hash([first, second]): SipHasher128Hash) -> Self {
        Self((u128::from(first) << 64) | u128::from(second))
    }
}

impl fmt::Display for BaselineHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

impl Serialize for BaselineHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BaselineHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BaselineHashVisitor;

        impl Visitor<'_> for BaselineHashVisitor {
            type Value = BaselineHash;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a 32-character hexadecimal hash")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() != 32 {
                    return Err(E::invalid_length(value.len(), &self));
                }

                u128::from_str_radix(value, 16)
                    .map(BaselineHash)
                    .map_err(E::custom)
            }
        }

        deserializer.deserialize_str(BaselineHashVisitor)
    }
}

#[cfg(test)]
mod tests {
    use ruff_db::diagnostic::{Annotation, Diagnostic, DiagnosticId, Severity, Span};
    use ruff_db::files::{File, system_path_to_file};
    use ruff_db::system::{DbWithWritableSystem as _, SystemPath, SystemPathBuf};
    use ruff_db::testing::assert_const_function_query_was_not_run;
    use ruff_text_size::TextRange;

    use crate::db::Db as _;
    use crate::db::testing::TestDb;
    use crate::metadata::options::Options;
    use crate::metadata::value::RelativePathBuf;
    use crate::{ProjectMetadata, check_file_impl};
    use ty_python_semantic::Db as _;

    use super::{
        BaselineEntry, BaselineFile, BaselineHash, BaselineMatcher, Error, baseline, baselines,
        context_hashes, demote_diagnostics, write,
    };

    fn entry(rule: &str, preceding: &str, following: &str) -> BaselineEntry {
        BaselineEntry {
            offset: 0.into(),
            rule: rule.into(),
            preceding_hash: BaselineHash::backward(preceding),
            following_hash: BaselineHash::forward(following),
        }
    }

    #[test]
    fn baseline_hash_serializes_as_hexadecimal() -> anyhow::Result<()> {
        let hash = BaselineHash(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let serialized = r#""0123456789abcdef0123456789abcdef""#;

        assert_eq!(serde_json::to_string(&hash)?, serialized);
        assert_eq!(serde_json::from_str::<BaselineHash>(serialized)?, hash);
        assert!(serde_json::from_str::<BaselineHash>(r#""0123456789abcdef""#).is_err());
        assert!(
            serde_json::from_str::<BaselineHash>(r#""0123456789abcdef0123456789abcdeg""#).is_err()
        );
        Ok(())
    }

    #[test]
    fn entries_match_by_rule_and_either_hash() {
        assert!(entry("rule", "a", "b").matches(&entry("rule", "a", "c")));
        assert!(entry("rule", "a", "b").matches(&entry("rule", "c", "b")));
        assert!(!entry("rule", "a", "b").matches(&entry("rule", "c", "d")));
        assert!(!entry("rule", "a", "b").matches(&entry("other-rule", "a", "b")));
    }

    #[test]
    fn context_hash_ignores_non_newline_whitespace() {
        assert_eq!(
            context_hashes("a + \tç . d", 5.into()),
            context_hashes("a+ç.d", 2.into())
        );
    }

    #[test]
    fn context_hashes_preserve_newlines() {
        let (preceding, following) = context_hashes("before\nword\nafter", 7.into());

        assert_eq!(preceding, BaselineHash::backward("before\nword"));
        assert_eq!(following, BaselineHash::forward("word\nafter"));
        assert_ne!(
            (preceding, following),
            context_hashes("before word after", 7.into())
        );
    }

    #[test]
    fn context_hashes_normalize_line_endings() {
        let expected = context_hashes("before\nword\nafter", 7.into());

        assert_eq!(expected, context_hashes("before\rword\rafter", 7.into()));
        assert_eq!(
            expected,
            context_hashes("before\r\nword\r\nafter", 8.into())
        );
    }

    #[test]
    fn context_hashes_limit_each_window() {
        let source = format!("{}target{}", "ç ".repeat(100), " β".repeat(100));
        let (preceding, following) = context_hashes(&source, 300.into());

        assert_eq!(
            preceding,
            BaselineHash::backward(&format!("{}target", "ç".repeat(94)))
        );
        assert_eq!(
            following,
            BaselineHash::forward(&format!("target{}", "β".repeat(94)))
        );
    }

    #[test]
    fn context_hashes_overlap_complete_word() {
        let (preceding, following) = context_hashes("before diagnostic_word.after", 7.into());

        assert_eq!(preceding, BaselineHash::backward("beforediagnostic_word"));
        assert_eq!(following, BaselineHash::forward("diagnostic_word.after"));
    }

    #[test]
    fn context_hashes_overlap_word_containing_diagnostic_start() {
        let (preceding, following) = context_hashes("before café_word2.after", 9.into());

        assert_eq!(preceding, BaselineHash::backward("beforecafé_word2"));
        assert_eq!(following, BaselineHash::forward("café_word2.after"));
    }

    #[test]
    fn context_hashes_overlap_punctuation() {
        let (preceding, following) = context_hashes("before + after", 7.into());

        assert_eq!(preceding, BaselineHash::backward("before+"));
        assert_eq!(following, BaselineHash::forward("+after"));
    }

    #[test]
    fn schema_rejects_unknown_fields() {
        let mut baseline = serde_json::json!({
            "version": 0,
            "files": {
                "test.py": [{
                    "rule": "invalid-assignment",
                    "precedingHash": "0123456789abcdef0123456789abcdef",
                    "followingHash": "fedcba9876543210fedcba9876543210"
                }]
            }
        });
        let mut unknown_top_level = baseline.clone();
        unknown_top_level["unknown"] = true.into();
        assert!(serde_json::from_value::<BaselineFile>(unknown_top_level).is_err());
        baseline["files"]["test.py"][0]["unknown"] = true.into();
        assert!(serde_json::from_value::<BaselineFile>(baseline).is_err());
    }

    #[test]
    fn matching_preserves_duplicate_cardinality() {
        let duplicate = entry("rule", "before", "after");
        let baseline = [duplicate.clone(), duplicate.clone()];
        let mut matcher = BaselineMatcher::new(&baseline);

        assert!(matcher.consume(&duplicate));
        assert!(matcher.consume(&duplicate));
        assert!(!matcher.consume(&duplicate));
    }

    #[test]
    fn matching_allows_reordered_diagnostics() {
        let first = entry("rule", "first-before", "first-after");
        let second = entry("rule", "second-before", "second-after");
        let baseline = [first.clone(), second.clone()];
        let mut matcher = BaselineMatcher::new(&baseline);

        assert!(matcher.consume(&second));
        assert!(matcher.consume(&first));
    }

    #[test]
    fn matching_retains_skipped_entries_after_matching_prefix() {
        let prefix = entry("prefix-rule", "prefix-before", "prefix-after");
        let first = entry("rule", "first-before", "first-after");
        let second = entry("other-rule", "second-before", "second-after");
        let new = entry("rule", "new-before", "new-after");
        let baseline = [prefix.clone(), first.clone(), second.clone(), first.clone()];
        let mut matcher = BaselineMatcher::new(&baseline);

        assert!(matcher.consume(&prefix));
        assert!(!matcher.consume(&new));
        assert!(matcher.consume(&second));
        assert!(!matcher.consume(&second));
        assert!(matcher.consume(&first));
        assert!(matcher.consume(&first));
        assert!(!matcher.consume(&first));
    }

    fn test_db_with_baseline() -> TestDb {
        let root = SystemPathBuf::from("/project");
        let mut metadata = ProjectMetadata::new("test", root);
        metadata.set_override_options(Options {
            baseline: Some(RelativePathBuf::cli("/project/baseline.json")),
            ..Options::default()
        });
        TestDb::new(metadata)
    }

    #[test]
    fn serialization_order_ignores_input_order_and_severity() -> ruff_db::system::Result<()> {
        let mut db = test_db_with_baseline();
        let source_path = SystemPath::new("/project/test.py");
        db.write_file(
            source_path,
            "\
x
",
        )?;
        let source_file = system_path_to_file(&db, source_path).unwrap();
        let span = Span::from(source_file).with_range(TextRange::new(0.into(), 1.into()));

        let mut warning = Diagnostic::new(
            DiagnosticId::lint("z-warning-name-longer-than-twenty-four-characters"),
            Severity::Warning,
            "warning",
        );
        warning.annotate(Annotation::primary(span.clone()));
        let mut error = Diagnostic::new(DiagnosticId::lint("a-error"), Severity::Error, "error");
        error.annotate(Annotation::primary(span));

        let initial =
            BaselineFile::extract_from_diagnostics(&db, &mut vec![warning.clone(), error.clone()]);

        let baseline_path = SystemPath::new("/project/baseline.json");
        db.write_file(baseline_path, serde_json::to_string(&initial)?)?;
        let mut diagnostics = vec![warning.clone(), error.clone()];
        demote_diagnostics(&db, source_file, &mut diagnostics);
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity() == Severity::Hint)
        );

        warning.set_severity(Severity::Hint);
        error.set_severity(Severity::Hint);
        assert_eq!(
            BaselineFile::extract_from_diagnostics(&db, &mut vec![error, warning]),
            initial
        );
        Ok(())
    }

    #[test]
    fn diagnostics_without_source_ranges_remain_unsuppressed() -> anyhow::Result<()> {
        let mut db = test_db_with_baseline();
        let source_path = SystemPath::new("/project/test.py");
        let baseline_path = SystemPath::new("/project/baseline.json");
        db.write_file(source_path, "x: int = 'wrong'\n")?;
        let file = system_path_to_file(&db, source_path)?;

        let mut diagnostics = db.check_file(file);
        let without_span = Diagnostic::new(
            DiagnosticId::lint("invalid-assignment"),
            Severity::Error,
            "diagnostic without a source location",
        );
        let mut without_range = without_span.clone();
        without_range.annotate(Annotation::primary(Span::from(file)));
        diagnostics.extend([without_span, without_range]);

        let written = write(&db, baseline_path, diagnostics.clone())?;
        assert_eq!(written.count, 1);
        assert_eq!(written.diagnostics, diagnostics[1..]);

        File::sync_path(&mut db, baseline_path);
        demote_diagnostics(&db, file, &mut diagnostics);
        assert_eq!(diagnostics[0].severity(), Severity::Hint);
        assert_eq!(diagnostics[1..], written.diagnostics);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn baseline_preserves_backslashes_in_filenames() -> anyhow::Result<()> {
        let mut db = test_db_with_baseline();
        let source_path = SystemPath::new(r"/project/nested/a\b.py");
        let baseline_path = SystemPath::new("/project/baseline.json");
        db.write_file(source_path, "x: int = 'wrong'\n")?;
        let file = system_path_to_file(&db, source_path)?;

        let written = write(&db, baseline_path, db.check_file(file))?;
        assert_eq!(written.count, 1);
        File::sync_path(&mut db, baseline_path);
        assert_eq!(db.check_file(file)[0].severity(), Severity::Hint);
        Ok(())
    }

    #[test]
    fn disabled_baseline_does_not_run_baseline_query() -> ruff_db::system::Result<()> {
        let mut db = TestDb::new(ProjectMetadata::new(
            "test",
            SystemPathBuf::from("/project"),
        ));
        let source_path = SystemPath::new("/project/test.py");
        db.write_file(
            source_path,
            "\
x: int = 'wrong'
",
        )?;
        let source_file = system_path_to_file(&db, source_path).unwrap();
        db.take_salsa_events();

        assert!(baseline(&db, source_file).is_none());
        let diagnostics = check_file_impl(&db, db.program_file(source_file));
        assert_eq!(diagnostics.unwrap()[0].severity(), Severity::Error);

        let events = db.take_salsa_events();
        assert_const_function_query_was_not_run(&db, baselines, &events);
        Ok(())
    }

    #[test]
    fn baseline_file_changes_invalidate_matching() -> anyhow::Result<()> {
        let mut db = test_db_with_baseline();
        let source_path = SystemPath::new("/project/test.py");
        let baseline_path = SystemPath::new("/project/baseline.json");
        db.write_file(
            source_path,
            "\
x: int = 'wrong'
",
        )?;
        let source_file = system_path_to_file(&db, source_path).unwrap();

        let initial_result = check_file_impl(&db, db.program_file(source_file));
        let initial = initial_result.as_ref().unwrap();
        assert!(initial.iter().any(|diagnostic| {
            diagnostic.id().is_lint() && diagnostic.severity() != Severity::Hint
        }));

        let written = write(&db, baseline_path, initial.to_vec())?;
        assert_eq!(written.count, 1);
        assert!(written.diagnostics.is_empty());
        ruff_db::files::File::sync_path(&mut db, baseline_path);

        let baselined_result = check_file_impl(&db, db.program_file(source_file));
        let baselined = baselined_result.as_ref().unwrap();
        assert!(baselined.iter().any(|diagnostic| {
            diagnostic.id().is_lint() && diagnostic.severity() == Severity::Hint
        }));

        db.write_file(
            baseline_path,
            r#"{
                "version": 1,
                "files": {}
            }"#,
        )?;
        assert!(matches!(
            baselines(&db),
            Err(Error::UnsupportedVersion { version: 1, .. })
        ));
        assert!(
            db.project()
                .check_settings(&db)
                .iter()
                .any(|diagnostic| diagnostic.id() == DiagnosticId::InvalidBaseline)
        );
        Ok(())
    }

    #[test]
    fn removing_baseline_entries_restores_diagnostics() -> anyhow::Result<()> {
        let mut db = test_db_with_baseline();
        let first_path = SystemPath::new("/project/first.py");
        let second_path = SystemPath::new("/project/nested/second.py");
        let baseline_path = SystemPath::new("/project/baseline.json");
        db.write_file(
            first_path,
            "\
x: int = 'wrong'
",
        )?;
        db.write_file(
            second_path,
            "\
y: int = 'wrong'
",
        )?;
        let first_file = system_path_to_file(&db, first_path).unwrap();
        let second_file = system_path_to_file(&db, second_path).unwrap();

        let mut diagnostics = db.check_file(first_file);
        diagnostics.extend(db.check_file(second_file));
        let written = write(&db, baseline_path, diagnostics.clone())?;
        assert_eq!(written.count, 2);
        assert!(written.diagnostics.is_empty());
        File::sync_path(&mut db, baseline_path);

        assert_eq!(db.check_file(first_file)[0].severity(), Severity::Hint);
        assert_eq!(db.check_file(second_file)[0].severity(), Severity::Hint);

        let mut updated = BaselineFile::extract_from_diagnostics(&db, &mut diagnostics);
        updated.files.remove(SystemPath::new("first.py"));
        db.write_file(baseline_path, serde_json::to_string(&updated)?)?;

        assert_eq!(db.check_file(first_file)[0].severity(), Severity::Error);
        assert_eq!(db.check_file(second_file)[0].severity(), Severity::Hint);
        Ok(())
    }

    #[test]
    fn missing_baseline_is_a_settings_diagnostic() {
        let db = test_db_with_baseline();
        assert!(matches!(baselines(&db), Err(Error::Read { .. })));
        let diagnostics = db.project().check_settings(&db);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].id(), DiagnosticId::InvalidBaseline);
    }
}
