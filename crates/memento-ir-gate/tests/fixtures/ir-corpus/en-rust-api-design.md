# Rust API design: traits, errors and ergonomics

This document collects practical rules for designing a Rust library
API that is pleasant to use. Good Rust APIs are discovered by reading
the type signature alone. If a caller needs the documentation to
understand what a function does, the API is probably too clever.

## Design the traits first

The first step of a Rust library is naming the core traits. A trait
describes a capability, not an implementation. Keep each trait small:
one capability, one set of methods, one clear contract. Large traits
force implementors to write methods they do not care about.

When a trait is used by many types, provide blanket implementations
where it is safe. Blanket implementations turn small pieces of shared
behavior into reusable building blocks.

## Errors as data

Errors in Rust are values, not strings. Define a dedicated error enum
for the crate and derive `thiserror::Error` for it. The enum carries
the context the caller needs to recover: the failed operation, the
input that caused the failure, and the underlying cause when it helps.

Do not use `String` as the error type. A string error cannot be
matched programmatically, and it loses the structured information that
the caller needs to handle the failure. Prefer a small enum with a
`#[error("...")]` attribute on every variant.

## Return types that guide the caller

Prefer `Result<T, Error>` over panics for anything that can fail at
runtime. Panics are for programming errors, not for bad input. A
library that panics on user input forces every caller to defensively
validate before every call, which duplicates logic and leaks the
library's internals into the caller's code.

Use `Option<T>` when absence is a normal outcome, and `Result<T, E>`
when failure carries context. The distinction tells the caller how to
handle the return value at a glance.

## Builder patterns for configuration

When a type has many configuration knobs, expose a builder. The
builder starts from a `Default` value and each setter returns `Self`,
allowing method chaining. The final `build()` method validates the
configuration and returns `Result<Configured, ConfigError>`.

Builders keep the public API small while remaining explicit: the
caller sees every knob they are setting and never relies on
argument-order tricks.

## Conclusion

A well-designed Rust API reads like a specification. Traits describe
capabilities, errors describe failures as data, return types describe
the possible outcomes, and builders describe configuration without
arguments sprawl. Design those four surfaces carefully and the rest of
the crate follows.
