# helm-starter-skills

Five hand-authored starter skills that exercise the patterns Helm projects actually need. Drop this bundle into a project's skills root (`~/.warp/skills/` for global, `<repo>/.agents/skills/` for repo-local) and the `crates/skills` loader will pick them up.

## What's in the box

| Skill ID                  | Pattern                                                                |
| ------------------------- | ---------------------------------------------------------------------- |
| `ai-gateway-routing`      | Route every model call through Cloudflare AI Gateway dynamic routes.   |
| `drizzle-migration`       | Define a schema, generate SQL, apply locally + remotely on D1.         |
| `wrangler-deploy`         | Deploy a Worker, set secrets from Doppler, wire the CI deploy job.     |
| `doppler-secret-fetch`    | Inject secrets via the Doppler CLI with cwd-based scoping.             |
| `warp-new-scaffolding`    | Scaffold a project from a template with variables and post-init hooks. |

Each skill lives in its own subdirectory as `SKILL.md` with YAML front matter that matches the `SkillFrontMatter` schema in `crates/skills/src/lib.rs` (`name`, `description`, `roles`, `tags`).

## Layout

```
templates/helm-starter-skills/
├── README.md
├── template.toml
├── ai-gateway-routing/SKILL.md
├── drizzle-migration/SKILL.md
├── wrangler-deploy/SKILL.md
├── doppler-secret-fetch/SKILL.md
└── warp-new-scaffolding/SKILL.md
```

## Installing

### Per-repo (recommended for project-specific patterns)

```bash
cp -R templates/helm-starter-skills/*/  .agents/skills/
```

The skills loader walks `<repo>/.agents/skills/` recursively for any `*.md` file, so each `SKILL.md` is picked up automatically. Repo-local skills override user-global skills with the same name.

### User-global (recommended for cross-repo patterns)

```bash
mkdir -p ~/.warp/skills
cp -R templates/helm-starter-skills/*/  ~/.warp/skills/
```

### Via `warp new`

This bundle is itself a Helm template, so it can be scaffolded:

```bash
warp new helm-starter-skills .agents/skills/
```

## Authoring conventions

These five skills follow a deliberate style — when you write more, match it:

1. **Specific behaviors over platitudes.** Good: "Block any PR that adds a `process.env.X` read without a corresponding `doppler secrets set X`." Bad: "Be careful with secrets."
2. **Front matter has `name`, `description`, `roles`, `tags`** — `name` matches the directory.
3. **Each skill ends with an "Anti-patterns" section** that lists the exact things to block in review. Reviewers and agents both consume this.
4. **Copy-pasteable code blocks**, not pseudocode. If a code block can't be pasted into a real shell or file, rewrite it.
5. **One skill = one pattern.** Don't bundle "all of Cloudflare" into a single skill. Split.

## Schema reference

Front matter is parsed by `serde_yaml` against `SkillFrontMatter`:

```rust
pub struct SkillFrontMatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub roles: Vec<String>,   // empty = applies to all roles
    pub tags: Vec<String>,
}
```

Runtime stats (`last_used`, `success_rate`, `total_tokens`, `tool_call_count`) and user overrides (`user_tags`, `user_description`) live in `SkillMetadata`, which is stored separately by `MetadataStore` — never in the markdown file. Don't put those fields in front matter.
