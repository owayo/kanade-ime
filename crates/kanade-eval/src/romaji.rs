//! Deterministic romaji keys for a kana reading.
//!
//! Every kana unit has exactly one spelling, so a reading always yields the
//! same keys. The spellings follow the pinned upstream table: kunrei forms as
//! in its kana-to-romaji table, with the remaining digraphs taken from its
//! romaji-to-kana table. The Japanese comma and full stop are typed in place.
//! A caller must still compare the worker's composition with the reading;
//! this table alone is not proof of what the worker reads.

/// Units of a reading and the keys typed for them. Two-kana units take
/// precedence.
const SYLLABLES: &[(&str, &str)] = &[
    ("あ", "a"),
    ("い", "i"),
    ("う", "u"),
    ("え", "e"),
    ("お", "o"),
    ("か", "ka"),
    ("き", "ki"),
    ("く", "ku"),
    ("け", "ke"),
    ("こ", "ko"),
    ("が", "ga"),
    ("ぎ", "gi"),
    ("ぐ", "gu"),
    ("げ", "ge"),
    ("ご", "go"),
    ("さ", "sa"),
    ("し", "si"),
    ("す", "su"),
    ("せ", "se"),
    ("そ", "so"),
    ("ざ", "za"),
    ("じ", "zi"),
    ("ず", "zu"),
    ("ぜ", "ze"),
    ("ぞ", "zo"),
    ("た", "ta"),
    ("ち", "ti"),
    ("つ", "tu"),
    ("て", "te"),
    ("と", "to"),
    ("だ", "da"),
    ("ぢ", "di"),
    ("づ", "du"),
    ("で", "de"),
    ("ど", "do"),
    ("な", "na"),
    ("に", "ni"),
    ("ぬ", "nu"),
    ("ね", "ne"),
    ("の", "no"),
    ("は", "ha"),
    ("ひ", "hi"),
    ("ふ", "hu"),
    ("へ", "he"),
    ("ほ", "ho"),
    ("ば", "ba"),
    ("び", "bi"),
    ("ぶ", "bu"),
    ("べ", "be"),
    ("ぼ", "bo"),
    ("ぱ", "pa"),
    ("ぴ", "pi"),
    ("ぷ", "pu"),
    ("ぺ", "pe"),
    ("ぽ", "po"),
    ("ま", "ma"),
    ("み", "mi"),
    ("む", "mu"),
    ("め", "me"),
    ("も", "mo"),
    ("や", "ya"),
    ("ゆ", "yu"),
    ("よ", "yo"),
    ("ら", "ra"),
    ("り", "ri"),
    ("る", "ru"),
    ("れ", "re"),
    ("ろ", "ro"),
    ("わ", "wa"),
    ("ゐ", "wyi"),
    ("ゑ", "wye"),
    ("を", "wo"),
    // A single "n" depends on the next key and stays pending at the end.
    ("ん", "nn"),
    ("ゔ", "vu"),
    ("ー", "-"),
    // Punctuation is composed as part of the reading; the default
    // configuration does not convert or commit on it.
    ("、", ","),
    ("。", "."),
    ("ぁ", "xa"),
    ("ぃ", "xi"),
    ("ぅ", "xu"),
    ("ぇ", "xe"),
    ("ぉ", "xo"),
    ("ゃ", "xya"),
    ("ゅ", "xyu"),
    ("ょ", "xyo"),
    ("ゎ", "xwa"),
    ("きゃ", "kya"),
    ("きぃ", "kyi"),
    ("きゅ", "kyu"),
    ("きぇ", "kye"),
    ("きょ", "kyo"),
    ("ぎゃ", "gya"),
    ("ぎぃ", "gyi"),
    ("ぎゅ", "gyu"),
    ("ぎぇ", "gye"),
    ("ぎょ", "gyo"),
    ("しゃ", "sya"),
    ("しぃ", "syi"),
    ("しゅ", "syu"),
    ("しぇ", "sye"),
    ("しょ", "syo"),
    ("じゃ", "zya"),
    ("じぃ", "zyi"),
    ("じゅ", "zyu"),
    ("じぇ", "zye"),
    ("じょ", "zyo"),
    ("ちゃ", "tya"),
    ("ちぃ", "tyi"),
    ("ちゅ", "tyu"),
    ("ちぇ", "tye"),
    ("ちょ", "tyo"),
    ("ぢゃ", "dya"),
    ("ぢぃ", "dyi"),
    ("ぢゅ", "dyu"),
    ("ぢぇ", "dye"),
    ("ぢょ", "dyo"),
    ("にゃ", "nya"),
    ("にぃ", "nyi"),
    ("にゅ", "nyu"),
    ("にぇ", "nye"),
    ("にょ", "nyo"),
    ("ひゃ", "hya"),
    ("ひぃ", "hyi"),
    ("ひゅ", "hyu"),
    ("ひぇ", "hye"),
    ("ひょ", "hyo"),
    ("びゃ", "bya"),
    ("びぃ", "byi"),
    ("びゅ", "byu"),
    ("びぇ", "bye"),
    ("びょ", "byo"),
    ("ぴゃ", "pya"),
    ("ぴぃ", "pyi"),
    ("ぴゅ", "pyu"),
    ("ぴぇ", "pye"),
    ("ぴょ", "pyo"),
    ("みゃ", "mya"),
    ("みぃ", "myi"),
    ("みゅ", "myu"),
    ("みぇ", "mye"),
    ("みょ", "myo"),
    ("りゃ", "rya"),
    ("りぃ", "ryi"),
    ("りゅ", "ryu"),
    ("りぇ", "rye"),
    ("りょ", "ryo"),
    ("いぇ", "ye"),
    ("うぁ", "wha"),
    ("うぃ", "wi"),
    ("うぇ", "we"),
    ("うぉ", "who"),
    ("くぁ", "kwa"),
    ("くぃ", "kwi"),
    ("くぅ", "kwu"),
    ("くぇ", "kwe"),
    ("くぉ", "kwo"),
    ("ぐぁ", "gwa"),
    ("ぐぃ", "gwi"),
    ("ぐぅ", "gwu"),
    ("ぐぇ", "gwe"),
    ("ぐぉ", "gwo"),
    ("すぁ", "swa"),
    ("すぃ", "swi"),
    ("すぅ", "swu"),
    ("すぇ", "swe"),
    ("すぉ", "swo"),
    ("ずぁ", "zwa"),
    ("ずぃ", "zwi"),
    ("ずぅ", "zwu"),
    ("ずぇ", "zwe"),
    ("ずぉ", "zwo"),
    ("つぁ", "tsa"),
    ("つぃ", "tsi"),
    ("つぇ", "tse"),
    ("つぉ", "tso"),
    ("てゃ", "tha"),
    ("てぃ", "thi"),
    ("てゅ", "thu"),
    ("てぇ", "the"),
    ("てょ", "tho"),
    ("でゃ", "dha"),
    ("でぃ", "dhi"),
    ("でゅ", "dhu"),
    ("でぇ", "dhe"),
    ("でょ", "dho"),
    ("とぁ", "twa"),
    ("とぃ", "twi"),
    ("とぅ", "twu"),
    ("とぇ", "twe"),
    ("とぉ", "two"),
    ("どぁ", "dwa"),
    ("どぃ", "dwi"),
    ("どぅ", "dwu"),
    ("どぇ", "dwe"),
    ("どぉ", "dwo"),
    ("ふぁ", "fa"),
    ("ふぃ", "fi"),
    ("ふぇ", "fe"),
    ("ふぉ", "fo"),
    ("ふゃ", "fya"),
    ("ふゅ", "fyu"),
    ("ふょ", "fyo"),
    ("ゔぁ", "va"),
    ("ゔぃ", "vi"),
    ("ゔぇ", "ve"),
    ("ゔぉ", "vo"),
    ("ゔゃ", "vya"),
    ("ゔゅ", "vyu"),
    ("ゔょ", "vyo"),
];

