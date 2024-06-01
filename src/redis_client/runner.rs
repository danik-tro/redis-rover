use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use color_eyre::eyre::Result;
use redis::aio::ConnectionManager;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{client::redis_info, types::RedisInfo};

pub struct Runner {
    cancelation_token: CancellationToken,
    manager: ConnectionManager,
    info: Arc<Mutex<Option<RedisInfo>>>,

    info_task: JoinHandle<()>,
}

impl Runner {
    pub fn new(manager: ConnectionManager, info: Arc<Mutex<Option<RedisInfo>>>) -> Self {
        let info_task = tokio::spawn(async {});
        let cancelation_token = CancellationToken::new();

        Self {
            manager,
            info,
            info_task,
            cancelation_token,
        }
    }

    pub fn cancelation_token(mut self, token: CancellationToken) -> Self {
        self.cancelation_token = token;
        self
    }

    pub fn start(&mut self) {
        let tick: Duration = std::time::Duration::from_secs_f64(2.0);
        let cancelation_token = self.cancelation_token.clone();
        let manager = self.manager.clone();
        let info = self.info.clone();

        self.info_task = tokio::spawn(async move {
            let mut refresh_interval = tokio::time::interval(tick);
            let mut manager = manager;

            loop {
                tokio::select! {
                    _ = refresh_interval.tick() => {
                        let info_res = redis_info(&mut manager).await;

                        match info_res {
                            Ok(redis_info) => *info.lock().unwrap() = Some(redis_info),
                            Err(_err) => {
                                // TODO: show the popup
                            },
                        }
                    },
                    _ = cancelation_token.cancelled() => {
                        break;
                    }
                }
            }
        });
    }

    pub fn stop(&self) -> Result<()> {
        self.cancel();

        let mut counter = 0;
        while !self.info_task.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
            counter += 1;
            if counter > 50 {
                self.info_task.abort();
            }
            if counter > 100 {
                log::error!("Failed to abort task in 100 milliseconds for unknown reason");
                break;
            }
        }
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancelation_token.cancel();
    }
}
