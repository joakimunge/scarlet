//! Honour the user's own vim/neovim keybindings for navigation.
//!
//! Instead of parsing Lua or Vimscript, Scarlet asks the editor itself: it
//! starts `nvim --headless` (or `vim -es`) with the user's real config, dumps
//! the normal-mode mappings to a temp file and translates the single-key ones
//! whose right-hand side is a motion (or window navigation) into a [`Motion`].
//! The result is cached and keyed by a fingerprint of the config, so later
//! starts never launch the editor.
//!
//! The map only holds what the config adds or changes; Scarlet's built-in
//! defaults (hjkl, arrows, gg, G, ...) still apply after it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Value, json};

/// Bumped whenever translation rules or the cache layout change, so stale
/// caches are rebuilt.
const CACHE_VERSION: u64 = 1;
const EDITOR_TIMEOUT: Duration = Duration::from_secs(5);
const OUT_ENV: &str = "SCARLET_VIMKEYS_OUT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    HalfUp,
    HalfDown,
    Top,
    Bottom,
    FirstColumn,
    LastColumn,
    FocusLeft,
    FocusRight,
}

impl Motion {
    const ALL: [Motion; 14] = [
        Motion::Up,
        Motion::Down,
        Motion::Left,
        Motion::Right,
        Motion::PageUp,
        Motion::PageDown,
        Motion::HalfUp,
        Motion::HalfDown,
        Motion::Top,
        Motion::Bottom,
        Motion::FirstColumn,
        Motion::LastColumn,
        Motion::FocusLeft,
        Motion::FocusRight,
    ];

    fn name(self) -> String {
        format!("{self:?}")
    }

    fn from_name(name: &str) -> Option<Motion> {
        Motion::ALL.into_iter().find(|m| m.name() == name)
    }
}

/// A key normalised so that a crossterm event and a vim key notation compare
/// equal: SHIFT is folded into the character, CTRL+letter is lowercase.
type Key = (KeyCode, KeyModifiers);

pub struct VimKeys {
    keys: HashMap<Key, Motion>,
    /// `(lhs as the editor printed it, motion)`, in config order.
    entries: Vec<(String, Motion)>,
    /// `mapleader` as the editor reports it (e.g. `" "`); `None` when unset.
    pub leader: Option<String>,
    /// Where the bindings came from, e.g. `"nvim (LazyVim config)"`.
    pub source: String,
}

impl VimKeys {
    /// Motion the user's config gives this key, if any.
    pub fn motion(&self, key: &KeyEvent) -> Option<Motion> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        self.keys.get(&normalize(key.code, key.modifiers)).copied()
    }

    /// The translated mappings as `(key notation, motion)`, e.g. for a help screen.
    #[allow(dead_code)]
    pub fn entries(&self) -> &[(String, Motion)] {
        &self.entries
    }

    /// Loads from cache when the config is unchanged, otherwise asks nvim/vim.
    /// Never panics; `None` when no vim config/binary.
    pub async fn load() -> Option<VimKeys> {
        let editor = Editor::discover()?;
        let fp_editor = editor.clone();
        let fingerprint = tokio::task::spawn_blocking(move || fp_editor.fingerprint()).await.ok()?;
        let cache = cache_path();
        if let Some(cached) = cache.as_deref().and_then(|p| read_cache(p, &fingerprint)) {
            return Some(cached);
        }
        let dump = editor.extract().await?;
        let keys = VimKeys::build(&dump.maps, dump.leader, dump.source);
        if let Some(path) = cache {
            write_cache(&path, &fingerprint, &keys);
        }
        Some(keys)
    }

    fn build(maps: &[RawMap], leader: Option<String>, source: String) -> VimKeys {
        let mut keys = HashMap::new();
        let mut entries = Vec::new();
        for map in maps {
            let (Some(key), Some(motion)) = (parse_single_key(&map.lhs), translate(map)) else {
                continue;
            };
            keys.insert(key, motion);
            entries.push((map.lhs.clone(), motion));
        }
        VimKeys { keys, entries, leader, source }
    }

    fn from_entries(entries: Vec<(String, Motion)>, leader: Option<String>, source: String) -> VimKeys {
        let keys = entries.iter().filter_map(|(lhs, m)| Some((parse_single_key(lhs)?, *m))).collect();
        VimKeys { keys, entries, leader, source }
    }
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// One normal-mode mapping as reported by the editor.
#[derive(Debug, Clone, Default)]
struct RawMap {
    lhs: String,
    rhs: String,
    expr: bool,
    desc: String,
    /// The mapping runs a Lua callback (nvim), so `rhs` is empty.
    callback: bool,
}

fn translate(map: &RawMap) -> Option<Motion> {
    let rhs = map.rhs.trim();
    if rhs.is_empty() {
        return if map.callback { focus_from_desc(&map.desc) } else { None };
    }
    if map.expr { translate_expr(rhs) } else { translate_plain(rhs) }
}

