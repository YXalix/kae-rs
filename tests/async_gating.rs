//! Gating tests for the async batch path: a missing patched library or
//! missing async symbols must produce explicit errors, never a silent
//! fallback.

#[test]
fn async_batch_rejects_stock_liblz4() {
    // The system liblz4 has no async symbols: open must fail with a Symbol
    // error (a library whose synchronous ABI works is not async-capable —
    // AsyncBatch is an explicit hardware choice)
    match kae::AsyncBatch::open_path("liblz4.so.1", &[]) {
        Err(kae::KaeError::Symbol(e)) => {
            assert!(
                e.contains("async"),
                "the symbol error should point at an async symbol: {e}"
            );
        }
        Err(e) => panic!("expected a Symbol error, got {e}"),
        Ok(_) => panic!("the system liblz4 must not open (no async symbols)"),
    }
}

#[test]
fn async_batch_open_hw_never_falls_back() {
    if !kae::hw_available() {
        match kae::AsyncBatch::open() {
            Err(kae::KaeError::Unavailable) => {}
            Err(e) => panic!("without hardware, open() must report Unavailable, got {e}"),
            Ok(_) => panic!("without hardware, open() must not succeed"),
        }
    }
}

#[test]
fn configure_sets_tuning_env_vars() {
    // No open in this binary reaches engine init (the stock-liblz4 open
    // fails at symbol load; the hardware open lives in tests/hw_required.rs),
    // so configure stays eligible here
    kae::AsyncBatch::configure(Some(7), Some(2)).expect("configure before any open");
    assert_eq!(std::env::var("KAE_LZ4_ASYNC_THREAD_NUM").unwrap(), "7");
    assert_eq!(std::env::var("KAE_LZ4_LEVEL").unwrap(), "2");
}
