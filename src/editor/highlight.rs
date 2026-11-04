use crate::nvim::redraw::HlAttr;

/// -1 in default_colors_set.
pub const UNSET: u32 = u32::MAX;

const FALLBACK_FG: u32 = 0xffffff;
const FALLBACK_BG: u32 = 0x000000;
const FALLBACK_SP: u32 = 0xff0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultColors {
    pub fg: u32,
    pub bg: u32,
    pub sp: u32,
}

/// Defaults and `reverse` already resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub fg: u32,
    pub bg: u32,
    pub sp: u32,
}

/// Indexed by hl id, 0 is the default highlight.
#[derive(Debug)]
pub struct HlTable {
    attrs: Vec<HlAttr>,
    defaults: DefaultColors,
}

impl Default for HlTable {
    fn default() -> Self {
        Self {
            attrs: vec![HlAttr::default()],
            defaults: DefaultColors {
                fg: FALLBACK_FG,
                bg: FALLBACK_BG,
                sp: FALLBACK_SP,
            },
        }
    }
}

impl HlTable {
    /// true if the id was redefined, so cells using it need a redraw.
    pub fn define(&mut self, id: u64, attr: HlAttr) -> bool {
        let id = id as usize;
        let existed = id < self.attrs.len();
        if !existed {
            self.attrs.resize_with(id + 1, HlAttr::default);
        }
        self.attrs[id] = attr;
        existed && id != 0
    }

    pub fn set_defaults(&mut self, fg: u32, bg: u32, sp: u32) {
        let pick = |c: u32, fallback: u32| if c == UNSET { fallback } else { c };
        self.defaults = DefaultColors {
            fg: pick(fg, FALLBACK_FG),
            bg: pick(bg, FALLBACK_BG),
            sp: pick(sp, FALLBACK_SP),
        };
    }

    pub fn defaults(&self) -> DefaultColors {
        self.defaults
    }

    pub fn attr(&self, id: u32) -> &HlAttr {
        self.attrs.get(id as usize).unwrap_or(&self.attrs[0])
    }

    pub fn colors(&self, id: u32) -> Colors {
        let attr = self.attr(id);
        let fg = attr.foreground.unwrap_or(self.defaults.fg);
        let bg = attr.background.unwrap_or(self.defaults.bg);
        let sp = attr.special.unwrap_or(fg);
        if attr.reverse {
            Colors { fg: bg, bg: fg, sp }
        } else {
            Colors { fg, bg, sp }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_defaults_and_reverse() {
        let mut hl = HlTable::default();
        hl.set_defaults(0xeeeeee, 0x111111, UNSET);
        assert!(!hl.define(
            3,
            HlAttr {
                foreground: Some(0xff0000),
                reverse: true,
                ..Default::default()
            }
        ));
        assert_eq!(
            hl.colors(3),
            Colors {
                fg: 0x111111,
                bg: 0xff0000,
                sp: 0xff0000
            }
        );
        assert_eq!(hl.colors(0).bg, 0x111111);
        // unknown id falls back to 0
        assert_eq!(hl.colors(99).fg, 0xeeeeee);
        assert!(hl.define(3, HlAttr::default()));
    }
}