/// Expression mappings like `v:count == 0 ? 'gj' : 'j'`: every quoted string
/// that is a motion must agree.
fn translate_expr(rhs: &str) -> Option<Motion> {
    let found: Vec<Motion> =
        quoted_strings(rhs).iter().filter_map(|s| translate_plain(&s.replace("\\<", "<"))).collect();
    let first = *found.first()?;
    found.iter().all(|m| *m == first).then_some(first)
}

fn translate_plain(rhs: &str) -> Option<Motion> {
    let mut toks = tokens(rhs);
    if let Some(focus) = focus_from_tokens(&toks) {
        return Some(focus);
    }
    // Cosmetic view adjustments after a motion (`<C-d>zz`, `nzzzv`).
    while toks.len() >= 2
        && toks[toks.len() - 2] == "z"
        && matches!(toks[toks.len() - 1].as_str(), "z" | "t" | "b" | "v")
    {
        toks.truncate(toks.len() - 2);
    }
    let toks: Vec<&str> = toks.iter().map(String::as_str).collect();
    Some(match toks.as_slice() {
        ["j"] | ["g", "j"] | ["<down>"] => Motion::Down,
        ["k"] | ["g", "k"] | ["<up>"] => Motion::Up,
        ["h"] | ["<left>"] => Motion::Left,
        ["l"] | ["<right>"] => Motion::Right,
        ["<c-d>"] => Motion::HalfDown,
        ["<c-u>"] => Motion::HalfUp,
        ["<c-f>"] | ["<pagedown>"] | ["<s-down>"] => Motion::PageDown,
        ["<c-b>"] | ["<pageup>"] | ["<s-up>"] => Motion::PageUp,
        ["g", "g"] | ["<c-home>"] => Motion::Top,
        ["G"] | ["<c-end>"] => Motion::Bottom,
        ["0"] | ["^"] | ["_"] | ["<home>"] => Motion::FirstColumn,
        ["$"] | ["g", "_"] | ["<end>"] => Motion::LastColumn,
        _ => return None,
    })
}

/// `<C-w>h`, `<C-w><C-h>`, `<Cmd>wincmd h<CR>`, `:<C-U>TmuxNavigateLeft<CR>`...
fn focus_from_tokens(toks: &[String]) -> Option<Motion> {
    let t: Vec<&str> = toks.iter().map(String::as_str).collect();
    match t.as_slice() {
        ["<c-w>", "h" | "<c-h>" | "<left>"] => return Some(Motion::FocusLeft),
        ["<c-w>", "l" | "<c-l>" | "<right>"] => return Some(Motion::FocusRight),
        _ => {}
    }
    let rest = match t.first() {
        Some(&"<cmd>") | Some(&":") => &t[1..],
        _ => return None,
    };
    let rest = rest.strip_prefix(&["<c-u>"]).unwrap_or(rest);
    let body = rest.strip_suffix(&["<cr>"])?;
    let command: String = body.concat();
    focus_from_command(&command)
}

fn focus_from_command(command: &str) -> Option<Motion> {
    let lower = command.to_lowercase();
    if lower.contains("resize") || lower.contains("swap") {
        return None;
    }
    let sides = [(Motion::FocusLeft, 'h', "left"), (Motion::FocusRight, 'l', "right")];
    for (motion, letter, word) in sides {
        let patterns = [
            format!("navigate{word}"),     // TmuxNavigateLeft, KittyNavigateLeft
            format!("navigator{word}"),    // Navigator.nvim
            format!("move_cursor_{word}"), // smart-splits
            format!("move_{word}("),       // tmux.nvim
        ];
        if wincmd_target(command) == Some(letter) || patterns.iter().any(|p| lower.contains(p)) {
            return Some(motion);
        }
    }
    None
}

/// The direction letter after `wincmd` (`wincmd h`, `vim.cmd.wincmd("h")`).
/// Case-sensitive: `wincmd H` moves the window instead of the cursor.
fn wincmd_target(command: &str) -> Option<char> {
    let after = &command[command.find("wincmd")? + "wincmd".len()..];
    let mut chars = after.chars().skip_while(|c| matches!(c, ' ' | '(' | '\'' | '"' | '['));
    let letter = chars.next()?;
    let next = chars.next();
    (!next.is_some_and(|c| c.is_alphanumeric())).then_some(letter)
}

/// Lua-callback mappings carry only a description, e.g. "Go to Left Window".
fn focus_from_desc(desc: &str) -> Option<Motion> {
    let lower = desc.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |w: &str| words.contains(&w);
    let target = ["window", "windows", "split", "splits", "pane", "panes"].iter().any(|w| has(w));
    let excluded =
        ["resize", "swap", "increase", "decrease", "width", "height", "close", "new", "create", "open", "delete"]
            .iter()
            .any(|w| has(w));
    let moves_window = words.windows(2).any(|p| p[0] == "move" && matches!(p[1], "window" | "split" | "pane"));
    if !target || excluded || moves_window || words.first() == Some(&"split") {
        return None;
    }
    match (has("left"), has("right")) {
        (true, false) => Some(Motion::FocusLeft),
        (false, true) => Some(Motion::FocusRight),
        _ => None,
    }
}

