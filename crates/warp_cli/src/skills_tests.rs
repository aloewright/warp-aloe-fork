use super::*;
use std::cell::RefCell;
use std::fs;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Embedding ranking
// ---------------------------------------------------------------------------

struct StubEmbeddings {
    map: std::collections::HashMap<String, Vec<f32>>,
}

impl EmbeddingClient for StubEmbeddings {
    fn embed(&self, inputs: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(inputs.len());
        for input in inputs {
            let v = self
                .map
                .get(input)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no stub embedding for input: {input}"))?;
            out.push(v);
        }
        Ok(out)
    }
}

#[test]
fn cosine_similarity_orthogonal_is_zero() {
    let a = vec![1.0, 0.0, 0.0];
    let b = vec![0.0, 1.0, 0.0];
    assert!(cosine_similarity(&a, &b).abs() < 1e-6);
}

#[test]
fn cosine_similarity_identical_is_one() {
    let a = vec![0.5, 0.5, 0.5];
    assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
}

#[test]
fn rank_by_similarity_orders_drizzle_first_for_drizzle_query() {
    // Three skills, each pointed at a clearly distinct axis in the embedding
    // space. The drizzle query is aligned with the drizzle skill.
    let entries = vec![
        RegistryEntry {
            name: "drizzle-migration".into(),
            description: "drizzle d1 sql migrations".into(),
            source_dir: PathBuf::from("/tmp/a"),
        },
        RegistryEntry {
            name: "wrangler-deploy".into(),
            description: "deploy a worker".into(),
            source_dir: PathBuf::from("/tmp/b"),
        },
        RegistryEntry {
            name: "ai-gateway-routing".into(),
            description: "route llm calls".into(),
            source_dir: PathBuf::from("/tmp/c"),
        },
    ];

    let mut cache = EmbeddingCache::default();
    cache.upsert(EmbeddingEntry {
        name: "drizzle-migration".into(),
        fingerprint: 0,
        vector: vec![1.0, 0.0, 0.0],
    });
    cache.upsert(EmbeddingEntry {
        name: "wrangler-deploy".into(),
        fingerprint: 0,
        vector: vec![0.0, 1.0, 0.0],
    });
    cache.upsert(EmbeddingEntry {
        name: "ai-gateway-routing".into(),
        fingerprint: 0,
        vector: vec![0.0, 0.0, 1.0],
    });

    let query = vec![0.9, 0.1, 0.0]; // drizzle-aligned
    let ranked = rank_by_similarity(&query, &cache, &entries, 3);
    assert_eq!(ranked.len(), 3);
    assert_eq!(ranked[0].0, "drizzle-migration");
    assert!(ranked[0].1 > ranked[1].1);
}

#[test]
fn refresh_embeddings_only_embeds_stale_or_missing() {
    let entries = vec![
        RegistryEntry {
            name: "a".into(),
            description: "alpha".into(),
            source_dir: PathBuf::from("/tmp/a"),
        },
        RegistryEntry {
            name: "b".into(),
            description: "beta".into(),
            source_dir: PathBuf::from("/tmp/b"),
        },
    ];

    let mut map = std::collections::HashMap::new();
    map.insert("alpha".to_string(), vec![1.0, 0.0]);
    map.insert("beta".to_string(), vec![0.0, 1.0]);
    let client = StubEmbeddings { map };

    let mut cache = EmbeddingCache::default();
    let count = refresh_embeddings(&mut cache, &entries, &client, false).unwrap();
    assert_eq!(count, 2);
    assert_eq!(cache.entries.len(), 2);

    // Re-running without force is a no-op when fingerprints are unchanged.
    let count2 = refresh_embeddings(&mut cache, &entries, &client, false).unwrap();
    assert_eq!(count2, 0);

    // force=true re-embeds everything.
    let count3 = refresh_embeddings(&mut cache, &entries, &client, true).unwrap();
    assert_eq!(count3, 2);
}

#[test]
fn fingerprint_changes_when_text_changes() {
    let f1 = fingerprint("hello world");
    let f2 = fingerprint("hello world!");
    assert_ne!(f1, f2);
}

// ---------------------------------------------------------------------------
// Front-matter parsing for registry collection
// ---------------------------------------------------------------------------

#[test]
fn parse_name_and_description_reads_front_matter() {
    let raw = "---\n\
name: foo\n\
description: a foo skill\n\
tags: [a, b]\n\
---\n\
# heading\n\
body\n";
    let (name, desc) = parse_name_and_description(raw, Path::new("/tmp/foo"));
    assert_eq!(name, "foo");
    assert_eq!(desc, "a foo skill");
}

#[test]
fn parse_name_and_description_falls_back_to_dir_and_first_line() {
    let raw = "# Heading line\nbody\n";
    let (name, desc) = parse_name_and_description(raw, Path::new("/tmp/my-skill"));
    assert_eq!(name, "my-skill");
    assert_eq!(desc, "Heading line");
}

// ---------------------------------------------------------------------------
// Sources index round-trip
// ---------------------------------------------------------------------------

#[test]
fn sources_index_round_trips_through_disk() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("sources.json");
    let mut idx = SourcesIndex::default();
    idx.upsert(InstalledSkillSource {
        name: "drizzle-migration".into(),
        repo: DEFAULT_HELM_SKILLS_REPO.into(),
        source_dir: tmp.path().join("src"),
        install_dir: tmp.path().join("install"),
    });
    idx.save(&path).unwrap();

    let loaded = SourcesIndex::load(&path).unwrap();
    assert_eq!(loaded.skills.len(), 1);
    assert_eq!(loaded.skills[0].name, "drizzle-migration");
}

