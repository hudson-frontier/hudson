use hudson_core::storage::Store;
use postgres::{Client, NoTls};
use std::sync::{Arc, Barrier};

#[test]
#[ignore = "requires dedicated PostgreSQL database on /tmp"]
fn concurrent_hosts_create_an_empty_schema_without_catalog_races() {
    let database = std::env::var("HUDSON_TEST_DATABASE").expect("dedicated test database");
    let schema = format!("hudson_bootstrap_{}", uuid::Uuid::new_v4().simple());
    let connect = || {
        postgres::Config::new()
            .host("/tmp")
            .dbname(&database)
            .connect(NoTls)
            .unwrap()
    };
    let mut owner: Client = connect();
    owner
        .batch_execute(&format!("CREATE SCHEMA \"{schema}\""))
        .unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let mut peers = Vec::new();
    for index in 0..4 {
        let mut client = connect();
        client
            .batch_execute(&format!("SET search_path TO \"{schema}\""))
            .unwrap();
        let barrier = barrier.clone();
        peers.push(std::thread::spawn(move || {
            barrier.wait();
            Store::from_postgres(client, &format!("host-{index}"))
        }));
    }
    for peer in peers {
        peer.join().unwrap().unwrap();
    }
    let count: i64 = owner
        .query_one(
            &format!("SELECT count(*) FROM \"{schema}\".hudson_state"),
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 4);
    owner
        .batch_execute(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .unwrap();
}
