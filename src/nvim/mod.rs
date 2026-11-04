mod handler;
pub mod launch;
pub mod redraw;
mod session;

pub use session::run;

/// Sent from the window thread, run strictly in order.
#[derive(Debug, Clone, PartialEq)]
pub enum NvimCommand {
    Input(String),
    Mouse {
        button: &'static str,
        action: &'static str,
        modifier: String,
        // 0 without ext_multigrid; row/col are relative to this grid
        grid: u64,
        row: u64,
        col: u64,
    },
    Resize {
        cols: u64,
        rows: u64,
    },
    Focus(bool),
    Paste(String),
    /// Window close. nvim decides, and asks if there are unsaved changes.
    Quit,
    Lua(String, Vec<rmpv::Value>),
}

/// Merges runs of keys into one nvim_input and runs of resizes into the last
/// one. Order is preserved otherwise.
pub fn coalesce(commands: impl IntoIterator<Item = NvimCommand>) -> Vec<NvimCommand> {
    let mut out: Vec<NvimCommand> = Vec::new();
    for cmd in commands {
        match (out.last_mut(), cmd) {
            (Some(NvimCommand::Input(keys)), NvimCommand::Input(more)) => keys.push_str(&more),
            (Some(last @ NvimCommand::Resize { .. }), cmd @ NvimCommand::Resize { .. }) => {
                *last = cmd
            }
            (_, cmd) => out.push(cmd),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_keeps_order() {
        use NvimCommand::*;
        let cmds = vec![
            Input("a".into()),
            Input("<C-x>".into()),
            Resize { cols: 10, rows: 5 },
            Resize { cols: 20, rows: 6 },
            Input("b".into()),
            Focus(true),
            Input("c".into()),
        ];
        assert_eq!(
            coalesce(cmds),
            vec![
                Input("a<C-x>".into()),
                Resize { cols: 20, rows: 6 },
                Input("b".into()),
                Focus(true),
                Input("c".into()),
            ]
        );
    }
}
