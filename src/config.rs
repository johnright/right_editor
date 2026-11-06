use anyhow::{Context, Result, bail};

use crate::renderer::raster::Antialias;

#[derive(Debug, Clone)]
pub struct Config {
    pub log_filter: String,
    /// None = Mailbox if available, else Fifo.
    pub present_mode: Option<wgpu::PresentMode>,
    /// until nvim sends default_colors_set
    pub background: [u8; 3],
    /// None = take it from `guifont`. Same for the size (pt).
    pub font: Option<String>,
    pub font_size: Option<f32>,
    pub antialias: Antialias,
    pub font_weight: Option<u16>,
    pub hinting: bool,
    pub latency: bool,
    /// 0 = off. Short moves take half.
    pub cursor_animation_ms: u64,
    pub cursor_trail: f32,
    pub scroll_animation_ms: u64,
    pub text_fade_ms: u64,
    pub text_slide_ms: u64,
    pub multigrid: bool,
    pub floating_shadow: bool,
    pub backdrop: f32,
    pub opacity: f32,
    pub blur: bool,
    pub hidden: bool,
    pub preview_width: f32,
    pub preview_font: Option<String>,
    pub preview_font_size: Option<f32>,
    pub nvim: NvimLaunch,
}

#[derive(Debug, Clone)]
pub struct NvimLaunch {
    pub nvim: String,
    pub wsl: String,
    pub distro: Option<String>,
    /// Linux path when going through WSL
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub no_wsl: bool,
    pub args: Vec<String>,
}

impl Default for NvimLaunch {
    fn default() -> Self {
        Self {
            nvim: "nvim".to_string(),
            wsl: "wsl.exe".to_string(),
            distro: None,
            cwd: None,
            shell: None,
            no_wsl: false,
            args: Vec::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // RUST_LOG doesn't make it from WSL to Windows unless it's in WSLENV,
            // hence a usable default (and `--log`).
            log_filter: std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "warn,right_editor=info".to_string()),
            present_mode: None,
            background: [0x1e, 0x1e, 0x2e],
            font: None,
            font_size: None,
            antialias: Antialias::Subpixel,
            font_weight: None,
            hinting: false,
            latency: false,
            cursor_animation_ms: 60,
            cursor_trail: 0.4,
            scroll_animation_ms: 120,
            text_fade_ms: 70,
            text_slide_ms: 0,
            multigrid: true,
            floating_shadow: true,
            backdrop: 0.3,
            opacity: 1.0,
            blur: false,
            hidden: false,
            preview_width: 0.45,
            preview_font: None,
            preview_font_size: None,
            nvim: NvimLaunch::default(),
        }
    }
}