// ---------------------------------------------------------------------------
// install_skill: integration test via a fake `git` binary
// ---------------------------------------------------------------------------

/// Deterministic shell stub used by install/update tests. Records each
/// invocation and lets the caller pre-stage a fake checkout on disk.
struct ScriptedShell {
    paths: SkillsPaths,
    name: String,
    invocations: RefCell<Vec<String>>,
    repo_contents: String,
}

impl Shell for ScriptedShell {
    fn run(&self, program: &str, args: &[&str], _cwd: Option<&Path>) -> anyhow::Result<String> {
        let cmdline = format!("{} {}", program, args.join(" "));
        self.invocations.borrow_mut().push(cmdline.clone());

        // Match the canonical `git pull --depth 1 origin main` step in
        // install_skill — that's when the sparse checkout becomes available
        // on disk. Our stub just writes the expected file tree.
        if program == "git" && args.first() == Some(&"pull") {
            // The cwd is the source_dir. Recreate skills/<name>/SKILL.md
            // there.
            let source_dir = self.paths.registry_clone.join(&self.name);
            let dest = source_dir.join("skills").join(&self.name);
            fs::create_dir_all(&dest)?;
            fs::write(dest.join("SKILL.md"), &self.repo_contents)?;
        }
        Ok(String::new())
    }
}

#[test]
fn install_skill_clones_and_registers_drizzle_migration() {
    let tmp = TempDir::new().unwrap();
    let paths = SkillsPaths::rooted_at(tmp.path());
    let body = "---\n\
name: drizzle-migration\n\
description: drizzle d1 migrations\n\
tags: [drizzle, d1]\n\
---\n\
# drizzle-migration\n\
generated by test\n";
    let shell = ScriptedShell {
        paths: paths.clone(),
        name: "drizzle-migration".to_string(),
        invocations: RefCell::new(Vec::new()),
        repo_contents: body.to_string(),
    };

    let record = install_skill(
        &paths,
        "drizzle-migration",
        DEFAULT_HELM_SKILLS_REPO,
        &shell,
    )
    .expect("install");

    assert_eq!(record.name, "drizzle-migration");
    let installed_md = paths
        .install_root
        .join("drizzle-migration")
        .join("SKILL.md");
    assert!(
        installed_md.exists(),
        "expected installed SKILL.md at {}",
        installed_md.display()
    );
    let installed = fs::read_to_string(&installed_md).unwrap();
    assert!(installed.contains("name: drizzle-migration"));

    // The sources.json index now references the new skill.
    let idx = SourcesIndex::load(&paths.sources_index).unwrap();
    assert_eq!(idx.skills.len(), 1);
    assert_eq!(idx.skills[0].name, "drizzle-migration");

    // Sanity: we issued the expected sparse-checkout commands.
    let calls = shell.invocations.borrow();
    assert!(
        calls.iter().any(|c| c.contains("sparse-checkout init")),
        "missing sparse-checkout init in {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.contains("sparse-checkout set skills/drizzle-migration")),
        "missing sparse-checkout set in {calls:?}"
    );
}

#[test]
fn install_skill_rejects_path_traversal() {
    let tmp = TempDir::new().unwrap();
    let paths = SkillsPaths::rooted_at(tmp.path());
    let shell = ScriptedShell {
        paths: paths.clone(),
        name: "traversal".to_string(),
        invocations: RefCell::new(Vec::new()),
        repo_contents: String::new(),
    };
    let err = install_skill(&paths, "../etc", DEFAULT_HELM_SKILLS_REPO, &shell)
        .expect_err("expected validation error");
    assert!(err.to_string().contains("invalid skill name"));
}

#[test]
fn list_installed_returns_what_install_wrote() {
    let tmp = TempDir::new().unwrap();
    let paths = SkillsPaths::rooted_at(tmp.path());
    let body = "---\nname: x\ndescription: y\n---\nbody\n";
    let shell = ScriptedShell {
        paths: paths.clone(),
        name: "x".to_string(),
        invocations: RefCell::new(Vec::new()),
        repo_contents: body.to_string(),
    };
    install_skill(&paths, "x", DEFAULT_HELM_SKILLS_REPO, &shell).unwrap();

    let listed = list_installed(&paths).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "x");
}

#[test]
fn read_skill_info_returns_installed_body() {
    let tmp = TempDir::new().unwrap();
    let paths = SkillsPaths::rooted_at(tmp.path());
    let body = "---\nname: x\ndescription: y\n---\nthe body\n";
    let shell = ScriptedShell {
        paths: paths.clone(),
        name: "x".to_string(),
        invocations: RefCell::new(Vec::new()),
        repo_contents: body.to_string(),
    };
    install_skill(&paths, "x", DEFAULT_HELM_SKILLS_REPO, &shell).unwrap();

    let info = read_skill_info(&paths, "x").unwrap();
    assert!(info.contains("the body"));
}

#[test]
fn collect_registry_walks_skills_directory() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join("skills");
    fs::create_dir_all(skills.join("alpha")).unwrap();
    fs::write(
        skills.join("alpha").join("SKILL.md"),
        "---\nname: alpha\ndescription: alpha skill\n---\nbody\n",
    )
    .unwrap();
    fs::create_dir_all(skills.join("beta")).unwrap();
    fs::write(
        skills.join("beta").join("SKILL.md"),
        "---\nname: beta\ndescription: beta skill\n---\nbody\n",
    )
    .unwrap();

    let entries = collect_registry(tmp.path()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "alpha");
    assert_eq!(entries[1].name, "beta");
}
