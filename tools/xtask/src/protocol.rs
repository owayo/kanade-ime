use super::*;
use prost::Message;
use prost_types::{DescriptorProto, EnumDescriptorProto, FileDescriptorSet};
fn signatures(set: &FileDescriptorSet) -> BTreeMap<String, String> {
    fn enumeration(prefix: &str, value: &EnumDescriptorProto, out: &mut BTreeMap<String, String>) {
        let name = format!("{prefix}.{}", value.name.as_deref().unwrap_or(""));
        for entry in &value.value {
            out.insert(
                format!("{name}.{}", entry.number.unwrap_or(0)),
                entry.name.clone().unwrap_or_default(),
            );
        }
    }
    fn message(prefix: &str, value: &DescriptorProto, out: &mut BTreeMap<String, String>) {
        let name = format!("{prefix}.{}", value.name.as_deref().unwrap_or(""));
        out.insert(name.clone(), "message".into());
        for field in &value.field {
            out.insert(
                format!("{name}.{}", field.number.unwrap_or(0)),
                format!(
                    "{:?}",
                    (
                        &field.name,
                        &field.r#type,
                        &field.type_name,
                        &field.label,
                        &field.default_value,
                        &field.oneof_index
                    )
                ),
            );
        }
        for nested in &value.nested_type {
            message(&name, nested, out);
        }
        for value in &value.enum_type {
            enumeration(&name, value, out);
        }
    }
    let mut out = BTreeMap::new();
    for file in &set.file {
        let package = file.package.as_deref().unwrap_or("");
        for value in &file.message_type {
            message(package, value, &mut out);
        }
        for value in &file.enum_type {
            enumeration(package, value, &mut out);
        }
    }
    out
}
pub fn check(root: &Path, candidate: &Path) -> Result<()> {
    let src = candidate.join("src");
    let temporary = root.join("target/proto-probe");
    fs::create_dir_all(&temporary)?;
    let descriptor = temporary.join("candidate.bin");
    let compiler = protoc_bin_vendored::protoc_bin_path()?;
    let mut command = Command::new(compiler);
    command
        .arg(format!("--proto_path={}", src.display()))
        .arg(format!("--descriptor_set_out={}", descriptor.display()))
        .arg("--include_imports");
    for name in [
        "commands",
        "candidate_window",
        "config",
        "engine_builder",
        "user_dictionary_storage",
    ] {
        command.arg(src.join(format!("protocol/{name}.proto")));
    }
    run(&mut command)?;
    let old = FileDescriptorSet::decode(
        include_bytes!("../../../crates/kanade-mozc/src/generated/mozc-descriptor.bin").as_slice(),
    )?;
    let new = FileDescriptorSet::decode(fs::read(descriptor)?.as_slice())?;
    let current = signatures(&new);
    let changed: Vec<_> = signatures(&old)
        .into_iter()
        .filter(|(name, value)| current.get(name) != Some(value))
        .map(|(name, _)| name)
        .collect();
    ensure!(
        changed.is_empty(),
        "incompatible protocol fields or enums: {}",
        changed.join(", ")
    );
    println!("existing protocol fields and enum values are compatible");
    Ok(())
}
