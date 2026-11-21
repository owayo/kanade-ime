use anyhow::{Result, bail};
use kanade_ipc::{Manage, manage::Action};
mod dictionary;
/// Windows opens a console for this program when it is started from the Start
/// menu or Explorer. Releasing a console that no other process uses closes
/// it; a shell that started the program keeps its own.
#[cfg(all(feature = "native", windows))]
fn release_own_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(processes: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }
    let mut processes = [0u32; 2];
    unsafe {
        if GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) == 1 {
            FreeConsole();
        }
    }
}
fn main() -> Result<()> {
    std::panic::set_hook(Box::new(|_| eprintln!("kanade-ui: interface failure")));
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("assist") | Some("assist-json") => {
            use std::io::Read;
            let mut text = String::new();
            std::io::stdin()
                .take(64 * 1024 + 1)
                .read_to_string(&mut text)?;
            let findings =
                kanade_ui::writing::review(&text, args.iter().any(|a| a == "--whole-document"))?;
            #[cfg(all(feature = "native", any(windows, target_os = "macos")))]
            if args.get(1).map(String::as_str) == Some("assist") {
                return kanade_ui::native::review_window(findings);
            }
            println!("{}", serde_json::to_string_pretty(&findings)?);
            Ok(())
        }
        Some("reload-projects") => {
            kanade_ui::manage(Manage {
                action: Action::ReloadProjects as i32,
                ..Default::default()
            })?;
            Ok(())
        }
        Some("settings") => {
            if args.get(2).map(String::as_str) == Some("set") {
                use std::io::Read;
                let path = args
                    .get(3)
                    .ok_or_else(|| anyhow::anyhow!("settings JSON file required"))?;
                let mut bytes = vec![];
                std::fs::File::open(path)?
                    .take(65537)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > 65536 {
                    bail!("settings file exceeds its limit");
                }
                kanade_ui::save(&serde_json::from_slice(&bytes)?)?;
            }
            println!("{}", serde_json::to_string_pretty(&kanade_ui::settings()?)?);
            Ok(())
        }
        Some("dictionary") => dictionary::run(&args[2..]),
        Some("models") => kanade_ui::models::run(&args[2..]),
        Some("learning") => {
            let request = match args.get(2).map(String::as_str) {
                Some("delete") => Manage {
                    action: Action::DeleteLearning as i32,
                    record_id: args
                        .get(3)
                        .ok_or_else(|| anyhow::anyhow!("record ID required"))?
                        .parse()?,
                    ..Default::default()
                },
                Some("clear") => Manage {
                    action: Action::ClearLearning as i32,
                    ..Default::default()
                },
                Some("clear-mozc-conversion") => Manage {
                    action: Action::ClearMozcConversion as i32,
                    ..Default::default()
                },
                Some("clear-mozc-prediction") => Manage {
                    action: Action::ClearMozcPrediction as i32,
                    ..Default::default()
                },
                Some("block") => Manage {
                    action: Action::BlockReading as i32,
                    json: serde_json::to_string(
                        args.get(3)
                            .ok_or_else(|| anyhow::anyhow!("reading required"))?,
                    )?,
                    ..Default::default()
                },
                Some("list") | None => Manage {
                    action: Action::ListLearning as i32,
                    ..Default::default()
                },
                _ => bail!("unknown learning command"),
            };
            let result = kanade_ui::manage(request)?;
            if result.get("cleared") == Some(&serde_json::Value::Bool(false)) {
                bail!("learning could not be cleared: {}", result["reason"]);
            }
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        None => {
            #[cfg(all(feature = "native", any(windows, target_os = "macos")))]
            {
                #[cfg(windows)]
                release_own_console();
                kanade_ui::native::run()
            }
            #[cfg(not(all(feature = "native", any(windows, target_os = "macos"))))]
            {
                bail!("run the native settings window with mise run ui")
            }
        }
        _ => bail!(
            "usage: kanade-ui [settings|models [status|import FILE --accept-license --accept-local-processing|qualify|remove]|dictionary [list|edit|status|cancel|promote]|learning [list|delete ID|clear|clear-mozc-conversion|clear-mozc-prediction|block READING]]"
        ),
    }
}
