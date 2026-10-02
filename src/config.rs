//! 設定ファイルの読み込み・検索・ホットキー文字列の解釈(OS 非依存)。

use serde::Deserialize;

pub const FILE_NAME: &str = "snippets.toml";
pub const SAMPLE: &str = include_str!("../snippets.sample.toml");

// RegisterHotKey の fsModifiers と同じ値
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_WIN: u32 = 0x0008;

const VK_SPACE: u32 = 0x20;
const VK_F1: u32 = 0x70;

const PREVIEW_CHARS: usize = 60;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_hotkey")]
    hotkey: String,
    #[serde(default)]
    snippet: Vec<RawSnippet>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSnippet {
    #[serde(default)]
    name: String,
    text: String,
}

fn default_hotkey() -> String {
    "Ctrl+Alt+Space".to_string()
}

pub struct Config {
    pub hotkey: String,
    pub snippets: Vec<Snippet>,
}

pub struct Snippet {
    /// 貼り付ける本文(改行は LF)
    pub text: String,
    /// 一覧に表示する文字列(名前 + タブ + 本文の冒頭)
    pub label: String,
    name_key: String,
    full_key: String,
}

impl Snippet {
    fn new(raw: RawSnippet) -> Self {
        let text = raw.text.replace("\r\n", "\n");
        let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
        let first_line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .replace('\t', " ");
        let preview: String = first_line.chars().take(PREVIEW_CHARS).collect();
        let (name, label) = if raw.name.is_empty() {
            (preview.clone(), preview)
        } else {
            let label = format!("{}\t{}", raw.name, preview);
            (raw.name, label)
        };
        Snippet {
            name_key: name.to_lowercase(),
            full_key: format!("{name}\n{text}").to_lowercase(),
            text,
            label,
        }
    }
}

pub fn parse(src: &str) -> Result<Config, String> {
    let raw: RawConfig =
        toml::from_str(src.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
    Ok(Config {
        hotkey: raw.hotkey,
        snippets: raw.snippet.into_iter().map(Snippet::new).collect(),
    })
}

/// 空白区切りの全語を含むスニペットの添字を返す。名前で一致したものを先に並べる。
pub fn search(snippets: &[Snippet], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();
    let mut by_name = Vec::new();
    let mut by_body = Vec::new();
    for (i, s) in snippets.iter().enumerate() {
        if terms.iter().all(|t| s.name_key.contains(t)) {
            by_name.push(i);
        } else if terms.iter().all(|t| s.full_key.contains(t)) {
            by_body.push(i);
        }
    }
    by_name.extend(by_body);
    by_name
}

/// "Ctrl+Alt+Space" のような文字列を (修飾キー, 仮想キーコード) に変換する。
pub fn parse_hotkey(spec: &str) -> Option<(u32, u32)> {
    let mut mods = 0;
    let mut vk = None;
    for part in spec.split('+') {
        let part = part.trim().to_ascii_lowercase();
        match part.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" => mods |= MOD_WIN,
            key => {
                if vk.replace(parse_key(key)?).is_some() {
                    return None;
                }
            }
        }
    }
    let vk = vk?;
    // 修飾キーなしの文字キーは通常の入力を奪ってしまうので認めない
    if mods == 0 && vk < VK_F1 {
        return None;
    }
    Some((mods, vk))
}

fn parse_key(key: &str) -> Option<u32> {
    if key == "space" {
        return Some(VK_SPACE);
    }
    let mut chars = key.chars();
    match (chars.next()?, chars.as_str()) {
        (c, "") if c.is_ascii_alphanumeric() => Some(c.to_ascii_uppercase() as u32),
        ('f', n) => match n.parse::<u32>() {
            Ok(n @ 1..=24) => Some(VK_F1 + n - 1),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_parses() {
        let cfg = parse(SAMPLE).unwrap();
        assert_eq!(cfg.hotkey, "Ctrl+Alt+Space");
        assert_eq!(cfg.snippets.len(), 3);
        assert_eq!(cfg.snippets[0].label, "メールアドレス\ttaro@example.com");
        assert_eq!(cfg.snippets[1].text, "お世話になっております。\n山田です。");
        assert_eq!(
            cfg.snippets[2].label,
            "git log --oneline --graph --decorate"
        );
        assert!(parse_hotkey(&cfg.hotkey).is_some());
    }

    #[test]
    fn hotkey_defaults_and_bom_and_crlf() {
        let cfg = parse("\u{feff}[[snippet]]\r\ntext = '''\r\na\r\nb\r\n'''\r\n").unwrap();
        assert_eq!(cfg.hotkey, "Ctrl+Alt+Space");
        assert_eq!(cfg.snippets[0].text, "a\nb");
    }

    #[test]
    fn empty_file_is_ok() {
        assert!(parse("").unwrap().snippets.is_empty());
    }

    #[test]
    fn typos_are_errors() {
        assert!(parse("[[snippets]]\ntext = 'a'").is_err());
        assert!(parse("[[snippet]]\nname = 'a'").is_err());
        assert!(parse("[[snippet]]\ntext = 'a'\nnmae = 'b'").is_err());
    }

    #[test]
    fn search_matches_all_terms_and_ranks_name_first() {
        let cfg = parse(
            "[[snippet]]\nname = 'Body'\ntext = 'the mail address'\n\
             [[snippet]]\nname = 'Mail Address'\ntext = 'x'\n\
             [[snippet]]\nname = 'Other'\ntext = 'y'\n",
        )
        .unwrap();
        let s = &cfg.snippets;
        assert_eq!(search(s, ""), vec![0, 1, 2]);
        assert_eq!(search(s, "MAIL"), vec![1, 0]);
        assert_eq!(search(s, "addr mail"), vec![1, 0]);
        assert_eq!(search(s, "body mail"), vec![0]);
        assert_eq!(search(s, "nothing"), Vec::<usize>::new());
    }

    #[test]
    fn hotkey_parsing() {
        assert_eq!(parse_hotkey("Ctrl+Alt+Space"), Some((0x3, 0x20)));
        assert_eq!(parse_hotkey(" win + shift + f12 "), Some((0xC, 0x7B)));
        assert_eq!(parse_hotkey("Alt+q"), Some((0x1, 0x51)));
        assert_eq!(parse_hotkey("Ctrl+1"), Some((0x2, 0x31)));
        assert_eq!(parse_hotkey("F9"), Some((0, 0x78)));
        assert_eq!(parse_hotkey("A"), None);
        assert_eq!(parse_hotkey("Ctrl+Alt"), None);
        assert_eq!(parse_hotkey("Ctrl+A+B"), None);
        assert_eq!(parse_hotkey("Ctrl+F25"), None);
        assert_eq!(parse_hotkey("Ctrl+"), None);
        assert_eq!(parse_hotkey("Ctrl+あ"), None);
        assert_eq!(parse_hotkey(""), None);
    }
}
