//! Dispatcher for `warp skills` (PDX-117). All real logic lives in the
//! `warp_cli::skills` module; this file just plumbs the subcommand through to
//! it and prints the result.

use anyhow::Result;
use warp_cli::skills::{
    self, ContributeArgs, EmbeddingCache, GatewayConfig, GatewayEmbeddingClient, InfoArgs,
    InstallArgs, SearchArgs, SkillsCommand, SkillsPaths, SourcesIndex, SystemShell,
};
use warp_cli::GlobalOptions;

pub(crate) fn run(_global: GlobalOptions, command: SkillsCommand) -> Result<()> {
    let paths = SkillsPaths::from_env()?;
    match command {
        SkillsCommand::Search(args) => run_search(&paths, args),
        SkillsCommand::Install(args) => run_install(&paths, args),
        SkillsCommand::Update => run_update(&paths),
        SkillsCommand::Contribute(args) => run_contribute(&paths, args),
        SkillsCommand::List => run_list(&paths),
        SkillsCommand::Info(args) => run_info(&paths, args),
    }
}

fn run_search(paths: &SkillsPaths, args: SearchArgs) -> Result<()> {
    let cfg = GatewayConfig::from_env()?;
    let client = GatewayEmbeddingClient { cfg };
    let entries = skills::collect_registry(&paths.registry_clone)?;
    if entries.is_empty() {
        eprintln!(
            "skills registry is empty at {}; run `warp skills update` first",
            paths.registry_clone.display()
        );
        return Ok(());
    }
    let mut cache = EmbeddingCache::load(&paths.embeddings_cache)?;
    let refreshed = skills::refresh_embeddings(&mut cache, &entries, &client, args.refresh)?;
    if refreshed > 0 {
        cache.save(&paths.embeddings_cache)?;
    }
    // Embed the query separately. We re-use the same client.
    let query_vec = client_embed_one(&client, &args.query)?;
    let ranked = skills::rank_by_similarity(&query_vec, &cache, &entries, args.limit);
    for (name, score) in ranked {
        let entry = entries.iter().find(|e| e.name == name);
        let desc = entry.map(|e| e.description.as_str()).unwrap_or("");
        println!("{score:>6.3}  {name}  —  {desc}");
    }
    Ok(())
}

fn client_embed_one(
    client: &GatewayEmbeddingClient,
    query: &str,
) -> Result<Vec<f32>> {
    use warp_cli::skills::EmbeddingClient;
    let mut got = client.embed(&[query.to_string()])?;
    got.pop()
        .ok_or_else(|| anyhow::anyhow!("gateway returned no embedding for query"))
}

fn run_install(paths: &SkillsPaths, args: InstallArgs) -> Result<()> {
    let shell = SystemShell;
    let record = skills::install_skill(paths, &args.name, &args.repo, &shell)?;
    println!(
        "installed `{}` from {} into {}",
        record.name,
        record.repo,
        record.install_dir.display()
    );
    Ok(())
}

fn run_update(paths: &SkillsPaths) -> Result<()> {
    let shell = SystemShell;
    let updated = skills::update_all(paths, &shell)?;
    if updated.is_empty() {
        println!("nothing to update");
    } else {
        for name in updated {
            println!("updated {name}");
        }
    }
    Ok(())
}

fn run_contribute(paths: &SkillsPaths, args: ContributeArgs) -> Result<()> {
    let shell = SystemShell;
    let url = skills::contribute_skill(paths, &args, &shell)?;
    println!("opened PR: {url}");
    Ok(())
}

fn run_list(paths: &SkillsPaths) -> Result<()> {
    let installed = skills::list_installed(paths)?;
    if installed.is_empty() {
        println!("no community skills installed");
        return Ok(());
    }
    for s in installed {
        println!("{}\t{}\t{}", s.name, s.repo, s.install_dir.display());
    }
    Ok(())
}

fn run_info(paths: &SkillsPaths, args: InfoArgs) -> Result<()> {
    let body = skills::read_skill_info(paths, &args.name)?;
    println!("{body}");
    Ok(())
}

// Touch SourcesIndex so it stays in scope for users who only want the type
// re-exported from this module.
#[allow(dead_code)]
fn _index_type() -> SourcesIndex {
    SourcesIndex::default()
}
