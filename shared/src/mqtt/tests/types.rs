//! Tests of the data types of a connection.

use alloc::format;

use super::{PASSWORD, connect};

#[test]
fn debug_does_not_show_the_password() {
    let connect = connect();
    let credentials = connect.credentials.clone().expect("credentials");
    for text in [format!("{credentials:?}"), format!("{connect:?}")] {
        assert!(!text.contains(PASSWORD), "{text}");
        assert!(text.contains("dashboard"), "{text}");
    }
}
