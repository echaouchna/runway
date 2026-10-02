//! Deterministic build-context packaging.
//!
//! Ignore rules, in order:
//! 1. Built-in exclusions that can never be re-included: VCS metadata,
//!    runway's own `.runway/` directory, and common credential files.
//! 2. `.runwayignore` in the build context (gitignore syntax), if present;
//!    otherwise `.dockerignore` (Docker semantics: patterns are anchored at the
//!    context root, `!` re-includes).
//!
//! The runway configuration file is excluded when it lives inside the context,
//! so configuration-only changes (scaling, env) do not trigger a rebuild.
//! The Dockerfile and `.dockerignore` are always included, as with `docker build`.
//! Entries are sorted and written with fixed timestamps and owners, so the same
//! inputs always produce the same tar stream and the same SHA-256.
//!
//! Packaging is split in two phases for speed:
//! - [`scan`] walks the context and hashes the tar stream without buffering or
//!   compressing it (this is all `plan` needs);
//! - [`compress`] streams the same tar through a parallel gzip encoder into an
//!   anonymous temporary file, only when an upload is actually required, and
//!   verifies the content still matches the scanned hash.

use crate::config::BuildStrategy;
use crate::error::{Error, Result};
use gzp::deflate::Gzip;
use gzp::par::compress::{ParCompress, ParCompressBuilder};
use gzp::{Compression, ZWriter};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// Fixed modification time for archive entries (1980-01-02, safe for zip-based tooling).
const FIXED_MTIME: u64 = 315_619_200;

/// Patterns that are never uploaded.
pub const BUILTIN_EXCLUDES: &[&str] = &[
    ".git/",
    ".hg/",
    ".svn/",
    ".runway/",
    ".env",
    ".env.*",
    "!.env.example",
    "!.env.sample",
    "!.env.template",
    "*.pem",
    "*.p12",
    "*.pfx",
    "id_rsa*",
    "id_ecdsa*",
    "id_ed25519*",
    ".ssh/",
    ".aws/",
    ".gcloud/",
    ".config/gcloud/",
    ".netrc",
    "application_default_credentials.json",
    "credentials.json",
    "gha-creds-*.json",
    "*.tfstate",
    "*.tfstate.*",
    ".terraform/",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoreSource {
    RunwayIgnore,
    DockerIgnore,
    None,
}

impl IgnoreSource {
    pub fn describe(&self) -> &'static str {
        match self {
            IgnoreSource::RunwayIgnore => ".runwayignore",
            IgnoreSource::DockerIgnore => ".dockerignore",
            IgnoreSource::None => "built-in exclusions only",
        }
    }
}

/// Result of scanning a build context: what will be uploaded and its hash.
#[derive(Debug, Clone)]
pub struct SourceManifest {
    pub root: PathBuf,
    pub strategy: BuildStrategy,
    /// SHA-256 (hex) of the build strategy and the uncompressed tar stream.
    pub sha256: String,
    /// Content address of the image: the source hash plus the base images'
    /// digests (see [`crate::build::inputs`]). Equals `sha256` until base
    /// images are resolved.
    pub inputs_sha256: String,
    /// Resolved base images (empty until resolved).
    pub bases: Vec<crate::build::inputs::BaseImage>,
    /// Number of regular files and symlinks included.
    pub files: usize,
    /// Size of the uncompressed tar stream.
    pub tar_bytes: u64,
    pub ignore_source: IgnoreSource,
    /// Included paths, relative to the context, `/`-separated.
    pub entries: Vec<String>,
    items: Vec<Entry>,
}

/// A compressed archive in an anonymous temporary file (deleted on drop/exit).
#[derive(Debug)]
pub struct CompressedArchive {
    /// Positioned at the start.
    pub file: std::fs::File,
    pub len: u64,
}

impl CompressedArchive {
    /// A fresh handle positioned at the start (for upload retries).
    pub fn reopen(&self) -> Result<std::fs::File> {
        let mut f = self.file.try_clone()?;
        f.rewind()?;
        Ok(f)
    }

