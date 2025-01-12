use std::{
    net::{Ipv6Addr, SocketAddr},
    time::Duration,
};

use ffs::network::{client::broadcast_from_path, server::receive};

#[tokio::test]
async fn send_and_receive_ipv6() {
    // Client
    let client_handle = {
        let resources_dir = std::env::current_dir().unwrap().join("tests/resources");
        let addr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
        let grace_period = Duration::from_secs(1);

        tokio::spawn(async move { broadcast_from_path(addr, &resources_dir, grace_period).await })
    };

    // Server
    {
        let addr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
        let name = "receiver";
        // this task never completes
        tokio::spawn(receive(name, addr, true));
    }

    client_handle.await.unwrap().unwrap();
}
