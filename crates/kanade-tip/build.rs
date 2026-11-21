//! Links the brand icon into the input DLL. Windows shows it in the input
//! switcher and in the taskbar's input indicator, and reads it from the file
//! and the position that registration records.
use std::{env, error::Error, fs, path::PathBuf};

/// Identifier of the brand icon's group, and of the first of its bitmaps.
///
/// Registration names an icon by its position among the groups of the file,
/// which are counted in ascending identifier order. This is the only group, so
/// it is at position zero. A group added later needs a higher identifier.
const BRAND_ICON: u16 = 1;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS")? != "windows" {
        return Ok(());
    }
    let template = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?)
        .join("../../assets/icons/input-template.png");
    println!("cargo:rerun-if-changed={}", template.display());
    let images = kanade_winres::brand_icon::render(&fs::read(&template)?)?;
    let resources = kanade_winres::res::icon_group(BRAND_ICON, BRAND_ICON, &images)?;
    let file = PathBuf::from(env::var("OUT_DIR")?).join("kanade-tip.res");
    fs::write(&file, kanade_winres::res::file(&resources)?)?;
    // `link.exe` and `lld-link` convert a resource file themselves. Other
    // linkers would need it compiled to an object file first, and a DLL built
    // without the icon would only show the gap after installation.
    if env::var("CARGO_CFG_TARGET_ENV")? != "msvc" {
        return Err(
            "the input DLL's icon is linked as a resource file, which needs an MSVC target".into(),
        );
    }
    println!("cargo:rustc-link-arg-cdylib={}", file.display());
    Ok(())
}
