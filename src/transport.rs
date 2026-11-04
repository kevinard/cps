use std::time::Duration;

use reqwest::{Client, ClientBuilder, redirect::Policy};

pub(crate) fn https_client() -> ClientBuilder {
    Client::builder()
        .https_only(true)
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
}
