use caduceus::config::Config;
use caduceus::github::Client;

fn cfg(token: Option<&str>) -> Config {
    let mut c = Config::test_defaults(std::path::Path::new("/tmp"));
    c.github_token = token.map(|s| s.to_string());
    c
}

#[test]
fn with_config_resolves_from_env_var_when_config_is_none() {
    let c = cfg(None);
    temp_env::with_var(
        "CADUCEUS_GITHUB_TOKEN",
        Some("ghp_test_daemon_env_var_token"),
        || {
            let client = Client::with_config(&c).expect("client builds");
            assert_eq!(client.token(), Some("ghp_test_daemon_env_var_token"));
        },
    );
}

#[test]
fn with_config_falls_back_to_none_when_env_var_dropped() {
    let c = cfg(None);
    let client = Client::with_config(&c).expect("client builds");
    assert_eq!(client.token(), None);
}

#[test]
fn with_config_uses_explicit_config_when_set() {
    let c = cfg(Some("ghp_explicit_config"));
    let client = Client::with_config(&c).expect("client builds");
    assert_eq!(client.token(), Some("ghp_explicit_config"));
}

#[test]
fn with_cache_resolves_from_env_var_when_config_is_none() {
    let state_dir = std::path::Path::new("/tmp");
    let mut c = Config::test_defaults(state_dir);
    let cache = caduceus::github::HttpCache::open(state_dir).expect("cache opens");
    temp_env::with_var(
        "CADUCEUS_GITHUB_TOKEN",
        Some("ghp_test_daemon_cache_token"),
        || {
            let client = Client::with_cache(&c, cache).expect("client builds");
            assert_eq!(client.token(), Some("ghp_test_daemon_cache_token"));
        },
    );
}