fn quoted_strings(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\'' && c != '"' {
            continue;
        }
        let mut text = String::new();
        let mut closed = false;
        while let Some(d) = chars.next() {
            if d == c {
                closed = true;
                break;
            }
            if d == '\\' && c == '"' {
                // Keep `\<` so callers can turn `"\<C-d>"` into a key.
                if let Some(e) = chars.next() {
                    if e == '<' {
                        text.push('\\');
                    }
                    text.push(e);
                }
                continue;
            }
            text.push(d);
        }
        if closed {
            out.push(text);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Key notation
// ---------------------------------------------------------------------------

/// Splits a key sequence into tokens: single characters (case kept) and
/// `<...>` keys in a canonical lowercase form (`<C-D>` -> `<c-d>`).
fn tokens(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<'
            && let Some(len) = chars[i + 1..].iter().take(32).position(|&c| c == '>')
        {
            let inner: String = chars[i + 1..i + 1 + len].iter().collect();
            if let Some(tok) = canonical_bracket(&inner) {
                out.push(tok);
                i += len + 2;
                continue;
            }
        }
        out.push(chars[i].to_string());
        i += 1;
    }
    out
}

/// `C-D` -> `<c-d>`, `CR` -> `<cr>`, `lt` -> `<`, `M-H` -> `<m-H>`.
fn canonical_bracket(inner: &str) -> Option<String> {
    if inner.is_empty() || inner.contains(['<', ' ']) {
        return None;
    }
    let (mods, name) = split_modifiers(inner);
    let mut name_out = if name.chars().count() == 1 {
        name.to_string()
    } else {
        match name.to_lowercase().as_str() {
            "enter" | "return" => "cr".into(),
            "backspace" => "bs".into(),
            "delete" => "del".into(),
            "lt" if mods.is_empty() => return Some("<".into()),
            other => other.into(),
        }
    };
    if mods.contains(&'c') {
        name_out = name_out.to_lowercase();
    }
    let prefix: String = mods.iter().map(|m| format!("{m}-")).collect();
    Some(format!("<{prefix}{name_out}>"))
}

/// Leading `C-`, `S-`, `M-`/`A-`, `D-` modifiers, sorted and lowercase.
fn split_modifiers(inner: &str) -> (Vec<char>, &str) {
    let mut mods = Vec::new();
    let mut rest = inner;
    while rest.len() > 2 && rest.as_bytes()[1] == b'-' {
        let m = match rest.as_bytes()[0].to_ascii_lowercase() {
            b'c' => 'c',
            b's' => 's',
            b'm' | b'a' => 'm',
            b'd' => 'd',
            b't' => 't',
            _ => break,
        };
        if !mods.contains(&m) {
            mods.push(m);
        }
        rest = &rest[2..];
    }
    mods.sort_unstable();
    (mods, rest)
}

/// The key for a single-key lhs (`n`, `Ö`, `<C-d>`, `<M-h>`, `<S-Up>`);
/// `None` for sequences, `<leader>`/`<Plug>` and keys a terminal can't send.
fn parse_single_key(lhs: &str) -> Option<Key> {
    let mut chars = lhs.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        return Some(normalize(KeyCode::Char(first), KeyModifiers::NONE));
    }
    let inner = lhs.strip_prefix('<')?.strip_suffix('>')?;
    if inner.contains(['<', '>']) {
        return None;
    }
    let (mods, name) = split_modifiers(inner);
    let mut modifiers = KeyModifiers::NONE;
    for m in mods {
        modifiers |= match m {
            'c' => KeyModifiers::CONTROL,
            's' => KeyModifiers::SHIFT,
            'm' => KeyModifiers::ALT,
            _ => return None, // <D-..> / <T-..>: not sent by terminals
        };
    }
    let code = if name.chars().count() == 1 {
        let c = name.chars().next()?;
        if modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT) {
            return None; // <C-S-n>: most terminals can't tell it from <C-n>
        }
        let shifted =
            modifiers.contains(KeyModifiers::SHIFT) && !modifiers.contains(KeyModifiers::CONTROL) && c.is_alphabetic();
        KeyCode::Char(if shifted { c.to_uppercase().next()? } else { c })
    } else {
        let lower = name.to_lowercase();
        if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
            KeyCode::F(n)
        } else {
            match lower.as_str() {
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "pageup" => KeyCode::PageUp,
                "pagedown" => KeyCode::PageDown,
                "tab" if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
                "tab" => KeyCode::Tab,
                "cr" | "enter" | "return" => KeyCode::Enter,
                "esc" => KeyCode::Esc,
                "bs" | "backspace" => KeyCode::Backspace,
                "del" | "delete" => KeyCode::Delete,
                "insert" => KeyCode::Insert,
                "space" => KeyCode::Char(' '),
                "lt" => KeyCode::Char('<'),
                "bslash" => KeyCode::Char('\\'),
                "bar" => KeyCode::Char('|'),
                _ => return None,
            }
        }
    };
    Some(normalize(code, modifiers))
}

