//! Lazy Prometheus metrics registry (REQ-OBS-006/007, design D2).
//! Implemented via strict TDD: the module's enable/disable + render tests
//! land here first (RED), then the lazy recorder.
