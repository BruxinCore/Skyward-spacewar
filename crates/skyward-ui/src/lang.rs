use anyhow::{Context, Result};
use icu_locale_core::Locale;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Hans,
    Hant,
    En,
    PtBr,
}

pub type Text = [&'static str; 3];

impl Lang {
    pub const ALL: [Lang; 4] = [Lang::Hans, Lang::Hant, Lang::En, Lang::PtBr];

    pub fn name(self) -> &'static str {
        match self {
            Lang::Hans => "简体中文",
            Lang::Hant => "繁體中文",
            Lang::En => "English",
            Lang::PtBr => "Português (Brasil)",
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Lang::Hans => "zh-Hans",
            Lang::Hant => "zh-Hant",
            Lang::En => "en",
            Lang::PtBr => "pt-BR",
        }
    }

    pub fn pick<'a>(self, [hans, hant, en]: [&'a str; 3]) -> &'a str {
        match self {
            Lang::Hans => hans,
            Lang::Hant => hant,
            Lang::En => en,
            Lang::PtBr => crate::pt_br::pick(en).unwrap_or(en),
        }
    }

    pub fn for_locale(tag: &str) -> Result<Lang> {
        let locale = Locale::try_from_str(&tag.replace('_', "-")).with_context(|| format!("read the locale {tag:?}"))?;
        let id = locale.id;
        if id.language.as_str() == "pt" {
            return Ok(Lang::PtBr);
        }
        if id.language.as_str() != "zh" {
            return Ok(Lang::En);
        }
        let traditional = match id.script {
            Some(script) => script.as_str() == "Hant",
            None => id.region.is_some_and(|r| matches!(r.as_str(), "TW" | "HK" | "MO")),
        };
        Ok(if traditional { Lang::Hant } else { Lang::Hans })
    }
}

#[macro_export]
macro_rules! tr {
    ($l:expr, $hans:literal, $hant:literal, $en:literal, $pt:literal $(,)?) => {
        match $l {
            $crate::lang::Lang::Hans => format!($hans),
            $crate::lang::Lang::Hant => format!($hant),
            $crate::lang::Lang::En => format!($en),
            $crate::lang::Lang::PtBr => format!($pt),
        }
    };
    ($l:expr, $hans:literal, $hant:literal, $en:literal $(,)?) => {
        match $l {
            $crate::lang::Lang::Hans => format!($hans),
            $crate::lang::Lang::Hant => format!($hant),
            $crate::lang::Lang::En | $crate::lang::Lang::PtBr => format!($en),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use proc_macro2::{TokenStream, TokenTree};
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;
    use syn::visit::Visit;

    #[derive(Default)]
    struct Texts(Vec<String>);

    impl Texts {
        fn tokens(&mut self, tokens: TokenStream) {
            for tree in tokens {
                match tree {
                    TokenTree::Literal(lit) => {
                        if let syn::Lit::Str(s) = syn::Lit::new(lit) {
                            self.0.push(s.value());
                        }
                    }
                    TokenTree::Group(group) => self.tokens(group.stream()),
                    TokenTree::Ident(_) | TokenTree::Punct(_) => {}
                }
            }
        }
    }

    impl<'ast> Visit<'ast> for Texts {
        fn visit_lit_str(&mut self, lit: &'ast syn::LitStr) {
            self.0.push(lit.value());
        }

        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            self.tokens(mac.tokens.clone());
        }
    }

    fn read(texts: &mut Texts, folder: &Path) {
        for entry in fs::read_dir(folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                read(texts, &path);
                continue;
            }
            let file = syn::parse_file(&fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
            texts.visit_file(&file);
        }
    }

    #[test]
    fn the_typeface_draws_every_text_the_pages_write() {
        let f = crate::font::bundled();
        let mut texts = Texts::default();
        read(&mut texts, &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
        assert!(texts.0.iter().any(|t| t.contains("簽章")), "the texts in traditional characters are read");
        let missing: BTreeSet<char> = texts.0.iter().flat_map(|t| t.chars()).filter(|&ch| !ch.is_control() && !f.body.has(ch)).collect();
        assert!(missing.is_empty(), "MiSans draws none of {missing:?}");
    }

    #[test]
    fn a_locale_picks_the_language_and_its_characters() {
        let of = |tag| Lang::for_locale(tag).unwrap();
        assert_eq!(of("zh-CN"), Lang::Hans, "a China system, as the 17 Ultra's");
        assert_eq!(of("zh_CN"), Lang::Hans, "as older systems write it");
        assert_eq!(of("zh-SG"), Lang::Hans);
        assert_eq!(of("zh"), Lang::Hans);
        assert_eq!(of("zh-TW"), Lang::Hant);
        assert_eq!(of("zh-HK"), Lang::Hant);
        assert_eq!(of("zh-Hant-TW"), Lang::Hant);
        assert_eq!(of("zh-Hans-HK"), Lang::Hans, "a script named wins over the region");
        assert_eq!(of("en-US"), Lang::En);
        assert_eq!(of("pt-BR"), Lang::PtBr);
        assert_eq!(of("pt_BR"), Lang::PtBr);
        assert_eq!(of("ja-JP"), Lang::En, "a language the pages do not speak");
        assert!(Lang::for_locale("not a locale").is_err());
    }

    #[test]
    fn each_language_s_locale_reads_back_as_it() {
        for lang in Lang::ALL {
            assert_eq!(Lang::for_locale(lang.tag()).unwrap(), lang);
        }
    }

    #[test]
    fn a_text_is_picked_and_made_in_the_language() {
        assert_eq!(Lang::Hant.pick(["设置", "設定", "Settings"]), "設定");
        assert_eq!(Lang::PtBr.pick(["设置", "設定", "Settings"]), "Configurações");
        let base = "vendor";
        assert_eq!(tr!(Lang::En, "挂载 {base} 失败", "掛載 {base} 失敗", "Couldn't mount {base}", "Falha ao montar {base}"), "Couldn't mount vendor");
        assert_eq!(tr!(Lang::PtBr, "挂载 {base} 失败", "掛載 {base} 失敗", "Couldn't mount {base}", "Falha ao montar {base}"), "Falha ao montar vendor");
        assert_eq!(Lang::ALL.map(Lang::name), ["简体中文", "繁體中文", "English", "Português (Brasil)"]);
    }
}
