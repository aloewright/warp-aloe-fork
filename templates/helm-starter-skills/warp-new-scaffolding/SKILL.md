---
name: warp-new-scaffolding
description: Scaffold a new project from a Helm template using `warp new <template>`. Covers variable prompts, `{{placeholder}}` substitution, post-init hooks, and what to do when a hook fails.
roles: []
tags: [warp, templates, scaffolding, cli, helm]
---

# warp-new-scaffolding

`warp new <template>` instantiates a project from a template directory. The engine copies files, substitutes `{{variable}}` placeholders in paths and contents, and then runs declared post-init hooks (e.g. `git init`, `npm install`, `doppler setup`).

Source crates: `crates/templates` (loader + substitution), `crates/template_hooks` (hook runner), `crates/warp_cli/src/new.rs` (CLI entry point).

## When to use

- Bootstrapping a new MCP server, Worker, or full-stack project from a vetted starter.
- Adding a new template to `templates/` for others to use.

## Discovering templates

```bash
warp new --list
```

Templates are loaded from:

- `templates/` in the warp repo (built-ins like `helm-mcp-project`, `helm-starter-skills`).
- `~/.warp/templates/` for user-installed templates.

## Instantiating

```bash
warp new helm-mcp-project ./my-mcp
```

The CLI:

1. Loads `templates/helm-mcp-project/template.toml`.
2. Prompts for each variable in `[[variables]]` whose `required = true` and has no default.
3. Walks the template tree, substituting `{{variable_name}}` in **both file paths and file contents**.
4. Writes the result to `./my-mcp`.
5. Runs each command in `[hooks].post_init` from `./my-mcp` in order. If any command exits non-zero, scaffolding aborts and the partial directory is left in place for inspection.

Pass values non-interactively:

```bash
warp new helm-mcp-project ./my-mcp \
  --var project_name=my-mcp \
  --var author="Ada Lovelace"
```

## template.toml shape

```toml
name = "helm-worker"
description = "Cloudflare Worker with D1, AI Gateway, and Doppler-backed secrets."
version = "0.1.0"
author = "helm-team"

[[variables]]
name = "project_name"
description = "Slug used for the Worker name and the directory."
required = true

[[variables]]
name = "author"
description = "Author name for package.json."
default = ""
required = false

[[variables]]
name = "d1_database_name"
description = "D1 binding database_name."
default = "{{project_name}}-db"
required = false

[hooks]
post_init = [
  "git init",
  "npm install",
  "doppler setup --scope . --project {{project_name}} --config dev --no-interactive",
  "git add -A",
  "git commit -m 'chore: scaffolded from helm-worker template'",
]
```

Variable defaults can themselves contain `{{placeholders}}` — they are resolved in declaration order.

## Using placeholders inside template files

In any text file under the template tree:

```jsonc
// package.json
{
  "name": "{{project_name}}",
  "author": "{{author}}",
  "scripts": {
    "dev": "doppler run --scope . -- wrangler dev",
    "deploy": "doppler run --scope . -- wrangler deploy"
  }
}
```

Placeholders also work in **paths**:

```
templates/helm-worker/
  src/
    {{project_name}}.ts
```

After instantiation that becomes `src/my-mcp.ts`.

Unknown placeholders are left intact — the engine does not fail on them. This lets a parent template embed a nested `{{placeholder}}` that a downstream tool resolves.

## When a post-init hook fails

`warp new` prints the failing command and its exit code, then leaves the partial directory. Common fixes:

- `git init` failed because the user is already inside a git repo with `--no-such-init` policy → cd into the new dir and run hooks manually.
- `npm install` failed → run it manually after fixing the underlying network / auth issue. The scaffolded files are correct.
- `doppler setup` failed because `doppler login` was never run → run `doppler login --yes --scope .` and retry the hook.

Hooks are intentionally **not** retried automatically — silent retries would mask broken templates.

## Authoring a new template

1. Create `templates/<your-template>/template.toml` with `name`, `description`, `version`, and any `[[variables]]`.
2. Drop the source files into `templates/<your-template>/`. Use `{{placeholder}}` syntax wherever a value should be substituted.
3. Add `[hooks].post_init` entries for anything that must run after copy (init git, install deps, set up Doppler).
4. Test with `warp new <your-template> /tmp/test-out --var ...` and read the diff.
5. Add the template name to the integration test in `crates/templates/src/lib_tests.rs` if there is a smoke-test list.

## Anti-patterns

- Hardcoding the project name in `package.json` or `wrangler.toml` instead of using `{{project_name}}`. Block the PR — it defeats the template.
- Putting real secrets in template files. Use `doppler setup` in `post_init` instead.
- Adding a `post_init` hook that requires interactive input (`npm init` without `-y`, `git config` prompts). Hooks must be non-interactive — pass `--yes`, `--no-interactive`, redirect stdin from `/dev/null`, etc.
- Adding a hook that talks to the network without an offline fallback path. If `npm install` is in `post_init`, the README must say so — don't surprise users.
- Forgetting to declare a `[[variables]]` entry for a `{{placeholder}}` used in the template — the engine will leave the literal `{{name}}` in the output. Block the PR.
