use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use nvim_rs::compat::tokio::Compat;
use nvim_rs::{Handler, Neovim, Value};
use tokio::process::ChildStdin;
use winit::event_loop::EventLoopProxy;

use super::redraw::{RedrawEvent, parse_redraw};
use crate::app::UserEvent;

pub type Writer = Compat<ChildStdin>;

/// nvim-rs calls us one notification at a time, in order, on the I/O loop.
/// Parse only; each batch goes to the window thread when its `flush` arrives.
#[derive(Clone)]
pub struct RedrawHandler {
    // Never contended, the Mutex is only there because Handler must be Sync.
    pending: Arc<Mutex<Vec<RedrawEvent>>>,
    proxy: EventLoopProxy<UserEvent>,
    started: Instant,
    first_flush: Arc<std::sync::Once>,
    /// Set by the `restart` UI event; the session reads it once the channel closes.
    restart: Arc<Mutex<Option<String>>>,
}

impl RedrawHandler {
    pub fn new(
        proxy: EventLoopProxy<UserEvent>,
        started: Instant,
        restart: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            pending: Arc::default(),
            proxy,
            started,
            first_flush: Arc::new(std::sync::Once::new()),
            restart,
        }
    }

    fn on_redraw(&self, args: Vec<Value>) {
        let mut pending = self.pending.lock().unwrap();
        let mut scan_from = pending.len();
        parse_redraw(args, &mut pending);
        for event in &pending[scan_from..] {
            if let RedrawEvent::Restart(addr) = event {
                log::info!(":restart: new server at {addr}");
                *self.restart.lock().unwrap() = Some(addr.clone());
            }
        }

        while let Some(offset) = pending[scan_from..]
            .iter()
            .position(|e| *e == RedrawEvent::Flush)
        {
            let end = scan_from + offset + 1;
            // eprintln!("batch {} events, pending {}", end, pending.len());
            // usually flush is the last event, so just take the whole Vec
            let batch = if end == pending.len() {
                std::mem::take(&mut *pending)
            } else {
                pending.drain(..end).collect()
            };
            self.first_flush.call_once(|| {
                log::info!("first flush {:?} after startup", self.started.elapsed());
            });
            if self
                .proxy
                .send_event(UserEvent::Redraw(batch, Instant::now()))
                .is_err()
            {
                // window is gone
                return;
            }
            scan_from = 0;
        }
    }
}

#[async_trait]
impl Handler for RedrawHandler {
    type Writer = Writer;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Neovim<Writer>) {
        match name.as_str() {
            "redraw" => self.on_redraw(args),
            "right_editor_md" => match crate::preview::parse_notification(args) {
                Some(event) => {
                    let _ = self.proxy.send_event(UserEvent::Preview(event));
                }
                None => log::debug!("preview: unrecognized notification"),
            },
            // errors from requests we sent as notifications (input goes this way)
            "nvim_error_event" => log::error!("nvim: {args:?}"),
            _ => log::debug!("ignored notification: {name} {args:?}"),
        }
    }
}
