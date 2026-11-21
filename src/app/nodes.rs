//! Which cluster node the Slowlog and MONITOR screens look at.

use super::App;
use crate::{
    mode::Mode,
    redis_client::monitor::MonitorCmd,
    widgets::node_picker::{NodeStrip, MONITOR_NODE_WARNING},
};

impl App {
    /// The masters' addresses, in slot order. Empty on one server.
    fn node_addrs(&self) -> Vec<(String, u16)> {
        self.state
            .masters
            .lock()
            .iter()
            .map(|master| (master.host.clone(), master.port))
            .collect()
    }

    /// Index of the picked node; the first one until a node is picked.
    fn picked_node(&self, addrs: &[(String, u16)]) -> usize {
        let picked = self.state.ops_node.lock();
        picked
            .as_ref()
            .and_then(|node| addrs.iter().position(|addr| addr == node))
            .unwrap_or(0)
    }

    /// The row above the Slowlog or MONITOR screen; `None` on one server.
    pub(super) fn node_strip(&self, mode: Mode) -> Option<NodeStrip> {
        if !matches!(mode, Mode::Slowlog | Mode::Monitor) {
            return None;
        }
        let addrs = self.node_addrs();
        if addrs.is_empty() {
            return None;
        }
        let selected = self.picked_node(&addrs);
        Some(NodeStrip {
            labels: addrs
                .iter()
                .map(|(host, port)| format!("{host}:{port}"))
                .collect(),
            selected,
            warning: (mode == Mode::Monitor).then_some(MONITOR_NODE_WARNING),
        })
    }

    /// The picked node as `host:port`, for a preview.
    pub(super) fn picked_node_label(&self) -> Option<String> {
        let addrs = self.node_addrs();
        let (host, port) = addrs.get(self.picked_node(&addrs))?;
        Some(format!("{host}:{port}"))
    }

    /// Make the node on screen the one the runner and MONITOR task use.
    pub(super) fn pin_picked_node(&self) {
        let addrs = self.node_addrs();
        if let Some(addr) = addrs.get(self.picked_node(&addrs)) {
            *self.state.ops_node.lock() = Some(addr.clone());
        }
    }

    /// `Tab` on the Slowlog or MONITOR screen: the next (or previous) node.
    pub(super) fn step_ops_node(&mut self, forward: bool) {
        let addrs = self.node_addrs();
        if addrs.len() < 2 {
            return;
        }
        let at = self.picked_node(&addrs);
        let next = if forward {
            (at + 1) % addrs.len()
        } else {
            (at + addrs.len() - 1) % addrs.len()
        };
        *self.state.ops_node.lock() = Some(addrs[next].clone());
        self.ops_node_changed();
    }

    /// A failover took the picked node away: go to the first master.
    /// Returns whether it did.
    pub(super) fn sync_ops_node(&mut self) -> bool {
        let addrs = self.node_addrs();
        let gone = {
            let picked = self.state.ops_node.lock();
            !addrs.is_empty() && picked.as_ref().is_some_and(|node| !addrs.contains(node))
        };
        if gone {
            *self.state.ops_node.lock() = Some(addrs[0].clone());
            self.ops_node_changed();
        }
        gone
    }

    fn ops_node_changed(&mut self) {
        match self.view_mode() {
            Mode::Slowlog => self.fetch_slowlog(),
            Mode::Monitor if self.monitor_active() => {
                // One log never mixes the lines of two nodes.
                self.send_monitor(MonitorCmd::Stop);
                self.monitor.clear();
                self.send_monitor(MonitorCmd::Start);
            }
            Mode::Monitor => self.monitor.clear(),
            _ => {}
        }
    }
}
