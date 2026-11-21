//! Cluster tests against the six-node fixture in `tests/docker`. Every test
//! is `#[ignore]`d (they need Docker) and uses the fixed ports 7100-7105:
//!
//! ```bash
//! cargo test --test cluster -- --ignored --test-threads=1
//! ```

mod cluster_support;

use cluster_support::ClusterFixture;
use redis::AsyncCommands;
use redis_rover::internals::{
    connect_cluster, KeyspaceState, Link, MasterNode, Storage, SweepApply,
};

/// The sweep walks every master and finds each key exactly once.
#[tokio::test]
#[ignore = "needs Docker"]
async fn sweep_covers_every_master() {
    let fixture = ClusterFixture::start();
    let client = fixture.client();
    let mut seed = client.get_async_connection().await.unwrap();
    for i in 0..3000 {
        let () = seed.set(format!("sweep:{i}"), i).await.unwrap();
    }

    let storage = Storage::new(connect_cluster(&client, Link::Bulk).await.unwrap());
    let masters = storage.masters().await.unwrap();
    assert_eq!(masters.len(), 3, "{masters:?}");
    let slots: Vec<u16> = masters.iter().filter_map(MasterNode::route_slot).collect();

    let mut state = KeyspaceState::default();
    state.set_sweep_nodes(&slots);
    while let Some(request) = state.load_request() {
        let result = storage.scan_until(&request).await.unwrap();
        assert_eq!(state.apply_result(&request, result), SweepApply::Applied);
    }
    assert_eq!(state.scanned.len(), 3000);
}