    /// Reads the whole archive (tests and small tools).
    pub fn read_all(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.len as usize);
        self.reopen()?.read_to_end(&mut out)?;
        Ok(out)
    }
}

fn builtin_matcher(root: &Path) -> Result<Gitignore> {
    let mut b = GitignoreBuilder::new(root);
    for p in BUILTIN_EXCLUDES {
        b.add_line(None, p)
            .map_err(|e| Error::internal(format!("bad built-in ignore pattern {p}: {e}")))?;
    }
    b.build()
        .map_err(|e| Error::internal(format!("cannot build ignore matcher: {e}")))
}

/// Translates a `.dockerignore` line into an equivalent root-anchored gitignore line.
pub fn dockerignore_to_gitignore(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (neg, pat) = match line.strip_prefix('!') {
        Some(p) => ("!", p.trim()),
        None => ("", line),
    };
    let mut pat = pat;
    while let Some(p) = pat.strip_prefix("./") {
        pat = p;
    }
    let pat = pat.trim_start_matches('/');
    if pat.is_empty() || pat == "." {
        return None;
    }
    if pat.starts_with("**") {
        Some(format!("{neg}{pat}"))
    } else {
        Some(format!("{neg}/{pat}"))
    }
}

/// `.dockerignore` rules in order, as Docker evaluates them: every rule is
/// tested against the path and each of its parent directories, and the last
/// rule that matches decides (an exception re-includes, any other excludes).
struct DockerRules {
    rules: Vec<(Gitignore, bool)>,
}

impl DockerRules {
    fn ignored(&self, abs: &Path, is_dir: bool) -> bool {
        let mut ignored = false;
        for (rule, exception) in &self.rules {
            if !rule.matched_path_or_any_parents(abs, is_dir).is_none() {
                ignored = !exception;
            }
        }
        ignored
    }
}

/// The user's ignore rules: gitignore rules from `.runwayignore`, or the
/// ordered rules and exception patterns of `.dockerignore`.
struct UserRules {
    git: Gitignore,
    docker: Option<DockerRules>,
    /// `.dockerignore` exceptions (`!…`, gitignore form, without the `!`).
    exceptions: Vec<String>,
}

fn user_matcher(root: &Path) -> Result<(UserRules, IgnoreSource)> {
    let mut b = GitignoreBuilder::new(root);
    let mut exceptions = Vec::new();
    let mut docker_rules = None;
    let runway = root.join(".runwayignore");
    let docker = root.join(".dockerignore");
    let source = if runway.is_file() {
        if let Some(e) = b.add(&runway) {
            return Err(Error::config(format!("invalid .runwayignore: {e}")));
        }
        IgnoreSource::RunwayIgnore
    } else if docker.is_file() {
        let text = std::fs::read_to_string(&docker)?;
        let mut rules = Vec::new();
        for line in text.lines() {
            if let Some(g) = dockerignore_to_gitignore(line) {
                let invalid = |e: ignore::Error| {
                    Error::config(format!("invalid .dockerignore pattern `{line}`: {e}"))
                };
                b.add_line(Some(docker.clone()), &g).map_err(invalid)?;
                let mut one = GitignoreBuilder::new(root);
                one.add_line(Some(docker.clone()), &g).map_err(invalid)?;
                let rule = one.build().map_err(invalid)?;
                if let Some(ex) = g.strip_prefix('!') {
                    exceptions.push(ex.to_string());
                }
                rules.push((rule, g.starts_with('!')));
            }
        }
        docker_rules = Some(DockerRules { rules });
        IgnoreSource::DockerIgnore
    } else {
        IgnoreSource::None
    };
    let gi = b
        .build()
        .map_err(|e| Error::config(format!("invalid ignore file: {e}")))?;
    Ok((
        UserRules {
            git: gi,
            docker: docker_rules,
            exceptions,
        },
        source,
    ))
}