fn normalize(code: KeyCode, modifiers: KeyModifiers) -> Key {
    let mut mods = modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
    match code {
        KeyCode::Char(c) => {
            mods.remove(KeyModifiers::SHIFT);
            let c = if mods.contains(KeyModifiers::CONTROL) { c.to_ascii_lowercase() } else { c };
            (KeyCode::Char(c), mods)
        }
        KeyCode::BackTab => (KeyCode::BackTab, mods - KeyModifiers::SHIFT),
        other => (other, mods),
    }
}

// ---------------------------------------------------------------------------
// Asking the editor
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Editor {
    Nvim { bin: PathBuf, config_dir: PathBuf, appname: Option<String> },
    Vim { bin: PathBuf, vimrc: PathBuf, vim_dir: Option<PathBuf> },
}

struct Dump {
    maps: Vec<RawMap>,
    leader: Option<String>,
    source: String,
}

/// Runs once nvim has finished starting: fire VeryLazy (LazyVim defines its
/// keymaps there and it never fires headless), then write the normal-mode
/// keymaps as JSON to `$SCARLET_VIMKEYS_OUT` and quit. Keys that lazy.nvim
/// has stubbed out until their plugin loads get their rhs from the lazy spec.
const NVIM_DUMP_LUA: &str = r#"
local out = os.getenv("SCARLET_VIMKEYS_OUT")
local function dump()
  local lazy_keys, distro = {}, nil
  pcall(function()
    local cfg = require("lazy.core.config")
    distro = "lazy.nvim"
    if cfg.plugins["LazyVim"] then distro = "LazyVim"
    elseif cfg.plugins["AstroNvim"] or cfg.plugins["astronvim"] then distro = "AstroNvim"
    elseif cfg.plugins["NvChad"] then distro = "NvChad" end
    for _, p in pairs(cfg.plugins) do
      local ks = p._ and p._.handlers and p._.handlers.keys
      if type(ks) == "table" then
        for id, k in pairs(ks) do
          if k.mode == "n" and type(k.rhs) == "string" then lazy_keys[id] = k end
        end
      end
    end
  end)
  local parts = {}
  for _, m in ipairs(vim.api.nvim_get_keymap("n")) do
    local rhs, desc, expr = m.rhs, m.desc, m.expr == 1
    local cb = m.callback ~= nil
    if (rhs == nil or rhs == "") and cb then
      local k = lazy_keys[vim.api.nvim_replace_termcodes(m.lhs, true, true, true)]
      if k then rhs = k.rhs; desc = desc or k.desc; expr = k.expr == true; cb = false end
    end
    local ok, enc = pcall(vim.json.encode, { lhs = m.lhs, rhs = rhs or "", expr = expr, desc = desc or "", callback = cb })
    if ok then table.insert(parts, enc) end
  end
  local leader = vim.g.mapleader
  local head = vim.json.encode({ leader = type(leader) == "string" and leader or "", has_leader = type(leader) == "string", distro = distro or "" })
  local f = io.open(out, "w")
  if f then f:write("{\"meta\":" .. head .. ",\"maps\":[" .. table.concat(parts, ",") .. "]}"); f:close() end
end
vim.api.nvim_create_autocmd("VimEnter", { once = true, callback = function()
  vim.schedule(function()
    pcall(vim.api.nvim_exec_autocmds, "User", { pattern = "VeryLazy", modeline = false })
    vim.schedule(function() pcall(dump); vim.cmd("qa!") end)
  end)
end })
"#;

impl Editor {
    fn discover() -> Option<Editor> {
        let home = dirs::home_dir()?;
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let appname = std::env::var("NVIM_APPNAME").ok().filter(|s| !s.is_empty());
        let config_dir = config_home.join(appname.as_deref().unwrap_or("nvim"));
        let has_nvim_config = config_dir.join("init.lua").is_file() || config_dir.join("init.vim").is_file();
        if has_nvim_config {
            if let Some(bin) = find_binary("nvim") {
                return Some(Editor::Nvim { bin, config_dir, appname });
            }
            return None;
        }
        let vimrc = [home.join(".vimrc"), home.join(".vim").join("vimrc")].into_iter().find(|p| p.is_file())?;
        let vim_dir = Some(home.join(".vim")).filter(|p| p.is_dir());
        Some(Editor::Vim { bin: find_binary("vim")?, vimrc, vim_dir })
    }

    /// Changes whenever a file in the config (or the editor binary) changes.
    fn fingerprint(&self) -> String {
        let (bin, roots, label) = match self {
            Editor::Nvim { bin, config_dir, appname } => {
                (bin, vec![config_dir.clone()], format!("nvim:{}", appname.as_deref().unwrap_or("")))
            }
            Editor::Vim { bin, vimrc, vim_dir } => {
                (bin, std::iter::once(vimrc.clone()).chain(vim_dir.clone()).collect(), "vim".to_string())
            }
        };
        let mut stats = TreeStats::default();
        stats.add_file(bin);
        for root in &roots {
            stats.walk(root, 0);
        }
        format!(
            "v{CACHE_VERSION}|{label}|{}|{}|files={}|bytes={}|newest={}",
            bin.display(),
            roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(","),
            stats.files,
            stats.bytes,
            stats.newest,
        )
    }