const SOKUON: char = 'っ';
/// The sokuon typed on its own, when no following consonant can be doubled.
const SOKUON_KEYS: &str = "xtu";

fn spelling(unit: &str) -> Option<&'static str> {
    SYLLABLES
        .iter()
        .find(|(kana, _)| *kana == unit)
        .map(|(_, keys)| *keys)
}

/// The upstream table doubles every consonant except "n", which is the
/// moraic nasal. Vowels, the long vowel mark and punctuation have nothing to
/// double.
fn doubles(key: u8) -> bool {
    key.is_ascii_lowercase() && !b"aiueon".contains(&key)
}

/// Katakana letters as hiragana. Other characters, including the long vowel
/// mark, are returned unchanged.
pub fn hiragana(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'ァ'..='ヴ' => char::from_u32(c as u32 - 0x60).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// Keys that compose a hiragana reading, or `None` when it contains a
/// character outside the table.
pub fn keys(reading: &str) -> Option<String> {
    let mut keys = String::with_capacity(reading.len());
    let mut sokuon = false;
    let mut rest = reading;
    while let Some(first) = rest.chars().next() {
        let mut width = first.len_utf8();
        if first == SOKUON {
            // Only the last sokuon of a run has a consonant to double.
            if sokuon {
                keys.push_str(SOKUON_KEYS);
            }
            sokuon = true;
            rest = &rest[width..];
            continue;
        }
        let pair = rest[width..]
            .chars()
            .next()
            .map(|second| &rest[..width + second.len_utf8()]);
        let unit = match pair.and_then(|pair| Some((pair, spelling(pair)?))) {
            Some((pair, unit)) => {
                width = pair.len();
                unit
            }
            None => spelling(&rest[..width])?,
        };
        if sokuon {
            match unit.as_bytes()[0] {
                consonant if doubles(consonant) => keys.push(char::from(consonant)),
                _ => keys.push_str(SOKUON_KEYS),
            }
            sokuon = false;
        }
        keys.push_str(unit);
        rest = &rest[width..];
    }
    if sokuon {
        keys.push_str(SOKUON_KEYS);
    }
    Some(keys)
}

/// Every unit with its keys, for checking the table against a real worker.
#[cfg(test)]
pub fn syllables() -> impl Iterator<Item = (&'static str, &'static str)> {
    SYLLABLES.iter().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(reading: &str) -> String {
        keys(reading).expect("supported reading")
    }

    #[test]
    fn each_unit_has_one_lowercase_spelling() {
        let mut seen = std::collections::BTreeSet::new();
        for (kana, keys) in SYLLABLES {
            assert!(seen.insert(*kana), "duplicate unit");
            assert!(matches!(kana.chars().count(), 1 | 2));
            assert!(!keys.is_empty());
            assert!(
                keys.bytes()
                    .all(|b| b.is_ascii_lowercase() || b"-,.".contains(&b))
            );
            // A lone "n" is the only spelling that waits for the next key.
            assert_ne!(*keys, "n");
            if let Some(small) = kana.chars().nth(1) {
                assert!("ぁぃぅぇぉゃゅょ".contains(small));
            }
        }
        // Every hiragana letter is typed directly or by the sokuon rule.
        for kana in 'ぁ'..='ゔ' {
            assert_eq!(
                spelling(kana.encode_utf8(&mut [0; 4])).is_some(),
                kana != SOKUON
            );
        }
        for mark in ["ー", "、", "。"] {
            assert!(spelling(mark).is_some());
        }
    }

    #[test]
    fn punctuation_is_typed_in_place() {
        assert_eq!(typed("きょうは、いいてんきです。"), "kyouha,iitennkidesu.");
        assert_eq!(typed("、"), ",");
        assert_eq!(typed("。。"), "..");
        assert_eq!(typed("、あ"), ",a");
        // Neither mark takes part in the sounds before it.
        assert_eq!(typed("ほん、"), "honn,");
        assert_eq!(typed("あっ、"), "axtu,");
        assert_eq!(typed("あっ。"), "axtu.");
        assert_eq!(typed("らー。"), "ra-.");
    }

    #[test]
    fn plain_and_voiced_syllables() {
        assert_eq!(typed("にほんご"), "nihonngo");
        assert_eq!(typed("あいうえお"), "aiueo");
        assert_eq!(typed("しちつふじ"), "sitituhuzi");
        assert_eq!(typed("ぱぴぷぺぽ"), "papipupepo");
    }

    #[test]
    fn moraic_nasal_never_depends_on_the_next_kana() {
        assert_eq!(typed("かんじ"), "kannzi");
        assert_eq!(typed("ほんや"), "honnya");
        assert_eq!(typed("あんない"), "annnai");
        assert_eq!(typed("きんえん"), "kinnenn");
        assert_eq!(typed("にほん"), "nihonn");
        assert_eq!(typed("ん"), "nn");
    }

    #[test]
    fn sokuon_doubles_the_next_consonant() {
        assert_eq!(typed("がっこう"), "gakkou");
        assert_eq!(typed("まっちゃ"), "mattya");
        assert_eq!(typed("ざっし"), "zassi");
        assert_eq!(typed("ろっぷ"), "roppu");
        assert_eq!(typed("ふぉっくす"), "fokkusu");
        assert_eq!(typed("あっゔ"), "avvu");
        assert_eq!(typed("あっを"), "awwo");
        assert_eq!(typed("あっぁ"), "axxa");
    }

    #[test]
    fn sokuon_without_a_consonant_is_typed_alone() {
        assert_eq!(typed("あっ"), "axtu");
        assert_eq!(typed("っ"), "xtu");
        assert_eq!(typed("あっあ"), "axtua");
        assert_eq!(typed("いっぬ"), "ixtunu");
        assert_eq!(typed("あっん"), "axtunn");
        assert_eq!(typed("あっー"), "axtu-");
        // Three equal keys would select the upstream "www" rule.
        assert_eq!(typed("っっわ"), "xtuwwa");
        assert_eq!(typed("かっっか"), "kaxtukka");
        assert_eq!(typed("っっ"), "xtuxtu");
    }

    #[test]
    fn contracted_sounds() {
        assert_eq!(typed("きょう"), "kyou");
        assert_eq!(typed("しゃしん"), "syasinn");
        assert_eq!(typed("ちゅうい"), "tyuui");
        assert_eq!(typed("じゅうしょ"), "zyuusyo");
        assert_eq!(typed("りょこう"), "ryokou");
        assert_eq!(typed("ぎゃくにひゃくみゃく"), "gyakunihyakumyaku");
        assert_eq!(typed("ぴゅあびょうにゅう"), "pyuabyounyuu");
        assert_eq!(typed("ぢゃ"), "dya");
    }

    #[test]
    fn small_kana_outside_a_contracted_sound() {
        assert_eq!(typed("ぁぃぅぇぉ"), "xaxixuxexo");
        assert_eq!(typed("ゃゅょゎ"), "xyaxyuxyoxwa");
        assert_eq!(typed("あぁ"), "axa");
        assert_eq!(typed("まゃ"), "maxya");
        assert_eq!(typed("ぐゎ"), "guxwa");
    }

    #[test]
    fn loanword_sounds() {
        assert_eq!(typed("ふぁいる"), "fairu");
        assert_eq!(typed("ふぃふぇふぉふゅ"), "fifefofyu");
        assert_eq!(typed("てぃー"), "thi-");
        assert_eq!(typed("でぃすく"), "dhisuku");
        assert_eq!(typed("でゅえっと"), "dhuetto");
        assert_eq!(typed("とぅどぅ"), "twudwu");
        assert_eq!(typed("ゔ"), "vu");
        assert_eq!(typed("ゔぁいおりん"), "vaiorinn");
        assert_eq!(typed("ゔぃゔぇゔぉ"), "vivevo");
        assert_eq!(typed("うぃんどう"), "winndou");
        assert_eq!(typed("うぇぶ"), "webu");
        assert_eq!(typed("うぉーく"), "who-ku");
        assert_eq!(typed("しぇあ"), "syea");
        assert_eq!(typed("じぇっと"), "zyetto");
        assert_eq!(typed("ちぇっく"), "tyekku");
        assert_eq!(typed("つぁつぃつぇつぉ"), "tsatsitsetso");
        assert_eq!(typed("くぉーたー"), "kwo-ta-");
        assert_eq!(typed("いぇい"), "yei");
    }

    #[test]
    fn long_vowel_mark_and_historical_kana() {
        assert_eq!(typed("らーめん"), "ra-menn");
        assert_eq!(typed("ー"), "-");
        assert_eq!(typed("を"), "wo");
        assert_eq!(typed("ぢづ"), "didu");
        assert_eq!(typed("ゐゑ"), "wyiwye");
    }

    #[test]
    fn katakana_becomes_hiragana() {
        assert_eq!(hiragana("ラーメン"), "らーめん");
        assert_eq!(hiragana("ヴァイオリン"), "ゔぁいおりん");
        assert_eq!(hiragana("ァヴ"), "ぁゔ");
        assert_eq!(hiragana("ひらがなカタカナ"), "ひらがなかたかな");
        assert_eq!(typed(&hiragana("ガッコウ")), "gakkou");
        // Letters without a hiragana form stay as they are and are rejected.
        assert_eq!(hiragana("ヵヶヷ・"), "ヵヶヷ・");
    }

    #[test]
    fn characters_outside_the_table_are_rejected() {
        for reading in [
            "abc",
            "漢字",
            "にほん語",
            "ヵ",
            "ヶ",
            "ゕ",
            "ゖ",
            "ヷ",
            "・",
            "ゝ",
            "ｶﾅ",
            "あ い",
            "１",
            "1",
            "か\u{3099}",
            "う゛",
            "，",
            "．",
            ",",
            ".",
            "､",
            "｡",
            "？",
            "！",
            "「」",
            "ラーメン",
        ] {
            assert_eq!(keys(reading), None);
        }
        assert_eq!(keys(""), Some(String::new()));
    }
}