/// Whether an exception pattern (gitignore form, anchored with `/` or
/// starting with `**`) could match something inside directory `dir` (a
/// relative path). `**` matches any number of directories. Conservative:
/// when unsure, the directory is walked (only costs time).
fn exception_may_apply_below(pattern: &str, dir: &str) -> bool {
    let pat: Vec<&str> = pattern
        .trim_start_matches('/')
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    let dir: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    may_match_below(&pat, &dir)
}

fn may_match_below(pat: &[&str], dir: &[&str]) -> bool {
    match (pat.first(), dir.first()) {
        // The directory is consumed: anything left in the pattern names
        // something inside it.
        (_, None) => !pat.is_empty(),
        // The pattern ends at an ancestor: it applies to the whole subtree.
        (None, Some(_)) => true,
        // `**` can absorb the rest of the directory and still match below.
        (Some(&"**"), Some(_)) => true,
        (Some(p), Some(d)) => component_matches(p, d) && may_match_below(&pat[1..], &dir[1..]),
    }
}

/// Glob match of one path component (`*`, `?`; character classes are
/// treated as matching).
fn component_matches(pattern: &str, name: &str) -> bool {
    if pattern.contains('[') {
        return true;
    }
    fn rec(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => rec(&p[1..], n) || (!n.is_empty() && rec(p, &n[1..])),
            (Some(b'?'), Some(_)) => rec(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => rec(&p[1..], &n[1..]),
            _ => false,
        }
    }
    rec(pattern.as_bytes(), name.as_bytes())
}

#[derive(Debug, Clone)]
struct Entry {
    rel: String,
    abs: PathBuf,
    kind: EntryKind,
}

#[derive(Debug, Clone)]
enum EntryKind {
    Dir,
    File { executable: bool },
    Symlink(PathBuf),
}

/// Collects the entries that would be uploaded, sorted.
fn collect(
    root: &Path,
    dockerfile: Option<&str>,
    excluded: &[String],
) -> Result<(Vec<Entry>, IgnoreSource)> {
    let builtin = builtin_matcher(root)?;
    let (user, source) = user_matcher(root)?;
    let exceptions = &user.exceptions;
    // `.dockerignore` follows Docker's semantics (see `DockerRules`), so
    // `!src/main.txt` re-includes a file inside an excluded directory unless a
    // later rule excludes it again. `.runwayignore` keeps gitignore semantics
    // (an excluded directory is final).
    let user_ignored = |abs: &Path, is_dir: bool| match &user.docker {
        Some(rules) => rules.ignored(abs, is_dir),
        None => user.git.matched(abs, is_dir).is_ignore(),
    };
    let forced: Vec<&str> = dockerfile.into_iter().chain([".dockerignore"]).collect();

    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| {
            Error::internal(format!("cannot read directory {}: {e}", dir.display()))
        })?;
        for ent in rd {
            let ent = ent?;
            let abs = ent.path();
            let ft = ent.file_type()?;
            let rel = abs
                .strip_prefix(root)
                .expect("walk stays under root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let is_dir = ft.is_dir();
            let force = forced.contains(&rel.as_str());
            if builtin.matched(&abs, is_dir).is_ignore()
                && !(force && Some(rel.as_str()) == dockerfile)
            {
                continue;
            }
            if !force && excluded.contains(&rel) {
                continue;
            }
            if !force && user_ignored(&abs, is_dir) {
                // Walk into an excluded directory only if an exception may
                // re-include something below it; its own entry is added
                // later, only if something inside is kept.
                if is_dir
                    && exceptions
                        .iter()
                        .any(|p| exception_may_apply_below(p, &rel))
                {
                    stack.push(abs);
                }
                continue;
            }
            if is_dir {
                out.push(Entry {
                    rel,
                    abs: abs.clone(),
                    kind: EntryKind::Dir,
                });
                stack.push(abs);
            } else if ft.is_symlink() {
                let target = std::fs::read_link(&abs)?;
                out.push(Entry {
                    rel,
                    abs,
                    kind: EntryKind::Symlink(target),
                });
            } else if ft.is_file() {
                let executable = is_executable(&ent.metadata()?);
                out.push(Entry {
                    rel,
                    abs,
                    kind: EntryKind::File { executable },
                });
            }
        }
    }
    // Ensure a forced Dockerfile inside an ignored directory is still present.
    if let Some(dockerfile) = dockerfile
        && !out.iter().any(|e| e.rel == dockerfile)
    {
        let abs = root.join(dockerfile);
        if abs.is_file() {
            let executable = is_executable(&std::fs::metadata(&abs)?);
            out.push(Entry {
                rel: dockerfile.to_string(),
                abs,
                kind: EntryKind::File { executable },
            });
        }
    }
    // Parent directories of files re-included inside excluded directories.
    let present: std::collections::BTreeSet<String> = out.iter().map(|e| e.rel.clone()).collect();
    let mut parents = std::collections::BTreeSet::new();
    for e in &out {
        let mut p = e.rel.as_str();
        while let Some((dir, _)) = p.rsplit_once('/') {
            if !present.contains(dir) {
                parents.insert(dir.to_string());
            }
            p = dir;
        }
    }
    for dir in parents {
        out.push(Entry {
            abs: root.join(&dir),
            rel: dir,
            kind: EntryKind::Dir,
        });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok((out, source))
}

