use anyhow::{Result, anyhow, bail};
use kanade_ipc::{Manage, manage::Action};
use std::io::Read;

fn json(path: &str) -> Result<String> {
    let mut bytes = vec![];
    std::fs::File::open(path)?
        .take(65537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        bail!("dictionary request exceeds its limit");
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid dictionary request JSON"))?;
    if !value.is_object() {
        bail!("dictionary request must be an object");
    }
    Ok(serde_json::to_string(&value)?)
}
fn id(value: &str) -> Result<u64> {
    value
        .parse()
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| anyhow!("a nonzero ID is required"))
}
pub fn run(args: &[String]) -> Result<()> {
    let parts: Vec<&str> = args.iter().map(String::as_str).collect();
    let request = match parts.as_slice() {
        [] | ["list"] => Manage {
            action: Action::ListMozcDictionary as i32,
            ..Default::default()
        },
        ["list", offset] => Manage {
            action: Action::ListMozcDictionary as i32,
            offset: offset
                .parse()
                .map_err(|_| anyhow!("invalid dictionary page offset"))?,
            ..Default::default()
        },
        ["edit", path] => Manage {
            action: Action::EditMozcDictionary as i32,
            json: json(path)?,
            ..Default::default()
        },
        ["status"] => Manage {
            action: Action::DictionaryJobStatus as i32,
            ..Default::default()
        },
        ["status", job] => Manage {
            action: Action::DictionaryJobStatus as i32,
            job_id: id(job)?,
            ..Default::default()
        },
        ["cancel", job] => Manage {
            action: Action::CancelDictionaryJob as i32,
            job_id: id(job)?,
            ..Default::default()
        },
        ["promote", record, path] => Manage {
            action: Action::PromoteLearning as i32,
            record_id: id(record)?,
            json: json(path)?,
            ..Default::default()
        },
        _ => bail!(
            "usage: kanade-ui dictionary [list [OFFSET]|edit FILE|status [JOB_ID]|cancel JOB_ID|promote RECORD_ID FILE]"
        ),
    };
    let result = kanade_ui::manage(request)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
