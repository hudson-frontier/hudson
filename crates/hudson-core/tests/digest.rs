use hudson_core::definitions::digest;
use serde_json::json;

#[test]
fn digest_preserves_persisted_lowercase_hex_including_leading_zeroes() {
    for (value, expected) in [
        (
            json!({"a": 1}),
            "015abd7f5cc57a2dd94b7590f04ad8084273905ee33ec5cebeae62276a97f862",
        ),
        (
            json!(null),
            "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b",
        ),
        (
            json!(""),
            "12ae32cb1ec02d01eda3581b127c1fee3b0dc53572ed6baf239721a03d82e126",
        ),
    ] {
        assert_eq!(digest(&value).unwrap(), expected);
    }
    assert_eq!(
        digest(&json!({"a": 1, "b": 2})).unwrap(),
        digest(&json!({"b": 2, "a": 1})).unwrap()
    );
}
