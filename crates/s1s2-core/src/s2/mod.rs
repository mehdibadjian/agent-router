//! System 2: slow, deliberative, compilative subsystems (spec §3.3).

pub mod handoff;
pub mod xml_compiler;

#[cfg(feature = "llm")]
pub mod llm;
