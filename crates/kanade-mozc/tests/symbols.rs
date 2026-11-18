use kanade_ipc::{Key, MOD_SHIFT, Privacy, surface};
use kanade_mozc::{ConversionEngine, Mozc, default_worker};

#[test]
#[ignore = "requires the locked worker; make contract"]
fn shifted_tilde_remains_fullwidth_in_hiragana() {
    let profile = tempfile::tempdir().unwrap();
    let mut mozc = Mozc::start(&default_worker(), profile.path()).unwrap();
    mozc.set_privacy(Privacy::Private).unwrap();
    for modifiers in [0, MOD_SHIFT] {
        for (input, expected) in [
            ("~", "〜"),
            ("a~", "あ〜"),
            ("~a", "〜あ"),
            ("~~", "〜〜"),
            ("nihongo~", "にほんご〜"),
        ] {
            let id = mozc.create_session().unwrap();
            let mut visible = String::new();
            for character in input.chars() {
                let out = mozc
                    .key(
                        id,
                        &Key {
                            text: character.to_string(),
                            modifiers: if character == '~' { modifiers } else { 0 },
                            ..Default::default()
                        },
                        "",
                    )
                    .unwrap();
                assert!(out.consumed, "modifiers={modifiers}");
                assert!(out.commit.is_none());
                visible = surface(&out.preedit);
            }
            assert_eq!(visible, expected);
            assert_eq!(mozc.submit(id).unwrap().commit.unwrap().value, expected);
            mozc.delete_session(id).unwrap();
        }
    }
    mozc.shutdown().unwrap();
}
