use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nvim_rs::UiAttachOptions;
use nvim_rs::create::tokio::new_child_cmd;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;
use tokio::sync::mpsc::UnboundedReceiver;
use winit::event_loop::EventLoopProxy;

use super::handler::{RedrawHandler, Writer};
use super::launch;
use super::{NvimCommand, coalesce};
use crate::app::UserEvent;
use crate::config::NvimLaunch;

/// Runs until nvim is gone for good (across `:restart`s), then sends
/// `UserEvent::NvimExited`.
pub async fn run(
    launch: NvimLaunch,
    multigrid: bool,
    cols: u64,
    rows: u64,
    proxy: EventLoopProxy<UserEvent>,
    commands: UnboundedReceiver<NvimCommand>,
) {
    let result = session(&launch, multigrid, cols, rows, proxy.clone(), commands)
        .await
        .map_err(|err| format!("{err:#}"));
    let _ = proxy.send_event(UserEvent::NvimExited(result));
}

async fn session(
    launch: &NvimLaunch,
    multigrid: bool,
    cols: u64,
    rows: u64,
    proxy: EventLoopProxy<UserEvent>,
    mut commands: UnboundedReceiver<NvimCommand>,
) -> Result<String> {
    let started = Instant::now();
    let mut cmd = launch::command(launch);
    // a restarted server gets attached at the last size we sent
    let mut size = (cols, rows);
    let mut restarted = false;
    loop {
        log::info!("starting: {:?}", cmd.as_std());
        let restart = Arc::new(Mutex::new(None));
        let handler = RedrawHandler::new(proxy.clone(), started, restart.clone());
        let (nvim, mut io, mut child) = new_child_cmd(&mut cmd, handler)
            .await
            .with_context(|| format!("cannot start {:?}", cmd.as_std().get_program()))?;
        launch::bind_to_parent(&child);
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward_stderr(stderr));
        }

        let mut opts = UiAttachOptions::new();
        opts.set_rgb(true)
            .set_linegrid_external(true)
            .set_multigrid_external(multigrid);
        nvim.ui_attach(size.0 as i64, size.1 as i64, &opts)
            .await
            .context("nvim_ui_attach failed")?;
        log::info!("ui_attach done after {:?}", started.elapsed());
        // spawned so keys don't queue behind it
        tokio::spawn(install_preview(nvim.clone()));
        if restarted {
            // Unlike --embed, a `:restart`ed server keeps running when its UI
            // goes away.
            if let Err(err) = nvim.exec_lua(EXIT_WITH_GUI, vec![]).await {
                log::warn!(":restart: cannot tie the server to the window: {err}");
            }
        }

        // Commands sent before the attach are still queued in the channel.
        // If the window side hangs up first we keep waiting for nvim to exit.
        let io_result = {
            let commands_done = command_loop(nvim.clone(), &mut commands, &mut size);
            tokio::pin!(commands_done);
            tokio::select! {
                result = &mut io => result,
                () = &mut commands_done => (&mut io).await,
            }
        };
        // Keep `nvim` alive until here. Dropping the last ref closes stdin,
        // which makes nvim quit.
        drop(nvim);
        match io_result {
            Ok(Ok(())) => {}
            Ok(Err(err)) if err.is_channel_closed() => {}
            Ok(Err(err)) => log::warn!("RPC loop ended with an error: {err}"),
            Err(err) => log::warn!("RPC task aborted: {err}"),
        }
        let status = child.wait().await.context("waiting for the nvim process")?;
        let Some(addr) = restart.lock().unwrap().take() else {
            return Ok(format!("nvim exited ({status})"));
        };
        // old server is gone, go through the bridge to the new one
        cmd = launch::bridge_command(launch, &addr)
            .context(":restart is not supported with a native Windows nvim")?;
        restarted = true;
        let _ = proxy.send_event(UserEvent::NvimRestarted);
    }
}

/// Makes a `:restart`ed server exit when our UI disappears without nvim
/// exiting (GUI crashed or killed), like `--embed` does. SIGTERM if there are
/// unsaved changes, so the swap files stay around for recovery.
const EXIT_WITH_GUI: &str = r#"
vim.api.nvim_create_autocmd('UILeave', {
  desc = 'right_editor: exit with the GUI',
  callback = function()
    if #vim.api.nvim_list_uis() > 0 or vim.v.exiting ~= vim.NIL then
      return
    end
    vim.schedule(function()
      if #vim.fn.getbufinfo({ bufmodified = 1 }) == 0 then
        vim.cmd('qall!')
      else
        vim.uv.kill(vim.uv.os_getpid(), 'sigterm')
      end
    end)
  end,
})
"#;

