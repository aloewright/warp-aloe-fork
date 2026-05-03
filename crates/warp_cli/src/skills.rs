//! `warp skills` CLI surface — discover, install, update, and contribute
//! community skills sourced from the `aloewright/helm-skills` git repo.
//!
//! This module is intentionally self-contained: it does its own filesystem
//! work, shells out to `git` / `gh` for repo operations, and routes embedding
//! requests through the Cloudflare AI Gateway (`dynamic/ai_embed`) — never
//! through a provider SDK. See CLAUDE.md "AI Gateway routing" rule.
//!
//! Commands:
//!
//! * `warp skills search <query>` — semantic search over the registry by
//!   embedding the query and the skill descriptions, then ranking by cosine
//!   similarity. Embeddings are cached locally so repeat searches do not hit
//!   the gateway.
//! * `warp skills install <name>` — sparse-checkout `skills/<name>/` from
//!   `aloewright/helm-skills` into `~/.warp/skills/<name>` so the existing
//!   `crates/skills` loader picks it up.
//! * `warp skills update` — `git pull` on each installed skill's source.
//! * `warp skills contribute <local-path>` — opens a PR against
//!   `aloewright/helm-skills` containing the local skill's content. Shells
//!   out to `gh pr create`.
//! * `warp skills list` — lists installed skills + their source URLs.
//! * `warp skills info <name>` — prints the metadata block + body.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as SysCommand, Stdio};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

/// Default git remote for the community skills repo.
pub const DEFAULT_HELM_SKILLS_REPO: &str = "https://github.com/aloewright/helm-skills.git";
/// Owner used by `warp skills contribute` when creating a PR.
pub const DEFAULT_HELM_SKILLS_OWNER: &str = "aloewright";
/// Repo slug used by `warp skills contribute` when creating a PR.
pub const DEFAULT_HELM_SKILLS_NAME: &str = "helm-skills";

/// Top-level `warp skills` subcommand.
#[derive(Debug, Clone, Subcommand)]
pub enum SkillsCommand {
    /// Semantic search over the community skills registry.
    Search(SearchArgs),
    /// Install a skill into `~/.warp/skills/<name>` from the community repo.
    Install(InstallArgs),
    /// Pull the latest revision for every installed community skill.
    Update,
    /// Open a PR against `aloewright/helm-skills` with a local skill.
    Contribute(ContributeArgs),
    /// List installed skills with their source repo + URL.
    List,
    /// Print the metadata block + body of an installed skill.
    Info(InfoArgs),
}

/// Args for `warp skills search`.
#[derive(Debug, Clone, Args)]
pub struct SearchArgs {
    /// Free-text query — matched semantically against skill descriptions.
    pub query: String,

    /// Maximum number of results to print.
    #[arg(long = "limit", default_value_t = 5)]
    pub limit: usize,

    /// Force re-embedding of the registry instead of using the on-disk cache.
    #[arg(long = "refresh")]
    pub refresh: bool,
}

/// Args for `warp skills install`.
#[derive(Debug, Clone, Args)]
pub struct InstallArgs {
    /// Name of the skill to install (matches a directory under `skills/` in
    /// the community repo).
    pub name: String,

    /// Override the source git remote.
    #[arg(long = "repo", default_value = DEFAULT_HELM_SKILLS_REPO)]
    pub repo: String,
}

/// Args for `warp skills contribute`.
#[derive(Debug, Clone, Args)]
pub struct ContributeArgs {
    /// Path to the local skill directory or `SKILL.md` file to contribute.
    pub local_path: PathBuf,

    /// Branch name to use on the fork. Defaults to `contribute/<skill-name>`.
    #[arg(long = "branch")]
    pub branch: Option<String>,

    /// Title of the PR. Defaults to `Add skill: <name>`.
    #[arg(long = "title")]
    pub title: Option<String>,
}

