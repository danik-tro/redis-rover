use std::sync::{Arc, Mutex};

use ratatui::{prelude::*, widgets::*};

use crate::redis_client::types::RedisInfo;

pub struct Info {
    info: Arc<Mutex<Option<RedisInfo>>>,
}

impl Info {
    pub fn new(info: Arc<Mutex<Option<RedisInfo>>>) -> Self {
        Self { info }
    }

    pub fn info(&self) -> Arc<Mutex<Option<RedisInfo>>> {
        self.info.clone()
    }
}

pub struct InfoWidget;

impl StatefulWidget for InfoWidget {
    type State = Info;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        Block::new()
            .fg("#62d6e8".parse::<Color>().unwrap())
            .bg("#282936".parse().unwrap())
            .render(area, buf);

        let common_info = { state.info.lock().unwrap().as_ref().map(Clone::clone) };

        let [top, bottom] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)])
            .flex(layout::Flex::Center)
            .areas(area);

        let [version_area, _, cpu_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Fill(1), Constraint::Min(0)])
                .flex(layout::Flex::Center)
                .areas(top);

        let [os_area, clients_area, memory_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Fill(1), Constraint::Min(0)])
                .flex(layout::Flex::Center)
                .areas(bottom);

        // TODO: handle failed case
        let (os_info, memory, redis_info, cpu, clients) = if let Some(info) = common_info {
            (
                info.os(),
                info.memory(),
                info.redis_version(),
                info.cpu(),
                info.clients(),
            )
        } else {
            ("-".into(), "-".into(), "-".into(), "-".into(), "-".into())
        };

        Paragraph::new(os_info)
            .alignment(Alignment::Left)
            .render(os_area, buf);
        Paragraph::new(redis_info)
            .alignment(Alignment::Left)
            .render(version_area, buf);

        Paragraph::new(memory)
            .alignment(Alignment::Right)
            .render(memory_area, buf);

        Paragraph::new(cpu)
            .alignment(Alignment::Right)
            .render(cpu_area, buf);

        Paragraph::new(clients)
            .alignment(Alignment::Center)
            .render(clients_area, buf);
    }
}