const USAGE: &str = "\
usage: right_editor [options] [file...] [-- arguments for nvim]

  --log <filter>          log filter, e.g. \"info\" or \"warn,right_editor=debug\"
  --present-mode <mode>   fifo | fifo-relaxed | mailbox | immediate (default: mailbox if available, else fifo)
  --bg <#rrggbb>          initial background color
  --font <family>         font (default: from 'guifont', then Cascadia Mono)
  --font-size <points>    font size (default: from 'guifont', then 12)
  --antialias <mode>      subpixel | grayscale (default: subpixel, if ClearType is on)
  --font-weight <weight>  text weight, 100-900 (default: from 'guifont' :wNNN, then 400)
  --hinting <mode>        none | full (default: none)
  --latency               log the latency of every input (key → flush → frame)
  --cursor-animation <ms> cursor animation, 0 = off (default: 60)
  --cursor-trail <0-1>    cursor trail: how far the leading corners run ahead (default: 0.4)
  --scroll-animation <ms> scroll animation, 0 = off (default: 120)
  --text-fade <ms>        fade-in of newly typed characters, 0 = off (default: 70)
  --text-slide <ms>       slide of the text to the right when typing mid-line,
                          0 = off (default: 0)
  --no-multigrid          everything on a single grid (no ext_multigrid)
  --no-floating-shadow    no shadow under floating windows
  --backdrop <0-1>        dimming behind a focused float, 0 = off (default: 0.3)
  --opacity <0.1-1>       opacity of the default background, text stays opaque (default: 1)
  --blur                  blur what's behind the window (Windows 11 acrylic, needs opacity < 1)
  --hidden                for automated tests: never shown or focused, no window.toml,
                          a posted WM_APP+0x52 (wParam = n) saves %TEMP%\\re_snap_<pid>_<n>.png
  --preview-width <0.2-0.8>  width of the markdown preview panel (default: 0.45)
  --preview-font <family>    font of the preview (default: Segoe UI)
  --preview-font-size <points> text size of the preview (default: the editor's)

  --nvim <path>           nvim executable (default: nvim)
  --wsl <path>            WSL executable (default: wsl.exe)
  --distro <name>         WSL distro (default: from the current directory)
  --cwd <path>            working directory (default: from the current directory)
  --shell <path>          start nvim through this login shell (e.g. /bin/zsh)
  --no-wsl                start nvim directly, without wsl.exe
  --config <path>         configuration file (default: see below)
  -h, --help              show this message

The same options (without --) can go in config.toml, in
%APPDATA%\\right_editor\\ on Windows or ~/.config/right_editor/ on Linux;
the command line wins. E.g.:  font = \"FiraCode Nerd Font\"
font-weight = 500,  latency = true,  args = [\"-u\", \"~/alt.lua\"].

useful environment variables: WGPU_BACKEND=dx12|vulkan|gl";

impl Config {
    /// defaults < config.toml < command line
    pub fn load(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let args: Vec<String> = args.into_iter().collect();
        let mut config = Self::default();
        let explicit = args
            .iter()
            .position(|a| a == "--config")
            .map(|i| {
                args.get(i + 1)
                    .cloned()
                    .context("--config requires a value")
            })
            .transpose()?
            .map(std::path::PathBuf::from);
        let path = explicit
            .clone()
            .or_else(|| config_dir().map(|d| d.join("config.toml")));
        if let Some(path) = path {
            match std::fs::read_to_string(&path) {
                Ok(text) => config
                    .apply_file(&text)
                    .with_context(|| format!("error in {}", path.display()))?,
                // missing default file is fine, a missing --config one isn't
                Err(err) if explicit.is_none() && err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err).with_context(|| format!("reading {}", path.display()));
                }
            }
        }
        config.apply_args(args)?;
        if cfg!(windows) && !config.nvim.no_wsl {
            config.nvim.infer_wsl_location();
        }
        Ok(config)
    }

    fn apply_file(&mut self, text: &str) -> Result<()> {
        let table: toml::Table = text.parse()?;
        for (key, value) in table {
            match (key.as_str(), value) {
                ("args", toml::Value::Array(items)) => {
                    for item in items {
                        let toml::Value::String(arg) = item else {
                            bail!("args: expected a list of strings");
                        };
                        self.nvim.args.push(arg);
                    }
                }
                (_, toml::Value::Boolean(on)) => self.set_flag(&key, on)?,
                (_, toml::Value::String(s)) => self.set(&key, &s)?,
                (_, toml::Value::Integer(n)) => self.set(&key, &n.to_string())?,
                (_, toml::Value::Float(n)) => self.set(&key, &n.to_string())?,
                (_, other) => bail!("{key}: unsupported value ({})", other.type_str()),
            }
        }
        Ok(())
    }

    fn apply_args(&mut self, args: Vec<String>) -> Result<()> {
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                "--" => {
                    self.nvim.args.extend(args.by_ref());
                    break;
                }
                "--config" => {
                    args.next(); // already read in load()
                }
                "--latency" | "--no-wsl" | "--blur" | "--hidden" => {
                    self.set_flag(&arg[2..], true)?
                }
                "--no-multigrid" => self.multigrid = false,
                "--no-floating-shadow" => self.floating_shadow = false,
                flag if flag.starts_with("--") => {
                    let value = args
                        .next()
                        .with_context(|| format!("{flag} requires a value"))?;
                    self.set(&flag[2..], &value)
                        .map_err(|e| anyhow::anyhow!("{e}\n\n{USAGE}"))?;
                }
                other if other.starts_with('-') => {
                    bail!("unknown argument: {other}\n\n{USAGE}")
                }
                // TODO: Windows paths (C:\...) go to nvim untranslated
                _ => self.nvim.args.push(arg),
            }
        }
        Ok(())
    }

    fn set_flag(&mut self, key: &str, on: bool) -> Result<()> {
        match key {
            "latency" => self.latency = on,
            "no-wsl" => self.nvim.no_wsl = on,
            "multigrid" => self.multigrid = on,
            "floating-shadow" => self.floating_shadow = on,
            "blur" => self.blur = on,
            "hidden" => self.hidden = on,
            _ => bail!("unknown or non-boolean option: {key}"),
        }
        Ok(())
    }

    /// Shared by config.toml and the CLI, same validation for both.
    fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "log" => self.log_filter = value.to_string(),
            "present-mode" => self.present_mode = Some(parse_present_mode(value)?),
            "bg" => self.background = parse_rgb(value)?,
            "font" => self.font = Some(value.to_string()),
            "font-size" => {
                self.font_size = Some(
                    value
                        .parse()
                        .ok()
                        .filter(|s: &f32| *s > 0.0)
                        .with_context(|| format!("invalid size: {value}"))?,
                )
            }
            "antialias" => {
                self.antialias = match value {
                    "subpixel" => Antialias::Subpixel,
                    "grayscale" => Antialias::Grayscale,
                    other => bail!("invalid antialias: {other}"),
                }
            }
            "font-weight" => self.font_weight = Some(parse_weight(value)?),
            "hinting" => {
                self.hinting = match value {
                    "full" => true,
                    "none" => false,
                    other => bail!("invalid hinting: {other}"),
                }
            }
            "cursor-animation" => self.cursor_animation_ms = parse_ms(value)?,
            "cursor-trail" => {
                self.cursor_trail = value
                    .parse()
                    .ok()
                    .filter(|t: &f32| (0.0..=1.0).contains(t))
                    .with_context(|| format!("invalid trail: {value} (0-1)"))?;
            }
            "scroll-animation" => self.scroll_animation_ms = parse_ms(value)?,
            "text-fade" => self.text_fade_ms = parse_ms(value)?,
            "text-slide" => self.text_slide_ms = parse_ms(value)?,
            "backdrop" => {
                self.backdrop = value
                    .parse()
                    .ok()
                    .filter(|b: &f32| (0.0..=1.0).contains(b))
                    .with_context(|| format!("invalid backdrop: {value} (0-1)"))?;
            }
            "opacity" => {
                self.opacity = value
                    .parse()
                    .ok()
                    .filter(|o: &f32| (0.1..=1.0).contains(o))
                    .with_context(|| format!("invalid opacity: {value} (0.1-1)"))?;
            }
            "preview-width" => {
                self.preview_width = value
                    .parse()
                    .ok()
                    .filter(|w: &f32| (0.2..=0.8).contains(w))
                    .with_context(|| format!("invalid width: {value} (0.2-0.8)"))?;
            }
            "preview-font" => self.preview_font = Some(value.to_string()),
            "preview-font-size" => {
                self.preview_font_size = Some(
                    value
                        .parse()
                        .ok()
                        .filter(|s: &f32| *s > 0.0)
                        .with_context(|| format!("invalid size: {value}"))?,
                )
            }
            "nvim" => self.nvim.nvim = value.to_string(),
            "wsl" => self.nvim.wsl = value.to_string(),
            "distro" => self.nvim.distro = Some(value.to_string()),
            "cwd" => self.nvim.cwd = Some(value.to_string()),
            "shell" => self.nvim.shell = Some(value.to_string()),
            _ => bail!("unknown option: {key}"),
        }
        Ok(())
    }
}