    async fn extract(&self) -> Option<Dump> {
        match self {
            Editor::Nvim { bin, config_dir, appname } => {
                if lazy_plugins_missing(config_dir, appname.as_deref()) {
                    return None;
                }
                let lua_cmd = format!("lua {}", NVIM_DUMP_LUA.replace('\n', " "));
                let args = ["--headless", "-n", "-i", "NONE", "-c", lua_cmd.as_str()];
                let text = run_editor(bin, &args, appname.as_deref()).await?;
                let mut dump = parse_nvim_dump(&text)?;
                let distro = dump.source;
                let name = match appname {
                    Some(app) => format!("nvim [{app}]"),
                    None => "nvim".to_string(),
                };
                dump.source = if distro.is_empty() {
                    format!("{name} ({})", tilde(config_dir))
                } else {
                    format!("{name} ({distro} config)")
                };
                Some(dump)
            }
            Editor::Vim { bin, vimrc, .. } => {
                let vimrc_arg = vimrc.to_string_lossy().to_string();
                // `redir` reads the target path from the command line; the
                // path is spliced in by `run_editor` with spaces escaped.
                let args = [
                    "-es",
                    "-N",
                    "-n",
                    "-i",
                    "NONE",
                    "-u",
                    vimrc_arg.as_str(),
                    "-c",
                    "redir! > {OUT}",
                    "-c",
                    "silent nmap",
                    "-c",
                    "silent echo exists('g:mapleader') ? 'SCARLET_LEADER:' . g:mapleader : 'SCARLET_NOLEADER'",
                    "-c",
                    "redir END",
                    "-c",
                    "qa!",
                ];
                let text = run_editor(bin, &args, None).await?;
                let (maps, leader) = parse_vim_listing(&text);
                Some(Dump { maps, leader, source: format!("vim ({})", tilde(vimrc)) })
            }
        }
    }
}

/// Whether lazy.nvim would install plugins on startup. A headless start would
/// then download them and could rewrite `lazy-lock.json`, so extraction is
/// skipped rather than risk touching the user's setup. Only the default
/// install location (`stdpath("data")/lazy`) is checked; a custom one reads
/// as missing, which errs on the safe side.
fn lazy_plugins_missing(config_dir: &Path, appname: Option<&str>) -> bool {
    let Ok(lock) = std::fs::read_to_string(config_dir.join("lazy-lock.json")) else {
        return false;
    };
    let Ok(Value::Object(plugins)) = serde_json::from_str::<Value>(&lock) else {
        return true;
    };
    let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))
    else {
        return true;
    };
    let root = data_home.join(appname.unwrap_or("nvim")).join("lazy");
    plugins.keys().any(|name| !root.join(name).is_dir())
}

/// Runs the editor with stdin closed, output discarded and a timeout, and
/// returns what it wrote to the temp file (`$SCARLET_VIMKEYS_OUT`, or the
/// `{OUT}` placeholder in an argument).
async fn run_editor(bin: &Path, args: &[&str], appname: Option<&str>) -> Option<String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_nanos();
    let out = std::env::temp_dir().join(format!("scarlet-vimkeys-{}-{nanos}.txt", std::process::id()));
    let out_str = out.to_string_lossy().to_string();
    let escaped = out_str.replace(' ', "\\ ");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args.iter().map(|a| a.replace("{OUT}", &escaped)))
        .env(OUT_ENV, &out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(app) = appname {
        cmd.env("NVIM_APPNAME", app);
    }
    let mut child = cmd.spawn().ok()?;
    let finished = tokio::time::timeout(EDITOR_TIMEOUT, child.wait()).await;
    if finished.is_err() {
        let _ = child.kill().await;
    }
    let text = tokio::fs::read_to_string(&out).await.ok();
    let _ = tokio::fs::remove_file(&out).await;
    text
}

fn parse_nvim_dump(text: &str) -> Option<Dump> {
    let v: Value = serde_json::from_str(text).ok()?;
    let meta = v.get("meta")?;
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let leader = meta.get("has_leader").and_then(Value::as_bool).unwrap_or(false).then(|| str_of(meta, "leader"));
    let maps = v
        .get("maps")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|m| RawMap {
                    lhs: str_of(m, "lhs"),
                    rhs: str_of(m, "rhs"),
                    expr: m.get("expr").and_then(Value::as_bool).unwrap_or(false),
                    desc: str_of(m, "desc"),
                    callback: m.get("callback").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Dump { maps, leader, source: str_of(meta, "distro") })
}