/// Args for `warp skills info`.
#[derive(Debug, Clone, Args)]
pub struct InfoArgs {
    /// Name of the installed skill to inspect.
    pub name: String,
}

/// Telemetry-friendly discriminant for the subcommand.
pub fn telemetry_label(cmd: &SkillsCommand) -> &'static str {
    match cmd {
        SkillsCommand::Search(_) => "search",
        SkillsCommand::Install(_) => "install",
        SkillsCommand::Update => "update",
        SkillsCommand::Contribute(_) => "contribute",
        SkillsCommand::List => "list",
        SkillsCommand::Info(_) => "info",
    }
}

/// Resolved set of paths used by the skills CLI.
///
/// All paths are derived from the user's home directory; `for_test` allows
/// integration tests to redirect them at a temp directory.
#[derive(Debug, Clone)]
pub struct SkillsPaths {
    /// `~/.warp/skills` — install root + the directory the agent runner reads.
    pub install_root: PathBuf,
    /// `~/.warp/skills-cache` — embedding cache + per-skill source clone.
    pub cache_root: PathBuf,
    /// `~/.warp/skills-cache/embeddings.bin` — serialised embedding cache.
    pub embeddings_cache: PathBuf,
    /// `~/.warp/skills-cache/sources.json` — installed-skill provenance index.
    pub sources_index: PathBuf,
    /// `~/.warp/skills-cache/registry/` — bare-ish clone of the community
    /// repo used for semantic search.
    pub registry_clone: PathBuf,
}

impl SkillsPaths {
    /// Build paths rooted at the user's home directory (`$WARP_HOME` /
    /// `$HOME`).
    pub fn from_env() -> anyhow::Result<Self> {
        let home = warp_home()?;
        Ok(Self::rooted_at(&home))
    }

    /// Build paths rooted at an arbitrary directory. Used by tests.
    pub fn rooted_at(home: &Path) -> Self {
        let install_root = home.join(".warp").join("skills");
        let cache_root = home.join(".warp").join("skills-cache");
        let embeddings_cache = cache_root.join("embeddings.bin");
        let sources_index = cache_root.join("sources.json");
        let registry_clone = cache_root.join("registry");
        Self {
            install_root,
            cache_root,
            embeddings_cache,
            sources_index,
            registry_clone,
        }
    }
}

fn warp_home() -> anyhow::Result<PathBuf> {
    if let Ok(custom) = std::env::var("WARP_HOME") {
        if !custom.is_empty() {
            return Ok(PathBuf::from(custom));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    #[cfg(windows)]
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.is_empty() {
            return Ok(PathBuf::from(profile));
        }
    }
    Err(anyhow::anyhow!(
        "could not determine home directory; set $HOME or $WARP_HOME"
    ))
}

/// One installed-skill record persisted to `sources.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledSkillSource {
    /// Skill name (matches the install directory).
    pub name: String,
    /// Git remote the skill was cloned from.
    pub repo: String,
    /// Path to the cloned source under `skills-cache/registry/<name>`.
    pub source_dir: PathBuf,
    /// Path to the installed copy under `~/.warp/skills/<name>`.
    pub install_dir: PathBuf,
}

/// On-disk format for the source index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcesIndex {
    pub skills: Vec<InstalledSkillSource>,
}

impl SourcesIndex {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    pub fn upsert(&mut self, source: InstalledSkillSource) {
        if let Some(existing) = self.skills.iter_mut().find(|s| s.name == source.name) {
            *existing = source;
        } else {
            self.skills.push(source);
        }
    }

    pub fn get(&self, name: &str) -> Option<&InstalledSkillSource> {
        self.skills.iter().find(|s| s.name == name)
    }
}

// -----------------------------------------------------------------------------
// Embedding cache
// -----------------------------------------------------------------------------

/// One cached embedding entry. The fingerprint is a short hash of the source
/// text so we can detect when a skill's description has changed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmbeddingEntry {
    pub name: String,
    pub fingerprint: u64,
    pub vector: Vec<f32>,
}

