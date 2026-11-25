//! Guards checked-in text against private tool names, hosts and identifiers.
//!
//! The rules are structural so this check does not carry the forbidden names
//! themselves. Every reported token needs a `path:token` entry with a
//! justification in `repo-hygiene-allowlist.txt`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const ALLOWLIST: &str = "repo-hygiene-allowlist.txt";
const SKIPPED_DIRS: [&str; 3] = [".git", "dist", "target"];
const TOOL_SUFFIX: &str = "-target";
const SOURCE_PREFIX: &str = "from the ";

#[derive(Debug, PartialEq, Eq)]
struct Finding {
    path: String,
    line: usize,
    rule: &'static str,
    token: String,
}

#[test]
fn checked_in_text_uses_neutral_names() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let allowlist = load_allowlist(&root.join(ALLOWLIST));
    let mut findings = Vec::new();

    for path in checked_in_files(&root) {
        let relative = path
            .strip_prefix(&root)
            .expect("file is below the crate root")
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            check_line(&relative, index + 1, line, &allowlist, &mut findings);
        }
    }

    assert!(
        findings.is_empty(),
        "unapproved names in checked-in files:\n{}",
        findings
            .iter()
            .map(|finding| format!(
                "{}:{}: {} {:?}",
                finding.path, finding.line, finding.rule, finding.token
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn load_allowlist(path: &Path) -> HashSet<(String, String)> {
    let text = fs::read_to_string(path).expect("allowlist is readable");
    let mut entries = HashSet::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (path, token) = line
            .split_once(':')
            .expect("allowlist entry has a path and a token");
        entries.insert((path.trim().to_owned(), token.trim().to_owned()));
    }
    assert!(!entries.is_empty(), "allowlist must not be empty");
    entries
}

fn checked_in_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files.sort();
    files
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIPPED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            collect_files(&path, files);
        } else if metadata.is_file() {
            files.push(path);
        }
    }
}

fn check_line(
    path: &str,
    line: usize,
    text: &str,
    allowlist: &HashSet<(String, String)>,
    findings: &mut Vec<Finding>,
) {
    for word in words(text) {
        if let Some(prefix) = word.strip_suffix(TOOL_SUFFIX)
            && !prefix.is_empty()
            && prefix
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            record(path, line, "tool-named fixture", word, allowlist, findings);
        }
        if word.len() == 2
            && word.as_bytes()[0].is_ascii_uppercase()
            && word.as_bytes()[1].is_ascii_digit()
        {
            record(path, line, "short tool label", word, allowlist, findings);
        }
    }
    for token in github_slugs(text) {
        record(path, line, "repository slug", &token, allowlist, findings);
    }
    for token in markdown_slugs(text) {
        record(path, line, "repository slug", &token, allowlist, findings);
    }
    for token in url_hosts(text) {
        record(path, line, "hostname", &token, allowlist, findings);
    }
    for token in email_addresses(text) {
        record(path, line, "email address", &token, allowlist, findings);
    }
    for token in copyright_holders(text) {
        record(path, line, "copyright holder", &token, allowlist, findings);
    }
    for token in source_repository_names(text) {
        record(
            path,
            line,
            "source repository reference",
            &token,
            allowlist,
            findings,
        );
    }
}

fn record(
    path: &str,
    line: usize,
    rule: &'static str,
    token: &str,
    allowlist: &HashSet<(String, String)>,
    findings: &mut Vec<Finding>,
) {
    if allowlist.contains(&(path.to_owned(), token.to_owned())) {
        return;
    }
    findings.push(Finding {
        path: path.to_owned(),
        line,
        rule,
        token: token.to_owned(),
    });
}

fn words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    for (index, byte) in text.bytes().enumerate() {
        if byte.is_ascii_alphanumeric() || byte == b'-' {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(first) = start.take() {
            words.push(&text[first..index]);
        }
    }
    if let Some(first) = start {
        words.push(&text[first..]);
    }
    words
}

fn is_slug_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

fn github_slugs(text: &str) -> Vec<String> {
    const PREFIX: &str = "github.com/";
    let mut slugs = Vec::new();
    for (_, rest) in text.match_indices(PREFIX) {
        let tail = &rest[PREFIX.len()..];
        let path = tail
            .split(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-' | '/'))
            })
            .next()
            .unwrap_or("");
        let mut segments = path.split('/');
        if let (Some(owner), Some(repo), _) = (segments.next(), segments.next(), segments.next())
            && !owner.is_empty()
            && !repo.is_empty()
        {
            slugs.push(format!("{owner}/{repo}"));
        }
    }
    slugs
}

fn markdown_slugs(text: &str) -> Vec<String> {
    let mut slugs = Vec::new();
    for part in text.split('[').skip(1) {
        let Some(end) = part.find(']') else {
            continue;
        };
        let candidate = &part[..end];
        let mut segments = candidate.split('/');
        if let (Some(owner), Some(repo), None) = (segments.next(), segments.next(), segments.next())
            && !owner.is_empty()
            && !repo.is_empty()
            && owner.bytes().all(is_slug_byte)
            && repo.bytes().all(is_slug_byte)
        {
            slugs.push(candidate.to_owned());
        }
    }
    slugs
}

fn url_hosts(text: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for scheme in ["https://", "http://"] {
        for (_, rest) in text.match_indices(scheme) {
            let tail = &rest[scheme.len()..];
            let host: String = tail
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(*character, '.' | '-')
                })
                .collect();
            if host.contains('.') {
                hosts.push(host);
            }
        }
    }
    hosts
}

fn email_addresses(text: &str) -> Vec<String> {
    let mut emails = Vec::new();
    for part in text.split(|character: char| {
        !(character.is_ascii_alphanumeric()
            || matches!(character, '@' | '.' | '_' | '%' | '+' | '-'))
    }) {
        let Some((local, domain)) = part.split_once('@') else {
            continue;
        };
        if !local.is_empty()
            && domain.contains('.')
            && domain
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            emails.push(part.to_owned());
        }
    }
    emails
}

fn copyright_holders(text: &str) -> Vec<String> {
    let lowered = text.to_ascii_lowercase();
    let mut holders = Vec::new();
    let mut offset = 0;
    while let Some(index) = lowered[offset..].find("copyright") {
        let start = offset + index + "copyright".len();
        let rest = text[start..].trim_start();
        let rest = rest
            .strip_prefix("(c)")
            .map(str::trim_start)
            .unwrap_or(rest);
        let year_len = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if year_len == 4 {
            let holder = rest[year_len..]
                .trim()
                .trim_matches(|character: char| matches!(character, '.' | ','));
            if !holder.is_empty() {
                holders.push(holder.to_owned());
            }
        }
        offset = start;
    }
    holders
}

fn source_repository_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (_, rest) in text.match_indices(SOURCE_PREFIX) {
        let tail = &rest[SOURCE_PREFIX.len()..];
        let name_len = tail
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric())
            .count();
        let (name, after) = tail.split_at(name_len);
        if name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_uppercase())
            && after.trim_start().starts_with(" repository")
        {
            names.push(name.to_owned());
        }
    }
    names
}