/// Parses vim's `:nmap` listing. Each line is a 3-column mode, the lhs padded
/// to 12 cells, a noremap flag (`*`, `&` or space), a buffer-local flag (`@`
/// or space) and the rhs.
fn parse_vim_listing(text: &str) -> (Vec<RawMap>, Option<String>) {
    let mut maps = Vec::new();
    let mut leader = None;
    for line in text.lines() {
        if let Some(l) = line.strip_prefix("SCARLET_LEADER:") {
            leader = Some(l.to_string());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        if chars.len() < 4 || !chars[..3].iter().all(|c| " nvoxsilct!".contains(*c)) {
            continue;
        }
        let rest = &chars[3..];
        let lhs_len = rest.iter().position(|c| *c == ' ').unwrap_or(rest.len());
        if lhs_len == 0 {
            continue;
        }
        let lhs: String = rest[..lhs_len].iter().collect();
        let flags_at = (lhs_len + 1).max(12);
        if rest.len() < flags_at + 2 {
            continue;
        }
        if rest[flags_at + 1] == '@' {
            continue; // buffer-local
        }
        let rhs: String = rest[flags_at + 2..].iter().collect();
        let expr = looks_like_expr(&rhs);
        maps.push(RawMap { lhs, rhs, expr, ..RawMap::default() });
    }
    (maps, leader)
}

/// `:nmap` doesn't show `<expr>`, so recognise `cond ? 'a' : 'b'` by shape.
fn looks_like_expr(rhs: &str) -> bool {
    rhs.contains('?') && rhs.contains(':') && (rhs.contains('\'') || rhs.contains('"'))
}

#[derive(Default)]
struct TreeStats {
    files: u64,
    bytes: u64,
    newest: u128,
}

impl TreeStats {
    const MAX_DEPTH: usize = 12;
    const MAX_FILES: u64 = 20_000;

    fn add_file(&mut self, path: &Path) {
        if let Ok(meta) = std::fs::metadata(path) {
            self.files += 1;
            self.bytes += meta.len();
            let mtime =
                meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
            self.newest = self.newest.max(mtime);
        }
    }

    fn walk(&mut self, path: &Path, depth: usize) {
        if depth > Self::MAX_DEPTH || self.files > Self::MAX_FILES {
            return;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        if !meta.is_dir() {
            self.add_file(path);
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name() == ".git" {
                continue;
            }
            self.walk(&entry.path(), depth + 1);
        }
    }
}

fn find_binary(name: &str) -> Option<PathBuf> {
    let path_dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    // GUI launches often have a minimal PATH; try the usual install spots too.
    let fallback = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from);
    path_dirs.into_iter().chain(fallback).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn tilde(path: &Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

fn cache_path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("scarlet").join("vim-keys.json"))
}

fn read_cache(path: &Path, fingerprint: &str) -> Option<VimKeys> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if v.get("fingerprint")?.as_str()? != fingerprint {
        return None;
    }
    let entries = v
        .get("keys")?
        .as_array()?
        .iter()
        .filter_map(|e| {
            let pair = e.as_array()?;
            let lhs = pair.first()?.as_str()?.to_string();
            let motion = Motion::from_name(pair.get(1)?.as_str()?)?;
            Some((lhs, motion))
        })
        .collect();
    let leader = v.get("leader").and_then(Value::as_str).map(str::to_string);
    let source = v.get("source")?.as_str()?.to_string();
    Some(VimKeys::from_entries(entries, leader, source))
}