/// %APPDATA%\right_editor on Windows, $XDG_CONFIG_HOME/right_editor
/// (or ~/.config/right_editor) elsewhere. Also holds window.toml.
pub fn config_dir() -> Option<std::path::PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
            })
    };
    base.map(|b| b.join("right_editor"))
}

pub fn parse_weight(value: &str) -> Result<u16> {
    value
        .parse()
        .ok()
        .filter(|w| (100..=900).contains(w))
        .with_context(|| format!("invalid weight: {value} (100-900)"))
}

impl NvimLaunch {
    /// Started from a WSL shell, the cwd is a UNC path (`\\wsl.localhost\<distro>\...`),
    /// so pull the distro and Linux dir out of it. For `C:\...` wsl.exe already
    /// translates the cwd itself.
    fn infer_wsl_location(&mut self) {
        let Ok(dir) = std::env::current_dir() else {
            return;
        };
        let Some((distro, linux_dir)) = crate::nvim::launch::parse_wsl_unc(&dir.to_string_lossy())
        else {
            return;
        };
        if self
            .distro
            .as_ref()
            .is_some_and(|d| !d.eq_ignore_ascii_case(&distro))
        {
            // cwd belongs to another distro than the one asked for
            return;
        }
        self.distro.get_or_insert(distro);
        self.cwd.get_or_insert(linux_dir);
    }
}

fn parse_ms(s: &str) -> Result<u64> {
    s.parse()
        .ok()
        .filter(|ms| *ms <= 2000)
        .with_context(|| format!("invalid duration: {s} (milliseconds, 0-2000)"))
}

fn parse_present_mode(s: &str) -> Result<wgpu::PresentMode> {
    Ok(match s {
        "fifo" => wgpu::PresentMode::Fifo,
        "fifo-relaxed" => wgpu::PresentMode::FifoRelaxed,
        "mailbox" => wgpu::PresentMode::Mailbox,
        "immediate" => wgpu::PresentMode::Immediate,
        _ => bail!("invalid present mode: {s}"),
    })
}

fn parse_rgb(s: &str) -> Result<[u8; 3]> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        bail!("invalid color: {s} (expected #rrggbb)");
    }
    let v = u32::from_str_radix(hex, 16).with_context(|| format!("invalid color: {s}"))?;
    Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_then_command_line() {
        let mut c = Config::default();
        c.apply_file(
            "font = \"FiraCode Nerd Font\"\nfont-weight = 500\nlatency = true\ncursor-trail = 0.6\nargs = [\"-u\", \"x.lua\"]\n",
        )
        .unwrap();
        assert_eq!(c.font.as_deref(), Some("FiraCode Nerd Font"));
        assert_eq!(
            (c.font_weight, c.latency, c.cursor_trail),
            (Some(500), true, 0.6)
        );
        c.apply_args(vec!["--font-weight".into(), "600".into(), "file.rs".into()])
            .unwrap();
        assert_eq!(c.font_weight, Some(600));
        assert_eq!(c.nvim.args, ["-u", "x.lua", "file.rs"]);
        assert!(c.apply_file("pippo = 1").is_err());
        assert!(c.apply_file("font-weight = 50").is_err());
    }

    #[test]
    fn parses_colors() {
        assert_eq!(parse_rgb("#1e1e2e").unwrap(), [0x1e, 0x1e, 0x2e]);
        assert_eq!(parse_rgb("ff0080").unwrap(), [0xff, 0x00, 0x80]);
        assert!(parse_rgb("#fff").is_err());
    }
}