/// On-disk format for the embedding cache. Uses `bincode` is overkill for a
/// small file — JSON is plenty and stays human-readable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbeddingCache {
    pub entries: Vec<EmbeddingEntry>,
}

impl EmbeddingCache {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&EmbeddingEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn upsert(&mut self, entry: EmbeddingEntry) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.name == entry.name) {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }
}

/// Small stable hash used as an embedding-cache fingerprint.
pub fn fingerprint(text: &str) -> u64 {
    // FNV-1a 64-bit. Stable across runs and platforms — good enough to detect
    // when a description changes between cache writes.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Cosine similarity between two equal-length vectors. Returns 0.0 when
/// either vector is zero-norm (no useful direction to measure).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot: f32 = 0.0;
    let mut na: f32 = 0.0;
    let mut nb: f32 = 0.0;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// One row in the search registry: a skill discovered under the registry
/// clone (or any other source dir).
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryEntry {
    pub name: String,
    pub description: String,
    pub source_dir: PathBuf,
}

/// Walk a checked-out community-skills tree and return one [`RegistryEntry`]
/// per skill directory. Skills are expected to live under `<root>/skills/<name>/SKILL.md`.
pub fn collect_registry(root: &Path) -> anyhow::Result<Vec<RegistryEntry>> {
    let skills_dir = root.join("skills");
    if !skills_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&skills_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let skill_md = dir.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }
        let raw = fs::read_to_string(&skill_md)?;
        let (name, description) = parse_name_and_description(&raw, &dir);
        out.push(RegistryEntry {
            name,
            description,
            source_dir: dir,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Pull `name` and `description` out of the YAML front matter of a skill,
/// falling back to the directory name + first non-blank body line.
pub fn parse_name_and_description(raw: &str, dir: &Path) -> (String, String) {
    let stripped = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let (front_matter, body) = match stripped.strip_prefix("---\n") {
        Some(rest) => match rest.find("\n---") {
            Some(end) => (Some(&rest[..end]), &rest[end + 4..]),
            None => (None, raw),
        },
        None => (None, raw),
    };
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    if let Some(fm) = front_matter {
        for line in fm.lines() {
            if let Some(rest) = line.strip_prefix("name:") {
                name = Some(rest.trim().trim_matches('"').to_string());
            } else if let Some(rest) = line.strip_prefix("description:") {
                description = Some(rest.trim().trim_matches('"').to_string());
            }
        }
    }
    let name = name.unwrap_or_else(|| {
        dir.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string()
    });
    let description = description.unwrap_or_else(|| {
        body.lines()
            .map(|l| l.trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string()
    });
    (name, description)
}

/// Rank candidates against `query_embedding` using cosine similarity. Returns
/// `(name, score)` pairs sorted descending by score.
pub fn rank_by_similarity(
    query_embedding: &[f32],
    cache: &EmbeddingCache,
    candidates: &[RegistryEntry],
    limit: usize,
) -> Vec<(String, f32)> {
    let mut scored: Vec<(String, f32)> = candidates
        .iter()
        .filter_map(|c| {
            cache
                .get(&c.name)
                .map(|e| (c.name.clone(), cosine_similarity(query_embedding, &e.vector)))
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

// -----------------------------------------------------------------------------
// Cloudflare AI Gateway embedding client
// -----------------------------------------------------------------------------

/// Configuration sourced from env for the AI Gateway embedding call.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub account_id: String,
    pub gateway_id: String,
    pub token: String,
}

impl GatewayConfig {
    /// Build from `CF_ACCOUNT_ID`, `CF_GATEWAY_ID` (defaults to `x`), and
    /// `CF_AIG_TOKEN`. Returns an error if `CF_ACCOUNT_ID` or `CF_AIG_TOKEN`
    /// are missing.
    pub fn from_env() -> anyhow::Result<Self> {
        let account_id = std::env::var("CF_ACCOUNT_ID")
            .map_err(|_| anyhow::anyhow!("CF_ACCOUNT_ID is not set"))?;
        let gateway_id =
            std::env::var("CF_GATEWAY_ID").unwrap_or_else(|_| "x".to_string());
        let token = std::env::var("CF_AIG_TOKEN")
            .map_err(|_| anyhow::anyhow!("CF_AIG_TOKEN is not set"))?;
        Ok(Self {
            account_id,
            gateway_id,
            token,
        })
    }

    /// URL for the OpenAI-compatible embeddings endpoint.
    pub fn embeddings_url(&self) -> String {
        format!(
            "https://gateway.ai.cloudflare.com/v1/{}/{}/compat/embeddings",
            self.account_id, self.gateway_id
        )
    }
}

/// Trait so tests can stub the gateway out without making real HTTP calls.
pub trait EmbeddingClient {
    fn embed(&self, inputs: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

#[cfg(not(target_family = "wasm"))]
pub struct GatewayEmbeddingClient {
    pub cfg: GatewayConfig,
}

#[cfg(not(target_family = "wasm"))]
impl EmbeddingClient for GatewayEmbeddingClient {
    fn embed(&self, inputs: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({
            "model": "dynamic/ai_embed",
            "input": inputs,
        });
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let resp = client
            .post(self.cfg.embeddings_url())
            .header("Content-Type", "application/json")
            .header(
                "cf-aig-authorization",
                format!("Bearer {}", self.cfg.token),
            )
            .header("cf-aig-zdr", "true")
            .json(&body)
            .send()?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(anyhow::anyhow!(
                "gateway embedding call failed: {} {}",
                status,
                text
            ));
        }
        let json: serde_json::Value = resp.json()?;
        let data = json
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| anyhow::anyhow!("gateway response missing 'data' array"))?;
        let mut out = Vec::with_capacity(data.len());
        for item in data {
            let v = item
                .get("embedding")
                .and_then(|e| e.as_array())
                .ok_or_else(|| anyhow::anyhow!("response item missing 'embedding'"))?;
            let mut vec_f32 = Vec::with_capacity(v.len());
            for n in v {
                vec_f32.push(n.as_f64().unwrap_or(0.0) as f32);
            }
            out.push(vec_f32);
        }
        Ok(out)
    }
}

// -----------------------------------------------------------------------------
// Command runner — minimal git/gh shim. Tests inject a fake binary on $PATH.
// -----------------------------------------------------------------------------

/// Trait for shelling out to external tools. Allows tests to stub git/gh.
pub trait Shell {
    fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> anyhow::Result<String>;
}

/// Production [`Shell`] implementation that calls the real binaries.
pub struct SystemShell;

impl Shell for SystemShell {
    fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> anyhow::Result<String> {
        let mut cmd = SysCommand::new(program);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::null());
        let output = cmd.output().map_err(|e| {
            anyhow::anyhow!("failed to run `{} {}`: {}", program, args.join(" "), e)
        })?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "`{} {}` exited with status {}: {}",
                program,
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

// -----------------------------------------------------------------------------
// Install / update / list / info / contribute
// -----------------------------------------------------------------------------

/// Perform a sparse-checkout of `skills/<name>/` from `repo` into the
/// registry clone, then copy the result into the install dir. Returns the
/// new [`InstalledSkillSource`] record.
pub fn install_skill(
    paths: &SkillsPaths,
    name: &str,
    repo: &str,
    shell: &dyn Shell,
) -> anyhow::Result<InstalledSkillSource> {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return Err(anyhow::anyhow!("invalid skill name: {name}"));
    }
    fs::create_dir_all(&paths.cache_root)?;
    let source_dir = paths.registry_clone.join(name);
    if source_dir.exists() {
        let _ = fs::remove_dir_all(&source_dir);
    }
    fs::create_dir_all(&source_dir)?;

    // Sparse-checkout pattern: only the per-skill directory.
    shell.run("git", &["init", "-q"], Some(&source_dir))?;
    shell.run(
        "git",
        &["remote", "add", "origin", repo],
        Some(&source_dir),
    )?;
    shell.run(
        "git",
        &["sparse-checkout", "init", "--cone"],
        Some(&source_dir),
    )?;
    let pattern = format!("skills/{name}");
    shell.run(
        "git",
        &["sparse-checkout", "set", &pattern],
        Some(&source_dir),
    )?;
    shell.run(
        "git",
        &["pull", "--depth", "1", "origin", "main"],
        Some(&source_dir),
    )?;

    let checked_out = source_dir.join("skills").join(name);
    if !checked_out.exists() {
        return Err(anyhow::anyhow!(
            "skill `{name}` not found in {repo} after sparse checkout"
        ));
    }

    let install_dir = paths.install_root.join(name);
    if install_dir.exists() {
        fs::remove_dir_all(&install_dir)?;
    }
    fs::create_dir_all(&paths.install_root)?;
    copy_dir_recursive(&checked_out, &install_dir)?;

    let mut index = SourcesIndex::load(&paths.sources_index)?;
    let record = InstalledSkillSource {
        name: name.to_string(),
        repo: repo.to_string(),
        source_dir,
        install_dir,
    };
    index.upsert(record.clone());
    index.save(&paths.sources_index)?;
    Ok(record)
}

/// Copy the contents of `src` into `dst`, creating `dst` if necessary.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut input = fs::File::open(&from)?;
            let mut buf = Vec::new();
            input.read_to_end(&mut buf)?;
            let mut output = fs::File::create(&to)?;
            output.write_all(&buf)?;
        }
    }
    Ok(())
}

/// Update every installed skill by running `git pull` in its source clone
/// and copying the refreshed contents back over the install dir.
pub fn update_all(paths: &SkillsPaths, shell: &dyn Shell) -> anyhow::Result<Vec<String>> {
    let index = SourcesIndex::load(&paths.sources_index)?;
    let mut updated = Vec::new();
    for source in &index.skills {
        if !source.source_dir.exists() {
            continue;
        }
        shell.run("git", &["pull", "--ff-only"], Some(&source.source_dir))?;
        let checked_out = source.source_dir.join("skills").join(&source.name);
        if checked_out.exists() {
            if source.install_dir.exists() {
                fs::remove_dir_all(&source.install_dir)?;
            }
            copy_dir_recursive(&checked_out, &source.install_dir)?;
            updated.push(source.name.clone());
        }
    }
    Ok(updated)
}

/// Open a PR against `aloewright/helm-skills` containing the local skill at
/// `local_path`. Shells out to `gh pr create`. The local path may be either
/// a directory containing `SKILL.md` or the `SKILL.md` itself.
pub fn contribute_skill(
    paths: &SkillsPaths,
    args: &ContributeArgs,
    shell: &dyn Shell,
) -> anyhow::Result<String> {
    let (skill_dir, skill_md) = if args.local_path.is_dir() {
        let md = args.local_path.join("SKILL.md");
        if !md.exists() {
            return Err(anyhow::anyhow!(
                "{} does not contain SKILL.md",
                args.local_path.display()
            ));
        }
        (args.local_path.clone(), md)
    } else if args.local_path.is_file() {
        let parent = args.local_path.parent().ok_or_else(|| {
            anyhow::anyhow!("local skill path has no parent directory")
        })?;
        (parent.to_path_buf(), args.local_path.clone())
    } else {
        return Err(anyhow::anyhow!(
            "local path {} not found",
            args.local_path.display()
        ));
    };
    let raw = fs::read_to_string(&skill_md)?;
    let (name, _) = parse_name_and_description(&raw, &skill_dir);

    // Stage a fresh working clone of the helm-skills repo, drop the new
    // skill directory in, and let `gh pr create` push + open the PR.
    fs::create_dir_all(&paths.cache_root)?;
    let staging = paths.cache_root.join(format!("contribute-{name}"));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    shell.run(
        "gh",
        &[
            "repo",
            "clone",
            &format!("{}/{}", DEFAULT_HELM_SKILLS_OWNER, DEFAULT_HELM_SKILLS_NAME),
            staging.to_string_lossy().as_ref(),
        ],
        None,
    )?;
    let branch = args
        .branch
        .clone()
        .unwrap_or_else(|| format!("contribute/{name}"));
    shell.run("git", &["checkout", "-b", &branch], Some(&staging))?;

    let dest = staging.join("skills").join(&name);
    fs::create_dir_all(dest.parent().unwrap())?;
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
    }
    copy_dir_recursive(&skill_dir, &dest)?;

    shell.run("git", &["add", "-A"], Some(&staging))?;
    shell.run(
        "git",
        &[
            "commit",
            "-m",
            &format!("contribute(skills): add {name}"),
        ],
        Some(&staging),
    )?;
    shell.run("git", &["push", "-u", "origin", &branch], Some(&staging))?;

    let title = args
        .title
        .clone()
        .unwrap_or_else(|| format!("Add skill: {name}"));
    let body = format!(
        "Adds the `{name}` skill, contributed via `warp skills contribute`.\n\n\
         Source: `{src}`",
        src = skill_md.display()
    );
    let url = shell.run(
        "gh",
        &[
            "pr", "create", "--title", &title, "--body", &body, "--base", "main", "--head",
            &branch,
        ],
        Some(&staging),
    )?;
    Ok(url.trim().to_string())
}

/// List every installed skill with its provenance.
pub fn list_installed(paths: &SkillsPaths) -> anyhow::Result<Vec<InstalledSkillSource>> {
    let index = SourcesIndex::load(&paths.sources_index)?;
    Ok(index.skills)
}

/// Read the metadata block + body of an installed skill.
pub fn read_skill_info(paths: &SkillsPaths, name: &str) -> anyhow::Result<String> {
    let path = paths.install_root.join(name).join("SKILL.md");
    if !path.exists() {
        return Err(anyhow::anyhow!(
            "skill `{name}` is not installed (looked for {})",
            path.display()
        ));
    }
    Ok(fs::read_to_string(path)?)
}

/// Refresh the embedding cache: for every entry in the registry whose
/// description fingerprint has changed (or is missing), embed it via
/// `client` and update the cache.
pub fn refresh_embeddings(
    cache: &mut EmbeddingCache,
    entries: &[RegistryEntry],
    client: &dyn EmbeddingClient,
    force: bool,
) -> anyhow::Result<usize> {
    let mut to_embed: Vec<(String, String, u64)> = Vec::new();
    for entry in entries {
        let fp = fingerprint(&entry.description);
        let stale = match cache.get(&entry.name) {
            Some(existing) => existing.fingerprint != fp,
            None => true,
        };
        if force || stale {
            to_embed.push((entry.name.clone(), entry.description.clone(), fp));
        }
    }
    if to_embed.is_empty() {
        return Ok(0);
    }
    let inputs: Vec<String> = to_embed.iter().map(|(_, d, _)| d.clone()).collect();
    let vectors = client.embed(&inputs)?;
    if vectors.len() != to_embed.len() {
        return Err(anyhow::anyhow!(
            "gateway returned {} vectors for {} inputs",
            vectors.len(),
            to_embed.len()
        ));
    }
    let count = to_embed.len();
    for ((name, _, fp), vector) in to_embed.into_iter().zip(vectors.into_iter()) {
        cache.upsert(EmbeddingEntry {
            name,
            fingerprint: fp,
            vector,
        });
    }
    Ok(count)
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