fn write_cache(path: &Path, fingerprint: &str, keys: &VimKeys) {
    let body = json!({
        "fingerprint": fingerprint,
        "source": keys.source,
        "leader": keys.leader,
        "keys": keys.entries.iter().map(|(lhs, m)| json!([lhs, m.name()])).collect::<Vec<_>>(),
    });
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, body.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(lhs: &str, rhs: &str) -> RawMap {
        RawMap { lhs: lhs.into(), rhs: rhs.into(), ..RawMap::default() }
    }

    fn keys(maps: &[RawMap]) -> VimKeys {
        VimKeys::build(maps, Some(" ".into()), "test".into())
    }

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn ch(c: char) -> KeyEvent {
        ev(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        ev(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// LazyVim's defaults as nvim_get_keymap reports them.
    fn lazyvim() -> Vec<RawMap> {
        vec![
            RawMap { expr: true, desc: "Down".into(), ..map("j", "v:count == 0 ? 'gj' : 'j'") },
            RawMap { expr: true, desc: "Up".into(), ..map("k", "v:count == 0 ? 'gk' : 'k'") },
            RawMap { expr: true, ..map("<Down>", "v:count == 0 ? 'gj' : 'j'") },
            RawMap { desc: "Go to Left Window".into(), ..map("<C-H>", "<C-W>h") },
            RawMap { desc: "Go to Right Window".into(), ..map("<C-L>", "<C-W>l") },
            RawMap { desc: "Go to Lower Window".into(), ..map("<C-J>", "<C-W>j") },
            RawMap { desc: "Prev Buffer".into(), ..map("<S-H>", "<Cmd>bprevious<CR>") },
            RawMap { desc: "Increase Window Height".into(), ..map("<C-Up>", "<Cmd>resize +2<CR>") },
            map("<Space>ff", ""),
            map("<Space>e", "<Cmd>Neotree<CR>"),
        ]
    }

    #[test]
    fn lazyvim_expr_motions_and_window_nav() {
        let k = keys(&lazyvim());
        assert_eq!(k.motion(&ch('j')), Some(Motion::Down));
        assert_eq!(k.motion(&ch('k')), Some(Motion::Up));
        assert_eq!(k.motion(&ev(KeyCode::Down, KeyModifiers::NONE)), Some(Motion::Down));
        assert_eq!(k.motion(&ctrl('h')), Some(Motion::FocusLeft));
        assert_eq!(k.motion(&ctrl('l')), Some(Motion::FocusRight));
        // Window up/down, buffer switching and resizing are not motions here.
        assert_eq!(k.motion(&ctrl('j')), None);
        assert_eq!(k.motion(&ch('H')), None);
        assert_eq!(k.motion(&ev(KeyCode::Up, KeyModifiers::CONTROL)), None);
        assert_eq!(k.entries().len(), 5);
    }

    #[test]
    fn colemak_remap_moves_like_the_target_key() {
        let k = keys(&[map("n", "j"), map("e", "k"), map("m", "h"), map("i", "l")]);
        assert_eq!(k.motion(&ch('n')), Some(Motion::Down));
        assert_eq!(k.motion(&ch('e')), Some(Motion::Up));
        assert_eq!(k.motion(&ch('m')), Some(Motion::Left));
        assert_eq!(k.motion(&ch('i')), Some(Motion::Right));
        assert_eq!(k.motion(&ch('j')), None);
    }

    #[test]
    fn cosmetic_recentering_is_ignored() {
        let k = keys(&[map("<C-D>", "<C-D>zz"), map("<C-u>", "<c-u>zzzv"), map("n", "nzzzv")]);
        assert_eq!(k.motion(&ctrl('d')), Some(Motion::HalfDown));
        assert_eq!(k.motion(&ctrl('u')), Some(Motion::HalfUp));
        assert_eq!(k.motion(&ch('n')), None);
    }

    #[test]
    fn tmux_navigator_and_wincmd_focus() {
        let k = keys(&[
            map("<C-h>", "<cmd><C-U>TmuxNavigateLeft<cr>"),
            map("<C-l>", ":<C-U>TmuxNavigateRight<CR>"),
            map("<M-h>", "<Cmd>wincmd h<CR>"),
            map("<M-l>", "<C-w><C-l>"),
            map("<M-H>", "<Cmd>wincmd H<CR>"), // moves the window: not focus
            map("<F2>", "<cmd>NavigatorLeft<cr>"),
        ]);
        assert_eq!(k.motion(&ctrl('h')), Some(Motion::FocusLeft));
        assert_eq!(k.motion(&ctrl('l')), Some(Motion::FocusRight));
        assert_eq!(k.motion(&ev(KeyCode::Char('h'), KeyModifiers::ALT)), Some(Motion::FocusLeft));
        assert_eq!(k.motion(&ev(KeyCode::Char('l'), KeyModifiers::ALT)), Some(Motion::FocusRight));
        assert_eq!(k.motion(&ev(KeyCode::Char('H'), KeyModifiers::ALT | KeyModifiers::SHIFT)), None);
        assert_eq!(k.motion(&ev(KeyCode::F(2), KeyModifiers::NONE)), Some(Motion::FocusLeft));
    }

    #[test]
    fn lua_callbacks_use_their_description() {
        let cb =
            |lhs: &str, desc: &str| RawMap { lhs: lhs.into(), desc: desc.into(), callback: true, ..RawMap::default() };
        let k = keys(&[
            cb("<C-h>", "Go to Left Window"),
            cb("<C-l>", "Move to right split"),
            cb("<C-Left>", "Resize split left"),
            cb("<C-Right>", "Move window right"),
            cb("H", "Left"),
        ]);
        assert_eq!(k.motion(&ctrl('h')), Some(Motion::FocusLeft));
        assert_eq!(k.motion(&ctrl('l')), Some(Motion::FocusRight));
        assert_eq!(k.motion(&ev(KeyCode::Left, KeyModifiers::CONTROL)), None);
        assert_eq!(k.motion(&ev(KeyCode::Right, KeyModifiers::CONTROL)), None);
        assert_eq!(k.motion(&ch('H')), None);
    }

    #[test]
    fn unrelated_and_multi_key_mappings_are_ignored() {
        let k = keys(&[
            map("<C-k>", "<cmd>cnext<CR>zz"),
            map("<C-f>", "<cmd>silent !tmux neww tmux-sessionizer<CR>"),
            map("J", "mzJ`z"),
            map("Q", "<Nop>"),
            map("gj", "j"),
            map("<Space>j", "<cmd>lprev<CR>zz"),
            map("<Plug>(foo)", "j"),
            map("<D-j>", "j"),
        ]);
        assert!(k.entries().is_empty());
        assert_eq!(k.motion(&ctrl('f')), None);
    }

    #[test]
    fn swedish_and_shifted_keys() {
        let k = keys(&[map("€", "$"), map("Ö", "gg"), map("<S-l>", "G"), map("<S-Down>", "<C-f>")]);
        assert_eq!(k.motion(&ch('€')), Some(Motion::LastColumn));
        assert_eq!(k.motion(&ch('Ö')), Some(Motion::Top));
        // Terminals report shift+l as 'L', with or without the SHIFT flag.
        assert_eq!(k.motion(&ev(KeyCode::Char('L'), KeyModifiers::SHIFT)), Some(Motion::Bottom));
        assert_eq!(k.motion(&ch('L')), Some(Motion::Bottom));
        assert_eq!(k.motion(&ev(KeyCode::Down, KeyModifiers::SHIFT)), Some(Motion::PageDown));
        assert_eq!(k.motion(&ev(KeyCode::Down, KeyModifiers::NONE)), None);
    }

    #[test]
    fn vim_listing_is_parsed() {
        let listing = "\n\
n  x           *@:echo 1<CR>\n\
n  ,w          * :w<CR>\n\
n  j           * v:count == 0 ? 'gj' : 'j'\n\
n  n           * j\n\
n  <C-L>       * :<C-U>TmuxNavigateRight<CR>\n\
n  <C-H>       * <C-W>h\n\
n  <C-D>         <C-D>zz\n\
n  Ö           * $\n\
SCARLET_LEADER:,\n";
        let (maps, leader) = parse_vim_listing(listing);
        assert_eq!(leader.as_deref(), Some(","));
        assert!(maps.iter().all(|m| m.lhs != "x"), "buffer-local skipped");
        let k = keys(&maps);
        assert_eq!(k.motion(&ch('j')), Some(Motion::Down));
        assert_eq!(k.motion(&ch('n')), Some(Motion::Down));
        assert_eq!(k.motion(&ctrl('l')), Some(Motion::FocusRight));
        assert_eq!(k.motion(&ctrl('h')), Some(Motion::FocusLeft));
        assert_eq!(k.motion(&ctrl('d')), Some(Motion::HalfDown));
        assert_eq!(k.motion(&ch('Ö')), Some(Motion::LastColumn));
    }

    #[test]
    fn cache_round_trips_and_checks_fingerprint() {
        let dir = std::env::temp_dir().join(format!("scarlet-vimkeys-test-{}", std::process::id()));
        let path = dir.join("vim-keys.json");
        let k = keys(&[map("n", "j"), map("<C-H>", "<C-W>h")]);
        write_cache(&path, "fp1", &k);
        assert!(read_cache(&path, "fp2").is_none());
        let cached = read_cache(&path, "fp1").expect("cache hit");
        assert_eq!(cached.source, "test");
        assert_eq!(cached.leader.as_deref(), Some(" "));
        assert_eq!(cached.motion(&ch('n')), Some(Motion::Down));
        assert_eq!(cached.motion(&ctrl('h')), Some(Motion::FocusLeft));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Runs against this machine's real editor config:
    /// `cargo test vimkeys::tests::real_config -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn real_config() {
        if let Some(path) = cache_path() {
            let _ = std::fs::remove_file(path);
        }
        for run in ["cold", "cached"] {
            let start = std::time::Instant::now();
            let keys = VimKeys::load().await;
            let elapsed = start.elapsed();
            println!("{run}: {elapsed:?}");
            match keys {
                Some(k) => {
                    println!("  source={} leader={:?}", k.source, k.leader);
                    for (lhs, m) in k.entries() {
                        println!("  {lhs:<12} -> {m:?}");
                    }
                }
                None => println!("  no vim config found"),
            }
        }
    }

    /// Plain vim fallback against a throwaway vimrc (needs `vim` on PATH):
    /// `cargo test vimkeys::tests::real_vim -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn real_vim() {
        let dir = std::env::temp_dir().join(format!("scarlet-vimrc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let vimrc = dir.join("vimrc");
        std::fs::write(
            &vimrc,
            "let mapleader = ','\nnnoremap n j\nnmap <C-d> <C-d>zz\nnnoremap <expr> k v:count == 0 ? 'gk' : 'k'\nnnoremap <C-h> <C-w>h\nnnoremap ,w :w<CR>\n",
        )
        .unwrap();
        let editor = Editor::Vim { bin: find_binary("vim").expect("vim on PATH"), vimrc, vim_dir: None };
        let dump = editor.extract().await.expect("vim dump");
        let k = VimKeys::build(&dump.maps, dump.leader, dump.source);
        println!("source={} leader={:?} entries={:?}", k.source, k.leader, k.entries());
        assert_eq!(k.leader.as_deref(), Some(","));
        assert_eq!(k.motion(&ch('n')), Some(Motion::Down));
        assert_eq!(k.motion(&ch('k')), Some(Motion::Up));
        assert_eq!(k.motion(&ctrl('d')), Some(Motion::HalfDown));
        assert_eq!(k.motion(&ctrl('h')), Some(Motion::FocusLeft));
        let _ = std::fs::remove_dir_all(dir);
    }
}
