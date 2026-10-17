---
name: tdd-workflow
description: Test-Driven Development workflow requiring failing unit tests first
triggers: [test, tdd, assert, coverage]
---
Always write or update a `#[test]` function before modifying implementation code, and verify with `cargo test`.