#[cfg(unix)]
fn is_executable(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_m: &std::fs::Metadata) -> bool {
    false
}

fn header(kind: tar::EntryType, mode: u32, size: u64) -> tar::Header {
    let mut h = tar::Header::new_ustar();
    h.set_entry_type(kind);
    h.set_mode(mode);
    h.set_size(size);
    h.set_mtime(FIXED_MTIME);
    h.set_uid(0);
    h.set_gid(0);
    h
}

/// Writer that hashes everything written to it, optionally forwarding it.
struct HashingWriter<W: Write> {
    hasher: Sha256,
    inner: W,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Reader that fails if a file's length differs from the size recorded in its header.
struct ExactReader<R: Read> {
    inner: R,
    remaining: u64,
    path: String,
}

impl<R: Read> Read for ExactReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let max = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            return Err(std::io::Error::other(format!(
                "{} shrank while it was being packaged",
                self.path
            )));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Hash prefix: the strategy is part of the content address, so switching
/// between a Dockerfile and buildpacks (or builders) produces a new image.
/// The Dockerfile form is unchanged since the first release.
fn hash_prefix(strategy: &BuildStrategy) -> Vec<u8> {
    match strategy {
        BuildStrategy::Dockerfile { path } => format!("runway-source-v1\ndockerfile:{path}\n"),
        BuildStrategy::Buildpacks { builder } => {
            format!("runway-source-v1\nbuildpacks:{builder}\n")
        }
    }
    .into_bytes()
}

/// Writes the deterministic tar stream for `items`, streaming file contents.
fn write_tar<W: Write>(items: &[Entry], w: W) -> Result<W> {
    let mut tb = tar::Builder::new(w);
    tb.mode(tar::HeaderMode::Deterministic);
    for e in items {
        match &e.kind {
            EntryKind::Dir => {
                let mut h = header(tar::EntryType::Directory, 0o755, 0);
                tb.append_data(&mut h, format!("{}/", e.rel), std::io::empty())?;
            }
            EntryKind::File { executable } => {
                let f = std::fs::File::open(&e.abs)?;
                let len = f.metadata()?.len();
                let mode = if *executable { 0o755 } else { 0o644 };
                let mut h = header(tar::EntryType::Regular, mode, len);
                let reader = ExactReader {
                    inner: std::io::BufReader::with_capacity(256 * 1024, f),
                    remaining: len,
                    path: e.rel.clone(),
                };
                tb.append_data(&mut h, &e.rel, reader)?;
            }
            EntryKind::Symlink(target) => {
                let mut h = header(tar::EntryType::Symlink, 0o777, 0);
                tb.append_link(&mut h, &e.rel, target)?;
            }
        }
    }
    Ok(tb.into_inner()?)
}

/// Walks `root` and computes the content hash, without compressing anything.
///
/// `excluded` lists additional context-relative paths (`/`-separated) to skip.
pub fn scan(root: &Path, strategy: &BuildStrategy, excluded: &[String]) -> Result<SourceManifest> {
    let dockerfile = strategy.dockerfile();
    let (items, ignore_source) = collect(root, dockerfile, excluded)?;
    if let Some(dockerfile) = dockerfile
        && !items.iter().any(|e| e.rel == dockerfile)
    {
        return Err(Error::config(format!(
            "Dockerfile `{dockerfile}` not found in build context {}",
            root.display()
        )));
    }
    let mut hasher = Sha256::new();
    hasher.update(hash_prefix(strategy));
    let mut counter = CountingSink(0);
    let w = write_tar(
        &items,
        HashingWriter {
            hasher,
            inner: &mut counter,
        },
    )?;
    let sha256 = hex(&w.hasher.finalize());
    let entries: Vec<String> = items
        .iter()
        .filter(|e| !matches!(e.kind, EntryKind::Dir))
        .map(|e| e.rel.clone())
        .collect();
    Ok(SourceManifest {
        root: root.to_path_buf(),
        strategy: strategy.clone(),
        inputs_sha256: sha256.clone(),
        bases: Vec::new(),
        sha256,
        files: entries.len(),
        tar_bytes: counter.0,
        ignore_source,
        entries,
        items,
    })
}

struct CountingSink(u64);

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn compression_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 32)
}

