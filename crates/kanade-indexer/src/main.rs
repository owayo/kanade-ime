use anyhow::{Context, Result, bail, ensure};
use kanade_indexer::{Project, Registry, scan, valid_id};
use std::path::PathBuf;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let data = PathBuf::from(
        std::env::var_os("KANADE_DATA_DIR")
            .context("set KANADE_DATA_DIR to the application data directory")?,
    )
    .join("kanade/projects");
    let mut registry = Registry::load(&data)?;
    if args.first().map(String::as_str) == Some("list") {
        ensure!(args.len() == 1, "list accepts no arguments");
        let projects: Vec<_> = registry.projects.iter().map(|p| serde_json::json!({
            "id":p.id, "root":p.root, "apps":p.apps, "enabled":p.enabled, "readings":p.readings
        })).collect();
        println!("{}", serde_json::to_string_pretty(&projects)?);
        return Ok(());
    }
    let id = args
        .get(1)
        .context("usage: kanade-indexer list | approve|index|watch|remove|enable|disable|apps|reading|clear-reading ID [ARGS...]")?;
    ensure!(valid_id(id), "invalid project ID");
    match args.first().map(String::as_str) {
        Some("approve") => {
            ensure!(
                !registry.projects.iter().any(|p| &p.id == id) && registry.projects.len() < 32,
                "project already exists or limit reached"
            );
            let root =
                PathBuf::from(args.get(2).context("project folder required")?).canonicalize()?;
            ensure!(root.is_dir(), "project root must be a directory");
            let apps = args.get(3..).context("application IDs required")?.to_vec();
            validate_apps(&apps)?;
            registry.projects.push(Project {
                id: id.clone(),
                approval: u64::try_from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos(),
                )?
                .max(
                    registry
                        .projects
                        .iter()
                        .map(|p| p.approval)
                        .max()
                        .unwrap_or(0)
                        .checked_add(1)
                        .context("approval exhausted")?,
                ),
                monitoring: false,
                directory_binding: Some(kanade_indexer::directory_binding(&root)?),
                root,
                apps,
                enabled: true,
                readings: Default::default(),
            });
            registry.save(&data)?;
            println!("project approved");
        }
        Some("index") => {
            let binary = std::env::var_os("KANADE_ASTRO_SIGHT")
                .map(PathBuf::from)
                .unwrap_or_else(|| "astro-sight".into());
            let changed = args.get(2..).filter(|a| !a.is_empty()).map(|a| a.to_vec());
            println!(
                "indexed {} terms",
                scan::index(&data, id, &binary, changed)?
            );
        }
        Some("watch") => {
            ensure!(args.len() == 2, "watch accepts one approved project ID");
            let binary = std::env::var_os("KANADE_ASTRO_SIGHT")
                .map(PathBuf::from)
                .unwrap_or_else(|| "astro-sight".into());
            kanade_indexer::watch::run(&data, id, &binary)?;
        }
        Some("remove") => {
            ensure!(args.len() == 2, "remove accepts one approved project ID");
            kanade_indexer::remove_project(&data, id)?;
            println!("project removed");
        }
        Some("enable" | "disable" | "apps" | "reading" | "clear-reading") => {
            let project = registry
                .projects
                .iter_mut()
                .find(|p| &p.id == id)
                .context("project has not been approved")?;
            let operation = args[0].as_str();
            match operation {
                "enable" | "disable" => {
                    ensure!(
                        args.len() == 2,
                        "enable/disable accepts one approved project ID"
                    );
                    project.enabled = operation == "enable";
                }
                "apps" => {
                    let apps = args.get(2..).context("application IDs required")?;
                    validate_apps(apps)?;
                    project.apps = apps.to_vec();
                }
                "reading" => {
                    ensure!(args.len() == 4, "reading requires ID IDENTIFIER KANA");
                    ensure!(
                        kanade_indexer::valid_term(&kanade_indexer::Term {
                            value: args[2].clone(),
                            reading: args[3].clone()
                        }),
                        "invalid identifier or kana reading"
                    );
                    ensure!(
                        project.readings.len() < 10_000 || project.readings.contains_key(&args[2]),
                        "too many reading overrides"
                    );
                    project.readings.insert(args[2].clone(), args[3].clone());
                }
                "clear-reading" => {
                    ensure!(args.len() == 3, "clear-reading requires ID IDENTIFIER");
                    project.readings.remove(&args[2]);
                }
                _ => unreachable!(),
            }
            registry.save(&data)?;
            if matches!(operation, "reading" | "clear-reading") {
                println!("reading override updated; index again or keep watch running to apply it");
            } else {
                println!("project settings updated");
            }
        }
        _ => bail!("unknown project operation"),
    }
    Ok(())
}
fn validate_apps(apps: &[String]) -> Result<()> {
    ensure!(
        kanade_indexer::management::valid_apps(apps),
        "one to 64 application IDs are required"
    );
    Ok(())
}
