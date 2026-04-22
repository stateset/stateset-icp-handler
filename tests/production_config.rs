use stateset_icp_handler::config::Config;

#[test]
fn production_profile_rejects_dev_defaults() {
    let mut cfg = Config::for_test();
    cfg.deployment_env = "production".into();
    let err = cfg.validate_runtime().unwrap_err().to_string();
    assert!(err.contains("ICP_SIGNING_KEY_PEM is required"));
    assert!(err.contains("ICP_ALLOW_INSECURE_URLS must be false"));
    assert!(err.contains("ICP_REQUIRE_REQUEST_ID must be true"));
    assert!(err.contains("ICP_PAYMENT_EXECUTION_MODE must be external_required"));
}

#[test]
fn invalid_payment_mode_is_rejected() {
    let mut cfg = Config::for_test();
    cfg.payment_execution_mode = "fake_mode".into();
    let err = cfg.validate_runtime().unwrap_err().to_string();
    assert!(err.contains("ICP_PAYMENT_EXECUTION_MODE"));
}

#[test]
fn disabling_insecure_urls_requires_https_urls() {
    let mut cfg = Config::for_test();
    cfg.allow_insecure_urls = false;
    cfg.public_base_url = "http://127.0.0.1:8082".into();
    let err = cfg.validate_runtime().unwrap_err().to_string();
    assert!(err.contains("ICP_PUBLIC_BASE_URL must use https://"));
}