async fn install_preview(nvim: nvim_rs::Neovim<Writer>) {
    let result = async {
        // 0 = our own channel
        let info = nvim.get_chan_info(0).await?;
        let id = info
            .iter()
            .find(|(k, _)| k.as_str() == Some("id"))
            .and_then(|(_, v)| v.as_i64())
            .unwrap_or(0);
        nvim.exec_lua(crate::preview::LUA, vec![id.into()]).await?;
        Ok::<_, Box<nvim_rs::error::CallError>>(())
    }
    .await;
    if let Err(err) = result {
        log::warn!("markdown preview not installed in nvim: {err}");
    }
}

/// One request in flight at a time, so keys can't get reordered. Pipelining
/// wouldn't help: a slow reply means nvim's main loop is busy and it wouldn't
/// look at the next keys any sooner.
async fn command_loop(
    nvim: nvim_rs::Neovim<Writer>,
    commands: &mut UnboundedReceiver<NvimCommand>,
    size: &mut (u64, u64),
) {
    let mut pending = Vec::new();
    while let Some(first) = commands.recv().await {
        // batch whatever piled up during the last reply, so a key waits at
        // most one round trip
        pending.push(first);
        while let Ok(next) = commands.try_recv() {
            pending.push(next);
        }
        for cmd in coalesce(pending.drain(..)) {
            if let NvimCommand::Resize { cols, rows } = cmd {
                *size = (cols, rows);
            }
            execute(&nvim, cmd).await;
        }
    }
}

// logged, usually means nvim was busy
const SLOW_RESPONSE: Duration = Duration::from_millis(50);

async fn execute(nvim: &nvim_rs::Neovim<Writer>, cmd: NvimCommand) {
    let started = Instant::now();
    let what = match &cmd {
        NvimCommand::Input(keys) => format!("nvim_input {keys:?}"),
        NvimCommand::Mouse { button, action, .. } => format!("nvim_input_mouse {button} {action}"),
        NvimCommand::Resize { cols, rows } => format!("ui_try_resize {cols}x{rows}"),
        NvimCommand::Focus(gained) => format!("ui_set_focus {gained}"),
        NvimCommand::Paste(text) => format!("nvim_paste {} bytes", text.len()),
        NvimCommand::Quit => "confirm qa".to_string(),
        NvimCommand::Lua(code, args) => format!("exec_lua {code:?} {args:?}"),
    };
    let result = match cmd {
        NvimCommand::Input(keys) => nvim.input(&keys).await.map(drop),
        NvimCommand::Mouse {
            button,
            action,
            modifier,
            grid,
            row,
            col,
        } => {
            nvim.input_mouse(
                button,
                action,
                &modifier,
                grid as i64,
                row as i64,
                col as i64,
            )
            .await
        }
        NvimCommand::Resize { cols, rows } => nvim.ui_try_resize(cols as i64, rows as i64).await,
        NvimCommand::Focus(gained) => nvim.ui_set_focus(gained).await,
        // crlf=true, converts Windows line endings
        NvimCommand::Paste(text) => nvim.paste(&text, true, -1).await.map(drop),
        NvimCommand::Lua(code, args) => nvim.exec_lua(&code, args).await.map(drop),
        NvimCommand::Quit => {
            // `confirm qa` may block on a prompt, and the answer keys have to
            // get through, so don't await it here.
            let nvim = nvim.clone();
            tokio::spawn(async move {
                if let Err(err) = nvim.command("confirm qa").await {
                    log::debug!("confirm qa: {err}");
                }
            });
            return;
        }
    };
    let elapsed = started.elapsed();
    if elapsed > SLOW_RESPONSE {
        log::warn!(
            "slow nvim reply to {what}: {} ms",
            elapsed.as_millis()
        );
    }
    if let Err(err) = result {
        log::warn!("nvim request failed ({what}): {err}");
    }
}

async fn forward_stderr(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        log::warn!("nvim stderr: {line}");
    }
}