/// Writes the gzipped archive for a scanned context to a temporary file using
/// all CPU cores, and verifies the content still matches `m.sha256`.
pub fn compress(m: &SourceManifest) -> Result<CompressedArchive> {
    let file = tempfile::tempfile()?;
    let gz_err = |e: gzp::GzpError| Error::internal(format!("compression failed: {e}"));
    let mut encoder: ParCompress<'static, Gzip, std::fs::File> = ParCompressBuilder::new()
        .compression_level(Compression::default())
        .num_threads(compression_threads())
        .map_err(gz_err)?
        .from_writer(file.try_clone()?);
    let mut hasher = Sha256::new();
    hasher.update(hash_prefix(&m.strategy));
    let w = write_tar(
        &m.items,
        HashingWriter {
            hasher,
            inner: &mut encoder,
        },
    )?;
    let sha256 = hex(&w.hasher.finalize());
    encoder.finish().map_err(gz_err)?;
    if sha256 != m.sha256 {
        return Err(Error::config(format!(
            "files in {} changed while packaging; re-run the command",
            m.root.display()
        )));
    }
    let len = file.metadata()?.len();
    let mut file = file;
    file.rewind()?;
    Ok(CompressedArchive { file, len })
}

/// Scans and compresses in one step (tests and simple callers).
pub fn package(
    root: &Path,
    strategy: &BuildStrategy,
    excluded: &[String],
) -> Result<(SourceManifest, CompressedArchive)> {
    let m = scan(root, strategy, excluded)?;
    let a = compress(&m)?;
    Ok((m, a))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn df() -> BuildStrategy {
        BuildStrategy::Dockerfile {
            path: "Dockerfile".into(),
        }
    }
    use std::fs;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn sample() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(r, "Dockerfile", "FROM scratch\n");
        write(r, "main.py", "print('hi')\n");
        write(r, "pkg/util.py", "x = 1\n");
        write(r, ".env", "SECRET=1\n");
        write(r, ".env.production", "SECRET=2\n");
        write(r, ".env.example", "SECRET=\n");
        write(r, ".git/config", "[core]\n");
        write(r, ".runway/tmp", "x\n");
        write(r, "certs/server.pem", "-----BEGIN-----\n");
        write(r, "application_default_credentials.json", "{}\n");
        write(r, "gha-creds-0a1b2c.json", "{}\n");
        d
    }

    #[test]
    fn excludes_credentials_vcs_and_tool_files() {
        let d = sample();
        let a = scan(d.path(), &df(), &[]).unwrap();
        assert_eq!(
            a.entries,
            [".env.example", "Dockerfile", "main.py", "pkg/util.py"]
        );
        assert_eq!(a.files, 4);
        assert_eq!(a.ignore_source, IgnoreSource::None);
    }

    #[test]
    fn archive_is_deterministic_and_content_addressed() {
        let d = sample();
        let a1 = scan(d.path(), &df(), &[]).unwrap();
        // Touch a file without changing content: hash must not change.
        let p = d.path().join("main.py");
        let content = fs::read(&p).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&p, &content).unwrap();
        let a2 = scan(d.path(), &df(), &[]).unwrap();
        assert_eq!(a1.sha256, a2.sha256);

        write(d.path(), "main.py", "print('changed')\n");
        let a3 = scan(d.path(), &df(), &[]).unwrap();
        assert_ne!(a1.sha256, a3.sha256);
    }

    #[test]
    fn runwayignore_takes_precedence_over_dockerignore() {
        let d = sample();
        write(d.path(), ".dockerignore", "pkg\n");
        write(d.path(), ".runwayignore", "*.py\n!main.py\n");
        let a = scan(d.path(), &df(), &[]).unwrap();
        assert_eq!(a.ignore_source, IgnoreSource::RunwayIgnore);
        assert!(a.entries.contains(&"main.py".to_string()));
        assert!(!a.entries.contains(&"pkg/util.py".to_string()));
        assert!(a.entries.contains(&".dockerignore".to_string()));
    }

    #[test]
    fn dockerignore_patterns_are_root_anchored() {
        let d = sample();
        write(d.path(), "docs/README.md", "x\n");
        write(d.path(), "README.md", "x\n");
        write(d.path(), "node_modules/a/index.js", "x\n");
        write(
            d.path(),
            ".dockerignore",
            "# comment\n*.md\nnode_modules\nDockerfile\n",
        );
        let a = scan(d.path(), &df(), &[]).unwrap();
        assert_eq!(a.ignore_source, IgnoreSource::DockerIgnore);
        assert!(!a.entries.contains(&"README.md".to_string()));
        // Docker's `*.md` only matches at the root.
        assert!(a.entries.contains(&"docs/README.md".to_string()));
        assert!(!a.entries.iter().any(|e| e.starts_with("node_modules")));
        // The Dockerfile is always sent, even when ignored.
        assert!(a.entries.contains(&"Dockerfile".to_string()));
    }

    #[test]
    fn dockerignore_exceptions_reinclude_files_in_excluded_directories() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "Dockerfile", "FROM scratch\n");
        write(d.path(), "src/main.txt", "keep\n");
        write(d.path(), "src/other.txt", "drop\n");
        write(d.path(), "lib/x.txt", "drop\n");
        write(d.path(), "vendor/keep/a.txt", "keep\n");
        write(d.path(), "vendor/drop/b.txt", "drop\n");
        // Allowlist: exclude everything, then re-include.
        write(
            d.path(),
            ".dockerignore",
            "*\n!Dockerfile\n!src/main.txt\n!vendor/keep/**\n",
        );
        let a = scan(d.path(), &df(), &[]).unwrap();
        let has = |p: &str| a.entries.contains(&p.to_string());
        assert!(has("src/main.txt"), "{:?}", a.entries);
        assert!(!has("src/other.txt"));
        assert!(!has("lib/x.txt"), "nothing re-included under lib");
        let (all, _) = collect(d.path(), Some("Dockerfile"), &[]).unwrap();
        let dirs: Vec<&str> = all
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Dir))
            .map(|e| e.rel.as_str())
            .collect();
        assert_eq!(
            dirs,
            ["src", "vendor", "vendor/keep"],
            "parent directories of kept files only"
        );
        assert!(has("vendor/keep/a.txt"));
        assert!(!has("vendor/drop/b.txt"));
        // `**` re-includes files at any depth.
        write(d.path(), "src/a/b/main.txt", "keep\n");
        write(d.path(), "src/a/b/other.txt", "drop\n");
        write(d.path(), ".dockerignore", "*\n!src/**/main.txt\n");
        let a = scan(d.path(), &df(), &[]).unwrap();
        assert!(
            a.entries.contains(&"src/a/b/main.txt".to_string()),
            "{:?}",
            a.entries
        );
        assert!(
            a.entries.contains(&"src/main.txt".to_string()),
            "`**` also matches no directory"
        );
        assert!(!a.entries.contains(&"src/a/b/other.txt".to_string()));
        // A later exclusion wins again, whether it names the file or a parent.
        for rules in [
            "src\n!src/main.txt\nsrc/main.txt\n",
            "*\n!src/main.txt\nsrc\n",
        ] {
            write(d.path(), ".dockerignore", rules);
            let a = scan(d.path(), &df(), &[]).unwrap();
            assert!(
                !a.entries.contains(&"src/main.txt".to_string()),
                "{rules:?}: {:?}",
                a.entries
            );
        }
        // ... and an exception after that re-includes it.
        write(
            d.path(),
            ".dockerignore",
            "*\n!src/main.txt\nsrc\n!src/main.txt\n",
        );
        let a = scan(d.path(), &df(), &[]).unwrap();
        assert!(a.entries.contains(&"src/main.txt".to_string()));
    }

    #[test]
    fn runwayignore_keeps_gitignore_semantics_for_excluded_directories() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "Dockerfile", "FROM scratch\n");
        write(d.path(), "src/main.txt", "x\n");
        write(d.path(), ".runwayignore", "src/\n!src/main.txt\n");
        let a = scan(d.path(), &df(), &[]).unwrap();
        assert!(
            !a.entries.contains(&"src/main.txt".to_string()),
            "as git: a parent exclusion is final"
        );
    }

    #[test]
    fn exception_reach() {
        assert!(exception_may_apply_below("/src/main.txt", "src"));
        assert!(!exception_may_apply_below("/src/main.txt", "lib"));
        assert!(!exception_may_apply_below("/src", "src"));
        assert!(exception_may_apply_below("/*/keep.txt", "anything"));
        assert!(exception_may_apply_below("**/keep.txt", "a/b"));
        assert!(exception_may_apply_below("/vendor/keep/**", "vendor/keep"));
        assert!(exception_may_apply_below("/src/**/main.txt", "src/a/b/c"));
        assert!(!exception_may_apply_below("/src/**/main.txt", "lib/a"));
        assert!(exception_may_apply_below("/src/*-gen/x.txt", "src/api-gen"));
        assert!(!exception_may_apply_below("/src/*-gen/x.txt", "src/api"));
    }

    #[test]
    fn extra_exclusions_skip_the_config_file() {
        let d = sample();
        write(d.path(), "runway.yaml", "version: 1\n");
        let with = scan(d.path(), &df(), &[]).unwrap();
        let without = scan(d.path(), &df(), &["runway.yaml".to_string()]).unwrap();
        assert!(with.entries.contains(&"runway.yaml".to_string()));
        assert!(!without.entries.contains(&"runway.yaml".to_string()));
        write(d.path(), "runway.yaml", "version: 1\n# changed\n");
        let changed = scan(d.path(), &df(), &["runway.yaml".to_string()]).unwrap();
        assert_eq!(
            without.sha256, changed.sha256,
            "config edits do not change the source hash"
        );
    }

    #[test]
    fn dockerignore_translation() {
        assert_eq!(dockerignore_to_gitignore("  # c"), None);
        assert_eq!(
            dockerignore_to_gitignore("./build/"),
            Some("/build/".into())
        );
        assert_eq!(
            dockerignore_to_gitignore("!keep.txt"),
            Some("!/keep.txt".into())
        );
        assert_eq!(
            dockerignore_to_gitignore("**/*.log"),
            Some("**/*.log".into())
        );
        assert_eq!(dockerignore_to_gitignore("."), None);
    }

    #[test]
    fn missing_dockerfile_is_a_config_error() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "main.py", "x");
        let e = scan(d.path(), &df(), &[]).unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::Config);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_executable_bit_and_extracts() {
        use std::os::unix::fs::PermissionsExt;
        let d = sample();
        write(d.path(), "run.sh", "#!/bin/sh\n");
        fs::set_permissions(d.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let (_, a) = package(d.path(), &df(), &[]).unwrap();
        let bytes = a.read_all().unwrap();
        let gz = flate2::read::GzDecoder::new(bytes.as_slice());
        let mut ar = tar::Archive::new(gz);
        let mut modes = std::collections::BTreeMap::new();
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            modes.insert(
                e.path().unwrap().to_string_lossy().into_owned(),
                e.header().mode().unwrap(),
            );
            assert_eq!(e.header().mtime().unwrap(), FIXED_MTIME);
        }
        assert_eq!(modes["run.sh"], 0o755);
        assert_eq!(modes["main.py"], 0o644);
    }

    /// The hash defines image tags and object names; it must stay stable
    /// across runway versions so existing images keep being reused.
    #[cfg(unix)]
    #[test]
    fn hash_definition_is_stable() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("pkg/empty")).unwrap();
        fs::write(r.join("Dockerfile"), "FROM scratch\nCOPY . /\n").unwrap();
        fs::write(r.join("main.py"), "print('hi')\n".repeat(1000)).unwrap();
        fs::write(r.join("pkg/util.py"), "x = 1\n").unwrap();
        std::os::unix::fs::symlink("main.py", r.join("link.py")).unwrap();
        assert_eq!(
            scan(r, &df(), &[]).unwrap().sha256,
            "8f3f0b1459b3509da153ecfe51322338e224a60de1fbf665eb5b106c8547b1f5"
        );
    }

    #[test]
    fn compressed_archive_is_a_single_gzip_stream_matching_the_hash() {
        let d = sample();
        // Large enough to be split across several parallel compression blocks.
        let big: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        write(d.path(), "data/big.txt", &big);
        let (m, a) = package(d.path(), &df(), &[]).unwrap();
        let bytes = a.read_all().unwrap();
        assert_eq!(bytes.len() as u64, a.len);
        assert!(a.len < m.tar_bytes, "compressed");
        // A single-member decoder must see the whole tar stream.
        let mut tar_stream = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut tar_stream)
            .unwrap();
        assert_eq!(tar_stream.len() as u64, m.tar_bytes);
        let mut h = Sha256::new();
        h.update(hash_prefix(&df()));
        h.update(&tar_stream);
        assert_eq!(hex(&h.finalize()), m.sha256);
        let mut ar = tar::Archive::new(tar_stream.as_slice());
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with('/'))
            .collect();
        assert_eq!(names, m.entries);
    }

    #[test]
    fn detects_files_changing_between_scan_and_compress() {
        let d = sample();
        let m = scan(d.path(), &df(), &[]).unwrap();
        write(d.path(), "main.py", "print('edited, same length?')\n");
        let e = compress(&m).unwrap_err();
        assert!(
            e.message.contains("changed") || e.message.contains("shrank"),
            "{}",
            e.message
        );
    }

    #[test]
    fn buildpacks_need_no_dockerfile_and_hash_differently() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "package.json", "{}\n");
        write(d.path(), "server.js", "console.log(1)\n");
        let bp = BuildStrategy::Buildpacks {
            builder: "gcr.io/buildpacks/builder:latest".into(),
        };
        let m = scan(d.path(), &bp, &[]).unwrap();
        assert_eq!(m.entries, ["package.json", "server.js"]);
        let other = BuildStrategy::Buildpacks {
            builder: "gcr.io/buildpacks/builder:google-22".into(),
        };
        assert_ne!(
            m.sha256,
            scan(d.path(), &other, &[]).unwrap().sha256,
            "builder is part of the hash"
        );
        write(d.path(), "Dockerfile", "FROM scratch\n");
        let with_df = scan(d.path(), &bp, &[]).unwrap();
        let as_df = scan(d.path(), &df(), &[]).unwrap();
        assert_ne!(with_df.sha256, as_df.sha256, "strategy is part of the hash");
    }
}
