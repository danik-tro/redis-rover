use std::sync::{Arc, Mutex};

use ratatui::{prelude::*, widgets::*};
use tokio::sync::mpsc::UnboundedSender;

use crate::{action::Action, redis_client::types::RedisInfo};

pub struct Summary {
    tx: UnboundedSender<Action>,
    info: Arc<Mutex<Option<RedisInfo>>>,
}

impl Summary {
    pub fn new(info: Arc<Mutex<Option<RedisInfo>>>, tx: UnboundedSender<Action>) -> Self {
        Self { info, tx }
    }

    pub fn info(&self) -> Arc<Mutex<Option<RedisInfo>>> {
        self.info.clone()
    }
}

pub struct SummaryWidget;

impl StatefulWidget for SummaryWidget {
    type State = Summary;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let common_info = {
            state
                .info
                .lock()
                .unwrap()
                .as_ref()
                .map(|info| info.common.clone())
        };

        let text = if let Some(info) = common_info {
            format!(
                "Version: {}. User memory: {}. Total memory: {}. OS: {}",
                info.version, info.memory, info.total_memory, info.os,
            )
        } else {
            "Version: -. User memory: -. Total memory: -. OS: -".into()
        };

        Paragraph::new(text)
            .block(
                Block::bordered()
                    .title("Summary")
                    .title_alignment(Alignment::Left),
            )
            .render(area, buf);
    }
}
