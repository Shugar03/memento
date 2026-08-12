//! Gated process sampler (REQ-OBS-011, design D6).
//! Implemented via strict TDD: the module's probe + sample_now + fake-clock
//! tests land here first (RED), then the sampler.
